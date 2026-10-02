use std::{
    fs::File,
    io::{Read, Seek},
    path::PathBuf,
};

use blake3::Hash;
use serde::{Deserialize, Serialize};

use crate::db::hash2path;

/// 一个真实文件
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Chunk {
    /// 哈希索引
    pub(crate) hash: Hash,
    /// 大小
    pub(crate) size: usize,
    /// 开始位置
    pub(crate) offset: u64,
}

impl Chunk {
    const ZSTD_MAGIC: u32 = 0xFD2FB528;
    #[inline]
    pub(crate) fn from_slice(s: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(s)
    }
    #[inline]
    pub(crate) fn to_vec(self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(&self)
    }
    pub(crate) fn read(&self) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(self.size);
        let mut file = File::open(self.path())?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        file.seek(std::io::SeekFrom::Start(0))?;
        file.read_to_end(&mut buf)?;
        if u32::from_le_bytes(magic) == Self::ZSTD_MAGIC {
            buf = zstd::decode_all(buf.as_slice())?;
        }
        Ok(buf)
    }
    #[inline]
    #[must_use]
    pub(crate) fn path(&self) -> PathBuf {
        hash2path(&self.hash)
    }
}
