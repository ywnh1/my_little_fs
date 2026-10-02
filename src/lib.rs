pub mod chunk;
pub mod db;
pub mod file;
pub mod fs;
pub mod io;

pub mod prelude {
    pub use crate::chunk::Chunk;
    pub use crate::db::FsDbReadOnly;
    pub use crate::file::FsFile;
    pub use crate::fs::{Fs, FsBuilder};
    pub use crate::io::{FsInput, FsOutput};
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("fs io error: {0}")]
    FsIoError(#[from] crate::io::FsIoError),
    #[error("fs db error: {0}")]
    FsDbError(#[from] crate::db::FsDbError),
}
