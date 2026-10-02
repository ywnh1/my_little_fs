use fastcdc::v2020::StreamCDC;
use redb::Database;
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::{chunk::Chunk, db::hash2path};

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

/// 一个创建 `Fs` 对象的工厂
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub struct FsBuilder {
    /// cdc 分片的最大大小
    pub max_size: Option<usize>,
    /// cdc 分片的最小大小
    pub min_size: Option<usize>,
    /// cdc 分片的平均大小
    pub avg_size: Option<usize>,
    /// 是否启用压缩
    /// - 启用：`Some(level)`
    /// - 不启用：`None`
    pub compress: Option<i32>,
    /// 指定数据库的位置
    pub db_path: Option<PathBuf>,
    /// 指定数据的储存位置
    pub data_path: Option<PathBuf>,
    /// 指定 `Fs` 根目录的位置，会被单独指定的其他路径覆盖
    pub root_path: PathBuf,
}

/// 保存 cdc 分片的三个 `*_size`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub struct CdcOptions {
    pub max_size: usize,
    pub min_size: usize,
    pub avg_size: usize,
}

impl From<&FsBuilder> for CdcOptions {
    fn from(value: &FsBuilder) -> Self {
        let (min_size, avg_size, max_size) =
        // 为了避免传入的内容 max < min 这种奇怪的问题，手动对最后的结果进行排序
        // 如果本来顺序是对的，这些匹配和排序不会改变
            sort_tuple(match (value.min_size, value.avg_size, value.max_size) {
                (None, None, None) => (16384, 32768, 65536), // 默认使用官方文档提供的
                (Some(min), Some(avg), Some(max)) => (min, avg, max),
                (Some(min), None, None) => (min, min * 4, min * 16), // 否则 1:4:16
                (None, Some(avg), None) => (avg / 4, avg, avg * 4),
                (None, None, Some(max)) => (max / 16, max / 4, max),
                (Some(min), Some(avg), None) => (min, avg, avg * 4),
                (None, Some(avg), Some(max)) => (avg / 4, avg, max),
                (Some(min), None, Some(max)) => {
                    let avg_min = min * 4;
                    let avg_max = max / 4;
                    let avg = if avg_min < max && avg_max > min {
                        (avg_max + avg_min) / 2
                    } else if avg_min < max {
                        avg_min
                    } else if avg_max > min {
                        avg_max
                    } else {
                        (max + min) / 2
                    };
                    (min, avg, max)
                }
            });

        Self {
            max_size,
            min_size,
            avg_size,
        }
    }
}

impl FsBuilder {
    /// 创建新工厂
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            ..Default::default()
        }
    }
    /// 消耗自身，构建 `Fs`
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
    pub fn with_cdc_config(mut self, options: CdcOptions) -> Self {
        self.max_size = Some(options.max_size);
        self.min_size = Some(options.min_size);
        self.avg_size = Some(options.avg_size);
        self
    }
}

pub struct Fs {
    pub(crate) db: Database,
    pub(crate) cdc_opt: CdcOptions,
    pub(crate) data_path: PathBuf,
    pub(crate) compress: Option<i32>,
}

impl Fs {
    /// 返回一个工厂 `FsBuilder`
    pub fn builder(root_path: PathBuf) -> FsBuilder {
        FsBuilder::new(root_path)
    }

    pub(crate) fn file_processing(&self, path: &Path) -> io::Result<Vec<Chunk>> {
        let mut chunks = Vec::new();
        let mut f = File::open(path)?;
        let cdc = StreamCDC::new(
            &mut f,
            self.cdc_opt.min_size,
            self.cdc_opt.avg_size,
            self.cdc_opt.max_size,
        );
        for res in cdc {
            // 1. 分块
            let chunk = res?;
            // 2. 哈希
            let hash = blake3::hash(&chunk.data);
            let mut data = chunk.data;
            // 3. 寻址
            let path = self.data_path.join(hash2path(&hash));
            if !path.exists() // 如果文件不存在，肯定得创造
                || (
                // 如果需要压缩，就简单判断文件大小
                // 如果和数据一样或者大，就也需要压缩和写入
                self.compress.is_some()
                    && path.metadata()?.len() >= data.len() as u64
            ) {
                // 4. 压缩
                if let Some(level) = self.compress {
                    data = zstd::encode_all(data.as_slice(), level)?;
                }
                // 5. 写入文件
                let mut f = File::create(path)?;
                f.write_all(&data)?;
            }
            // 6. 建立 `Chunk`
            let chunk = Chunk {
                hash,
                size: chunk.length,
                offset: chunk.offset,
            };
            chunks.push(chunk);
        }
        Ok(chunks)
    }
}

fn sort_tuple(tup: (usize, usize, usize)) -> (usize, usize, usize) {
    let (mut a, mut b, mut c) = tup;
    // 1. 比较 a 和 b，保证 a <= b
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    // 2. 比较 a 和 c，保证 a <= c（此时 a 是三个数中最小的）
    if a > c {
        std::mem::swap(&mut a, &mut c);
    }
    // 3. 比较 b 和 c，保证 b <= c（此时 b 是中间值，c 是最大值）
    if b > c {
        std::mem::swap(&mut b, &mut c);
    }
    (a, b, c)
}
