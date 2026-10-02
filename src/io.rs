//! 外界文件与 [`Fs`] 之间的进出通道。
//!
//! 两个 trait 各自只有一个方向： [`FsInput`] 把真实文件复制进 `Fs`，
//! [`FsOutput`] 把 `Fs` 里的文件复制出去。它们都不做目录递归，只处理单个文件。

use crate::{db::FsDbWrite, file::FsFile, fs::Fs};
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

/// 把外界文件导入 [`Fs`] 的方法。
///
/// 一次导入 = 分块落盘（[`crate::fs::Fs::file_processing`]）+ 写数据库元数据。
pub trait FsInput {
    /// 把 `path` 处的文件导入 `Fs`，登记为 id `id`。
    ///
    /// 同一个 id 反复导入会累积成多个历史版本（以毫秒时间戳为键）。
    fn copy_in<P: AsRef<Path>, I: AsRef<str>>(&self, path: P, id: I) -> Result<(), FsIoError>;

    /// 批量导入，每一项是 `(id, path)`。
    ///
    /// 所有元数据在**一次**数据库事务里提交；块数据则是逐个落盘的。
    /// 因此中途失败会留下若干已经写好、但没有被任何元数据引用的块
    /// （内容寻址系统的常见取舍：它们不在引用计数表里，`release` 不会清理，
    /// 但下次导入同样内容时会被直接复用）。
    fn copy_in_many<I, P, T>(&self, item: T) -> Result<(), FsIoError>
    where
        T: IntoIterator<Item = (I, P)>,
        I: AsRef<str>,
        P: AsRef<Path>;
}

/// 把 [`Fs`] 里的文件导出到外界的方法。
pub trait FsOutput {
    /// 把 `file` 的内容写到 `path`，若 `path` 已存在则覆盖。
    fn copy_out<P: AsRef<Path>>(&self, path: P, file: FsFile) -> Result<(), FsIoError>;
}

/// [`Fs`] 输入输出过程中可能出现的错误。
#[derive(Debug, thiserror::Error)]
pub enum FsIoError {
    #[error("io error: {0}")]
    IoError(#[from] io::Error),
    #[error("fs db error: {0}")]
    DbError(#[from] crate::db::FsDbError),
}

impl FsInput for Fs {
    fn copy_in<P: AsRef<Path>, I: AsRef<str>>(&self, path: P, id: I) -> Result<(), FsIoError> {
        let chunks = self.file_processing(path.as_ref())?;
        let file = FsFile::from(chunks);
        self.insert(id, file)?;
        Ok(())
    }

    fn copy_in_many<I, P, T>(&self, item: T) -> Result<(), FsIoError>
    where
        T: IntoIterator<Item = (I, P)>,
        I: AsRef<str>,
        P: AsRef<Path>,
    {
        let mut v = Vec::new();
        for (id, path) in item {
            let chunks = self.file_processing(path.as_ref())?;
            let file = FsFile::from(chunks);
            v.push((id, file));
        }
        self.insert_many(v)?;
        Ok(())
    }
}

impl FsOutput for Fs {
    fn copy_out<P: AsRef<Path>>(&self, path: P, mut file: FsFile) -> Result<(), FsIoError> {
        // 这里必须是 create（写），而不是 open（只读）——否则写入必定失败
        let mut out = File::create(path)?;
        io::copy(&mut file, &mut out)?;
        out.flush()?;
        Ok(())
    }
}
