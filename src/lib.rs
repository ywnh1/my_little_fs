//! # my_little_fs
//!
//! 一个内容寻址的文件系统：把文件按内容切块、用 blake3 摘要寻址、
//! 可选 zstd 压缩，块之间的重复内容自动去重。
//!
//! # 结构
//!
//! - [`fs`]：门面。用 [`Fs::builder`] / [`FsBuilder`] 打开一个 `Fs`
//! - [`db`]：元数据。`file_<id>` 存历史版本，`gc` 存块引用计数。
//!   读写分别由 [`FsDbReadOnly`] / [`FsDbWrite`] 提供，回收由 [`FsGc`] 提供
//! - [`chunk`]：块的元数据与磁盘布局
//! - [`file`]：逻辑文件 [`FsFile`]，实现 `Read` + `Seek`
//! - [`io`]：与外界的双向复制，[`FsInput`] / [`FsOutput`]
//!
//! # 例子
//!
//! ```no_run
//! use my_little_fs::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let fs = Fs::builder("/tmp/example-fs".into()).build()?;
//! fs.copy_in("photo.png", "album/photo")?;
//! let file = fs.get("album/photo", Index::Latest)?.remove(0);
//! fs.copy_out("/tmp/restored.png", file)?;
//! # Ok(())
//! # }
//! ```

pub mod chunk;
pub mod db;
pub mod file;
pub mod fs;
pub mod io;

/// 常用类型的集合。`use my_little_fs::prelude::*;` 即可开始读写。
pub mod prelude {
    pub use crate::chunk::Chunk;
    pub use crate::db::{FsDbReadOnly, FsDbWrite, FsGc, Index};
    pub use crate::file::FsFile;
    pub use crate::fs::{Fs, FsBuilder};
    pub use crate::io::{FsInput, FsOutput};
}

/// 把两个子模块的错误统一成一个类型的门面错误。
///
/// 实践中从 [`Fs`](crate::fs::Fs) 的方法里冒出来的错误大多已经是
/// [`FsIoError`](crate::io::FsIoError)（它内部已经能包住
/// [`FsDbError`](crate::db::FsDbError)），这个枚举主要用于调用方统一签名。
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("fs io error: {0}")]
    FsIoError(#[from] crate::io::FsIoError),
    #[error("fs db error: {0}")]
    FsDbError(#[from] crate::db::FsDbError),
}
