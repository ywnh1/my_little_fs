//! 文件系统的门面：工厂 [`FsBuilder`] 与实体 [`Fs`]。
//!
//! 一个 `Fs` 由三部分组成：
//!
//! - `db`：redb 数据库，记录「逻辑文件 → 块列表」的历史版本（见 [`crate::db`]）
//! - `data_path`：块实体数据的存放目录，布局见 [`crate::chunk`]
//! - `cdc_opt` / `compress`：分块粒度与压缩策略，只在**写入**时生效
//!
//! 写入流程是「分块 → 哈希 → 查重 → （压缩）→ 落盘 → 记元数据」，
//! 见 [`Fs::file_processing`]。

use fastcdc::v2020::StreamCDC;
use redb::Database;
use std::{
    fs,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::{chunk::Chunk, db::hash2path};

/// 为 `FsBuilder` 批量生成链式 setter 的辅助宏。
///
/// `字段名 => 类型` 会生成 `with_字段名(self, 值: 类型) -> Self`。
/// 由于字段本身多为 `Option<T>`，调用时需要显式包一层 `Some(..)`，
/// 例如 `.with_max_size(Some(4096))`。
macro_rules! with_any {
    ($($fn_name:tt => $field:tt => $type:ty),*$(,)?) => {
        $(
        #[inline]
        #[must_use]
        pub fn $fn_name(mut self,$field: $type) -> Self {
            self.$field = $field;
            self
        }
        )*
    };
}

/// 创建 [`Fs`] 的工厂。
///
/// 未指定的路径会按 `root_path` 推导：数据库为 `<root>/db.redb`，
/// 数据目录为 `<root>/data`。
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub struct FsBuilder {
    /// CDC 分片的最大大小（字节）
    pub max_size: Option<usize>,
    /// CDC 分片的最小大小（字节）
    pub min_size: Option<usize>,
    /// CDC 分片的平均大小（字节）
    pub avg_size: Option<usize>,
    /// 压缩设置：`Some(level)` 启用 zstd（level 越小越快），`None` 不压缩
    pub compress: Option<i32>,
    /// 数据库文件位置，默认 `<root>/db.redb`
    pub db_path: Option<PathBuf>,
    /// 块数据的存储位置，默认 `<root>/data`
    pub data_path: Option<PathBuf>,
    /// `Fs` 的根目录；单独指定的 `db_path` / `data_path` 会覆盖由它推导出的默认值
    pub root_path: PathBuf,
}

/// 已归一化的一组分块参数，保证 `min_size <= avg_size <= max_size`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub struct CdcOptions {
    pub max_size: usize,
    pub min_size: usize,
    pub avg_size: usize,
}

impl From<&FsBuilder> for CdcOptions {
    /// 把「用户可能只填了一部分」的三个尺寸补全成合法的一组。
    ///
    /// 规则：给定一个就按 1:4:16 推其余两个；给定两个就推剩下的那一个；
    /// 三个都没给就用 fastcdc 官方文档的 16 KiB / 32 KiB / 64 KiB。
    /// 最后统一排序，避免出现 `max < min` 这种自相矛盾的配置。
    fn from(value: &FsBuilder) -> Self {
        let (min_size, avg_size, max_size) = match (value.min_size, value.avg_size, value.max_size)
        {
            (None, None, None) => (16384, 32768, 65536), // fastcdc 官方推荐值
            (Some(min), Some(avg), Some(max)) => (min, avg, max),
            (Some(min), None, None) => (min, min * 4, min * 16),
            (None, Some(avg), None) => (avg / 4, avg, avg * 4),
            (None, None, Some(max)) => (max / 16, max / 4, max),
            (Some(min), Some(avg), None) => (min, avg, avg * 4),
            (None, Some(avg), Some(max)) => (avg / 4, avg, max),
            (Some(min), None, Some(max)) => {
                // 两端夹逼出中间的 avg：min 的 4 倍与 max 的 1/4 是「可能的最大 / 最小」
                // 平均值；两者没有交叠（min 相对 max 太大）时就取算术中点。
                let four_min = min * 4;
                let quarter_max = max / 4;
                let avg = if four_min < max {
                    (four_min + quarter_max) / 2
                } else {
                    (min + max) / 2
                };
                (min, avg, max)
            }
        };

        // 把三个值排好序：用户传入的顺序可能任意（例如 max < min）
        let mut sizes = [min_size, avg_size, max_size];
        sizes.sort_unstable();

        Self {
            min_size: sizes[0],
            avg_size: sizes[1],
            max_size: sizes[2],
        }
    }
}

