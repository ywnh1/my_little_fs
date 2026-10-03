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
//!
//! # Feature
//!
//! | feature | 默认 | 作用 |
//! | --- | --- | --- |
//! | `zstd` | 开 | zstd 压缩后端。关掉后 `with_compress` 不再存在，写入端一律存原始字节 |
//! | `tempfile` | 开 | 先写临时文件再 rename 的原子落盘，同时提供 `temp_dir` 配置项 |
//!
//! 两个开关互不影响，可以任意组合；关掉 `tempfile` 后代码里不再出现临时文件逻辑，
//! 关掉 `zstd` 后连压缩 crate 都不会被编译。

use fastcdc::v2020::StreamCDC;
use redb::Database;
use std::{
    fs,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};
#[cfg(feature = "tempfile")]
use tempfile::NamedTempFile;

use crate::{
    chunk::Chunk,
    codec::{self, Compress},
    db::hash2path,
};

/// 为 `FsBuilder` 批量生成链式 setter 的辅助宏。
///
/// `字段名 => 类型` 会生成 `with_字段名(self, 值: 类型) -> Self`。
/// 由于字段本身多为 `Option<T>`，调用时需要显式包一层 `Some(..)`，
/// 例如 `.with_max_size(Some(4096))`。
///
/// 每个条目前面可以带属性，用来把 setter 挂到某个 feature 上。
macro_rules! with_any {
    ($($(#[$meta:meta])* $fn_name:ident => $field:ident => $type:ty),*$(,)?) => {
        $(
        $(#[$meta])*
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
/// 数据目录为 `<root>/data`，临时目录为 `<root>/.tmp`。
///
/// 除了 `root_path` 必须存在，其他的可以通过 `with_<field>`
/// 设定（`Some(x)`）或取消（`None`）使用默认值
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub struct FsBuilder {
    /// CDC 分片的最大大小（字节）
    pub max_size: Option<usize>,
    /// CDC 分片的最小大小（字节）
    pub min_size: Option<usize>,
    /// CDC 分片的平均大小（字节）
    pub avg_size: Option<usize>,
    /// 压缩设置：`Some(..)` 启用压缩（后端与级别见 [`Compress`]），`None` 不压缩。
    /// 只有在启用任意压缩后端 feature 时才起作用。
    pub compress: Option<Compress>,
    /// 数据库文件位置，默认 `<root>/db.redb`
    pub db_path: Option<PathBuf>,
    /// 块数据的存储位置，默认 `<root>/data`
    pub data_path: Option<PathBuf>,
    /// 临时文件目录，默认 `<root>/.tmp`。
    /// 只有在启用 `tempfile` feature 时才起作用，而且**仅当它与目标文件位于同一个
    /// 文件系统上**才会被采用 —— 否则会退回目标的父目录，见 [`staging_dir`]。
    pub temp_dir: Option<PathBuf>,
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
    /// 以 `root_path` 为根创建工厂，其余字段用默认值 `None`。
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            ..Default::default()
        }
    }

    /// 消耗工厂，打开（不存在则创建）数据库并产出 [`Fs`]。
    ///
    /// 注意：这里**不会**创建数据目录与临时目录，它们都由第一次写入时按需创建。
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
        let temp_dir = if let Some(p) = self.temp_dir.take() {
            p
        } else {
            self.root_path.join(".tmp")
        };
        let db = Database::create(db_path)?;
        let cdc_opt = CdcOptions::from(&self);
        Ok(Fs {
            db,
            cdc_opt,
            data_path,
            compress: self.compress,
            temp_dir,
        })
    }

    with_any! {
        /// 设置压缩后端与级别；需要至少启用一个压缩后端 feature，否则这个方法不存在。
        #[cfg(feature = "compress")]
        with_compress => compress => Option<Compress>,
        with_db_path => db_path => Option<PathBuf>,
        with_data_path => data_path => Option<PathBuf>,
        with_root_path => root_path => PathBuf,
        with_max_size => max_size => Option<usize>,
        with_min_size => min_size => Option<usize>,
        with_avg_size => avg_size => Option<usize>,
        /// 设置临时文件目录；需要 `tempfile` feature，否则这个方法不存在。
        #[cfg(feature = "tempfile")]
        with_temp_dir => temp_dir => Option<PathBuf>,
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
    /// 压缩设置；实际生效的后端由启用的 feature 决定。
    pub(crate) compress: Option<Compress>,
    /// 临时文件目录。没启用 `tempfile` feature 时无用武之地。
    #[cfg_attr(not(feature = "tempfile"), allow(dead_code))]
    pub(crate) temp_dir: PathBuf,
}

/// 挑一个与 `dest` 位于**同一个文件系统**的暂存目录。
///
/// 落盘的做法是「写临时文件 → rename 到目标」，而 `rename` 跨文件系统会返回
/// `EXDEV`，所以临时文件必须和目标同设备。配置的 `preferred`（即 `temp_dir`）
/// 若与目标不同设备（例如根目录在 A 分区、数据目录却指到了 B 分区），
/// 这里会自动退回目标的父目录。
///
/// 两个路径中任意一个取不到元数据时保守地判为「不同设备」——
/// 宁可少用一次配置好的目录，也不能让 rename 失败。
#[cfg(feature = "tempfile")]
pub(crate) fn staging_dir(preferred: &Path, dest: &Path) -> PathBuf {
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        // 目标就是个裸文件名（如 "out.bin"）时，父目录按当前目录算
        _ => PathBuf::from("."),
    };
    if same_device(preferred, &parent) {
        preferred.to_path_buf()
    } else {
        parent
    }
}

/// 两个路径是否在同一个文件系统上。
#[cfg(all(feature = "tempfile", unix))]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.metadata(), b.metadata()) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => false,
    }
}

/// 非 unix 平台拿不到设备号，一律判为不同设备，退回目标父目录。
#[cfg(all(feature = "tempfile", not(unix)))]
fn same_device(_a: &Path, _b: &Path) -> bool {
    false
}

impl Fs {
    /// 返回一个以 `root_path` 为根的 [`FsBuilder`]。
    pub fn builder(root_path: PathBuf) -> FsBuilder {
        FsBuilder::new(root_path)
    }

    /// 把 `data` 落到 `dest`，尽量保证「要么是完整内容、要么根本不存在」。
    ///
    /// 启用 `tempfile` feature 时，先在同一个文件系统的暂存目录里写临时文件，
    /// 再 rename 到 `dest`：这样掉电或崩溃不会留下写了一半的块。
    /// 关掉之后退化成直接 `create` + `write`，代码里不再出现临时文件。
    ///
    /// 目录会按需创建，所以全新的 `Fs` 上第一次调用也能成功。
    pub(crate) fn write_blob(&self, dest: &Path, data: &[u8]) -> io::Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        #[cfg(feature = "tempfile")]
        {
            let staging = staging_dir(&self.temp_dir, dest);
            fs::create_dir_all(&staging)?;
            let mut tmp = NamedTempFile::new_in(&staging)?;
            tmp.write_all(data)?;
            // 统一按 io::Error 上报，调用方不必认识 tempfile 的错误类型
            tmp.persist(dest).map_err(|e| e.error)?;
        }

        #[cfg(not(feature = "tempfile"))]
        {
            File::create(dest)?.write_all(data)?;
        }

        Ok(())
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
            // 3. 压缩（可选，取决于 feature 与配置）：
            //    编码结果自带后端标签，读的时候不用猜
            let data = codec::encode(piece.data, self.compress)?;

            // 4. 落盘（已存在的块直接跳过）
            let dest = self.data_path.join(hash2path(&hash));
            if !dest.exists() {
                self.write_blob(&dest, &data)?;
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
                self.write_blob(&dest, &[])?;
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

    #[cfg(feature = "tempfile")]
    #[test]
    fn staging_dir_prefers_a_same_device_directory_and_falls_back_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("staging");
        std::fs::create_dir_all(&sub).unwrap();
        let dest = dir.path().join("data").join("ab").join("blob");
        // 目标的父目录必须先存在：设备号是靠它 stat 出来的
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();

        // 同一个文件系统：采用配置的目录
        assert_eq!(staging_dir(&sub, &dest), sub);
        assert_eq!(staging_dir(dir.path(), &dest), dir.path());

        // 不存在（取不到元数据）时保守退回目标的父目录
        let missing = dir.path().join("not-there");
        assert_eq!(
            staging_dir(&missing, &dest),
            dir.path().join("data").join("ab")
        );
    }

    #[cfg(feature = "tempfile")]
    #[test]
    fn staging_dir_handles_a_bare_file_name() {
        // 目标没有父目录（裸文件名）时，退回当前目录而不是空路径
        assert_eq!(
            staging_dir(Path::new("/nonexistent"), Path::new("out.bin")),
            PathBuf::from(".")
        );
    }
}
