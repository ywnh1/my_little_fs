use crate::{db::FsDbWrite, file::FsFile, fs::Fs};
use std::{fs::File, io, path::Path};

/// 定义了从外界文件传入 `Fs` 的方法
pub trait FsInput {
    /// 把一个文件复制到 `Fs` 当中，自动处理数据库
    fn copy_in<P: AsRef<Path>, I: AsRef<str>>(&self, path: P, id: I) -> Result<(), FsIoError>;
    /// 把多个文件复制到 `Fs` 当中，自动处理数据库
    fn copy_in_many<I, P, T>(&self, item: T) -> Result<(), FsIoError>
    where
        T: IntoIterator<Item = (I, P)>,
        I: AsRef<str>,
        P: AsRef<Path>;
}

/// 定义了从 `FS` 导出到外界文件的方法
pub trait FsOutput {
    /// 把一个文件复制出 `Fs`
    fn copy_out<P: AsRef<Path>>(&self, path: P, file: FsFile) -> Result<(), FsIoError>;
}

/// 一些关于 `Fs` io的错误
#[derive(Debug, thiserror::Error)]
pub enum FsIoError {
    #[error("io error: {0}")]
    IoError(#[from] io::Error),
    #[error("fs db error: {0}")]
    DbError(#[from] crate::db::FsDbError),
    #[error("fastcdc error: {0}")]
    FastCdcError(#[from] fastcdc::v2020::Error),
    #[error("too many or less files getted by index: {0}")]
    IndexError(String),
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
        io::copy(&mut file, &mut File::open(path)?)?;
        Ok(())
    }
}