impl FsBuilder {
    /// 以 `root_path` 为根创建工厂，其余字段用默认值。
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            ..Default::default()
        }
    }

    /// 消耗工厂，打开（不存在则创建）数据库并产出 [`Fs`]。
    ///
    /// 注意：这里**不会**创建数据目录 `data_path`，它由第一次写入块时按需创建。
    /// 因此 `build()` 成功只代表数据库可用。
    ///
    /// 同一个根目录同时只能有一个 `Fs`：redb 会对数据库文件加锁，
    /// 重复打开会返回 `DatabaseAlreadyOpen`。要并发访问请先 `drop` 掉前一个实例。
    pub fn build(mut self) -> Result<Fs, redb::Error> {
        let db_path = if let Some(p) = self.db_path.take() {
            p
        } else {
            self.root_path.join("db.redb")
        };
        let data_path = if let Some(p) = self.data_path.take() {
            p
        } else {
            self.root_path.join("data")
        };
        let db = Database::create(db_path)?;
        let cdc_opt = CdcOptions::from(&self);
        Ok(Fs {
            db,
            cdc_opt,
            data_path,
            compress: self.compress,
        })
    }

    with_any! {
        with_compress => compress => Option<i32>,
        with_db_path => db_path => Option<PathBuf>,
        with_data_path => data_path => Option<PathBuf>,
        with_root_path => root_path => PathBuf,
        with_max_size => max_size => Option<usize>,
        with_min_size => min_size => Option<usize>,
        with_avg_size => avg_size => Option<usize>,
    }

    /// 一次性设置三个分块尺寸，等价于分别调用三个 `with_*_size`。
    pub fn with_cdc_config(mut self, options: CdcOptions) -> Self {
        self.max_size = Some(options.max_size);
        self.min_size = Some(options.min_size);
        self.avg_size = Some(options.avg_size);
        self
    }
}

/// 一个内容寻址文件系统。
///
/// 所有写操作都是「先写块数据，再提交数据库」，读操作见 [`crate::db`] 与
/// [`crate::io`] 的各个 trait。
pub struct Fs {
    pub(crate) db: Database,
    pub(crate) cdc_opt: CdcOptions,
    pub(crate) data_path: PathBuf,
    pub(crate) compress: Option<i32>,
}

impl Fs {
    /// 返回一个以 `root_path` 为根的 [`FsBuilder`]。
    pub fn builder(root_path: PathBuf) -> FsBuilder {
        FsBuilder::new(root_path)
    }

