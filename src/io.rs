//! 外界文件与 [`Fs`] 之间的进出通道。
//!
//! 两个 trait 各自只有一个方向： [`FsInput`] 把真实文件复制进 `Fs`，
//! [`FsOutput`] 把 `Fs` 里的文件复制出去。它们都不做目录递归，只处理单个文件。

use crate::{db::FsDbWrite, file::FsFile, fs::Fs};
use std::{io, path::Path};

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
    ///
    /// 目标路径的父目录不存在时会按需创建。
    fn copy_out<P: AsRef<Path>>(&self, path: P, file: FsFile) -> Result<(), FsIoError>;
}

/// [`Fs`] 输入输出过程中可能出现的错误。
#[derive(Debug, thiserror::Error)]
pub enum FsIoError {
    #[error("io error: {0}")]
    IoError(#[from] io::Error),
    #[error("fs db error: {0}")]
    DbError(#[from] crate::db::FsDbError),
    /// 临时文件 rename 到目标位置失败（磁盘满、权限、跨设备等）。
    #[cfg(feature = "tempfile")]
    #[error("tempfile persist error: {0}")]
    TempfilePersistError(#[from] tempfile::PersistError),
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
    /// 先写临时文件再 rename 到目标。
    ///
    /// 临时文件由 [`crate::fs::staging_dir`] 选目录：优先用配置的 `temp_dir`，
    /// 但它必须与目标位于同一个文件系统，否则退回目标的父目录 ——
    /// `rename` 跨文件系统会失败（`EXDEV`）。
    #[cfg(feature = "tempfile")]
    fn copy_out<P: AsRef<Path>>(&self, path: P, mut file: FsFile) -> Result<(), FsIoError> {
        use tempfile::NamedTempFile;

        let dest = path.as_ref();
        let staging = crate::fs::staging_dir(&self.temp_dir, dest);
        std::fs::create_dir_all(&staging)?;
        let mut temp_file = NamedTempFile::new_in(&staging)?;
        io::copy(&mut file, &mut temp_file)?;
        temp_file.persist(dest)?;
        Ok(())
    }

    /// 没启用 `tempfile` feature：直接写目标文件。
    /// 写入过程中掉电可能留下内容不完整的文件。
    #[cfg(not(feature = "tempfile"))]
    fn copy_out<P: AsRef<Path>>(&self, path: P, mut file: FsFile) -> Result<(), FsIoError> {
        use std::io::Write;

        let mut out = std::fs::File::create(&path)?;
        io::copy(&mut file, &mut out)?;
        out.flush()?;
        Ok(())
    }
}