    /// 把 `path` 指向的真实文件切块并落盘，返回它的块列表（文件内容本身不入库）。
    ///
    /// 每个块的流程：
    /// 1. 按内容定义分块（CDC），得到原始字节与它在文件中的偏移
    /// 2. 对原始字节求 blake3，作为内容地址
    /// 3. 若该地址的文件已存在则跳过（内容寻址 ⇒ 同 hash 必同内容，天然去重）
    /// 4. 否则按需压缩并写入 `<data_path>/<hash 前缀>/<hash 其余>`
    ///
    /// 落盘前会按需创建目录，因此全新的 `Fs` 上第一次调用也能成功。
    pub(crate) fn file_processing(&self, path: &Path) -> io::Result<Vec<Chunk>> {
        let mut chunks = Vec::new();
        let mut file = File::open(path)?;
        let cdc = StreamCDC::new(
            &mut file,
            self.cdc_opt.min_size,
            self.cdc_opt.avg_size,
            self.cdc_opt.max_size,
        );
        for piece in cdc {
            // 1. 分块：拿到原始数据、长度与它在大文件中的偏移
            let piece = piece?;
            // 2. 哈希：原始内容决定地址，压缩与否不影响块的身份
            let hash = blake3::hash(&piece.data);
            let mut data = piece.data;
            let dest = self.data_path.join(hash2path(&hash));
            if !dest.exists() {
                // 3. 压缩（可选）
                if let Some(level) = self.compress {
                    let compressed = zstd::encode_all(data.as_slice(), level)?;
                    // 不可压缩的数据经 zstd 反而会变大，此时保留原始字节。
                    // 两种形态靠 zstd 魔数区分，见 `Chunk::read`。
                    if compressed.len() < data.len() {
                        data = compressed;
                    }
                }
                // 4. 落盘
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                File::create(&dest)?.write_all(&data)?;
                // ponytail: 此处未调用 fsync。块数据可能仍留在页缓存中，
                // 而调用方随后就会提交数据库引用（见 `db::FsDbWrite::insert`），
                // 断电时可能留下指向空文件的记录。逐块 fsync 在手机上代价过高，
                // 现约定由调用方在批量导入结束后自行同步整个目录。
            }
            // 5. 记录元数据；`size` 记的是**原始**长度
            chunks.push(Chunk {
                hash,
                size: piece.length,
                offset: piece.offset,
            });
        }

        // 空文件切不出任何块。但数据库里「一个版本」就是「某键下的若干值」，
        // 一个值都没有等于这个版本不存在 —— 于是空文件会连同它的 id 一起消失。
        // 这里补一个长度为 0 的哨兵块，让「空文件」和「没有这个文件」能区分开。
        if chunks.is_empty() {
            let hash = blake3::hash(b"");
            let dest = self.data_path.join(hash2path(&hash));
            if !dest.exists() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                File::create(&dest)?.write_all(&[])?;
            }
            chunks.push(Chunk {
                hash,
                size: 0,
                offset: 0,
            });
        }
        Ok(chunks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言三个尺寸已经归一化：非零、且 min <= avg <= max
    fn assert_normalized(opt: CdcOptions) {
        assert!(opt.min_size > 0, "{opt:?} 不应出现 0 长度分块");
        assert!(
            opt.min_size <= opt.avg_size && opt.avg_size <= opt.max_size,
            "{opt:?} 未归一化"
        );
    }

    #[test]
    fn defaults_are_the_fastcdc_recommended_sizes() {
        let opt = CdcOptions::from(&FsBuilder::new(PathBuf::from("/tmp")));
        assert_eq!(opt.min_size, 16384);
        assert_eq!(opt.avg_size, 32768);
        assert_eq!(opt.max_size, 65536);
    }

    #[test]
    fn giving_only_one_size_derives_the_others_by_four_and_sixteen() {
        let base = FsBuilder::new(PathBuf::from("/tmp"));

        let opt = CdcOptions::from(&base.clone().with_min_size(Some(1000)));
        assert_eq!(
            (opt.min_size, opt.avg_size, opt.max_size),
            (1000, 4000, 16000)
        );

        let opt = CdcOptions::from(&base.clone().with_avg_size(Some(4000)));
        assert_eq!(
            (opt.min_size, opt.avg_size, opt.max_size),
            (1000, 4000, 16000)
        );

        let opt = CdcOptions::from(&base.clone().with_max_size(Some(16000)));
        assert_eq!(
            (opt.min_size, opt.avg_size, opt.max_size),
            (1000, 4000, 16000)
        );
    }

    #[test]
    fn giving_two_sizes_keeps_them_and_derives_the_third() {
        let base = FsBuilder::new(PathBuf::from("/tmp"));

        let opt = CdcOptions::from(
            &base
                .clone()
                .with_min_size(Some(1000))
                .with_avg_size(Some(4000)),
        );
        assert_eq!(
            (opt.min_size, opt.avg_size, opt.max_size),
            (1000, 4000, 16000)
        );

        let opt = CdcOptions::from(
            &base
                .clone()
                .with_avg_size(Some(4000))
                .with_max_size(Some(16000)),
        );
        assert_eq!(
            (opt.min_size, opt.avg_size, opt.max_size),
            (1000, 4000, 16000)
        );
    }

    #[test]
    fn min_and_max_without_avg_lands_between_them() {
        for (min, max) in [(100usize, 1600usize), (100, 1000), (1000, 1050), (7, 7)] {
            let builder = FsBuilder::new(PathBuf::from("/tmp"))
                .with_min_size(Some(min))
                .with_max_size(Some(max));
            let opt = CdcOptions::from(&builder);
            assert_eq!(opt.min_size, min);
            assert_eq!(opt.max_size, max.max(min));
            assert!(
                opt.min_size <= opt.avg_size && opt.avg_size <= opt.max_size,
                "min={min} max={max} 推出的 {opt:?} 不合法"
            );
        }
    }

    #[test]
    fn contradictory_sizes_are_sorted_instead_of_rejected() {
        // 用户把 max 填得比 min 还小：结果应当被排序成合法的一组
        let builder = FsBuilder::new(PathBuf::from("/tmp"))
            .with_min_size(Some(9000))
            .with_avg_size(Some(3000))
            .with_max_size(Some(1000));
        let opt = CdcOptions::from(&builder);
        assert_eq!(
            (opt.min_size, opt.avg_size, opt.max_size),
            (1000, 3000, 9000)
        );
        assert_normalized(opt);
    }

    #[test]
    fn with_cdc_config_roundtrips_through_the_builder() {
        let wanted = CdcOptions {
            min_size: 1024,
            avg_size: 4096,
            max_size: 16384,
        };
        let builder = FsBuilder::new(PathBuf::from("/tmp")).with_cdc_config(wanted);
        assert_eq!(CdcOptions::from(&builder), wanted);
    }

    #[test]
    fn builder_paths_default_to_subdirectories_of_the_root() {
        let root = PathBuf::from("/tmp/some-root");
        assert_eq!(FsBuilder::new(root.clone()).root_path, root);
        // 未显式指定时由 build 推导，这里只验证推导规则本身
        let builder = FsBuilder::new(root.clone());
        assert_eq!(builder.db_path, None);
        assert_eq!(builder.data_path, None);
        assert_eq!(builder.root_path.join("db.redb"), root.join("db.redb"));
        assert_eq!(builder.root_path.join("data"), root.join("data"));
    }
}
