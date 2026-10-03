//! 数据块（`Chunk`）的元数据表示。
//!
//! `Fs` 把一个逻辑文件按内容定义分块（CDC）切开，每块用 blake3 摘要寻址，
//! 相同内容的块在磁盘上只存一份 —— 去重就发生在这一层。
//!
//! 磁盘布局固定为 `<data_path>/<hash 的 hex 前 2 位>/<hash 的 hex 其余 62 位>`，
//! 由 [`crate::db::hash2path`] 统一计算。写入侧见 [`crate::fs::Fs::file_processing`]，
//! 读取侧见 [`Chunk::read`]：**两侧必须使用同一套布局规则**。
//!
//! 块文件的内容不是原始字节，而是 `[1 字节编码标签][载荷]` ——
//! 压缩后端可以任选、可以共存，具体见 [`crate::codec`]。

use std::{
    io,
    path::{Path, PathBuf},
};

use blake3::Hash;
use serde::{Deserialize, Serialize};

use crate::db::hash2path;

/// 一个数据块的元数据。
///
/// 注意它**不持有数据本身**，只回答三个问题：是哪块内容（`hash`）、
/// 原始内容多大（`size`）、在所属逻辑文件的哪个位置（`offset`）。
/// 真正的字节存放在磁盘上那个由 `hash` 决定的文件里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Chunk {
    /// 内容的 blake3 摘要；既用于去重，也决定数据文件在磁盘上的存放路径（内容寻址）
    pub(crate) hash: Hash,
    /// 块的**原始（解压前）**长度。拼接文件、计算偏移都以此为准，
    /// 与磁盘上的实际字节数无关
    pub(crate) size: usize,
    /// 块在所属逻辑文件中的起始偏移
    pub(crate) offset: u64,
}

impl Chunk {
    /// 从字节反序列化出元数据（postcard 格式），供数据库读取使用。
    #[inline]
    pub(crate) fn from_slice(s: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(s)
    }

    /// 把元数据序列化成字节（postcard 格式），供数据库写入使用。
    #[inline]
    pub(crate) fn to_vec(self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(&self)
    }

    /// 读出该块的**原始**内容，需要时按标签解压。
    ///
    /// `data_root` 是 `Fs` 的数据根目录。块路径本身是相对路径，
    /// 调用方必须传入与写入时相同的根目录；传空路径会退化成相对
    /// **进程当前工作目录**查找，那只适合测试。
    ///
    /// 块是用本构建没编译进来的后端压的时，返回
    /// [`io::ErrorKind::Unsupported`]，并在信息里点出是哪个后端。
    pub(crate) fn read(&self, data_root: &Path) -> io::Result<Vec<u8>> {
        let raw = std::fs::read(data_root.join(self.path()))?;
        crate::codec::decode(raw)
    }

    /// 该块在数据目录中的**相对**路径，需由调用方拼接数据根目录后再使用。
    #[inline]
    #[must_use]
    pub(crate) fn path(&self) -> PathBuf {
        hash2path(&self.hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;

    /// 按 `.path()` 在临时目录里摆好一块数据（走正规编码），返回它的元数据。
    fn place(root: &Path, data: &[u8], compress: Option<codec::Compress>) -> Chunk {
        let hash = blake3::hash(data);
        let chunk = Chunk {
            hash,
            size: data.len(),
            offset: 0,
        };
        let path = root.join(chunk.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, codec::encode(data.to_vec(), compress).unwrap()).unwrap();
        chunk
    }

    #[test]
    fn uncompressed_data_reads_back_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"hello, chunk".to_vec();
        let chunk = place(dir.path(), &data, None);
        assert_eq!(chunk.read(dir.path()).unwrap(), data);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compressed_data_is_transparently_decompressed() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![7u8; 4096];
        let chunk = place(dir.path(), &data, Some(codec::Compress::zstd(3)));
        // 读回来的是原始数据，而块的 size 也一直按原始长度记账
        assert_eq!(chunk.read(dir.path()).unwrap(), data);
        assert_eq!(chunk.size, data.len());
    }

    #[test]
    fn empty_chunk_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = place(dir.path(), b"", None);
        assert_eq!(chunk.read(dir.path()).unwrap(), b"");
    }

    #[test]
    fn metadata_serialization_roundtrips() {
        let chunk = Chunk {
            hash: blake3::hash(b"abc"),
            size: 12345,
            offset: 678,
        };
        let bytes = chunk.to_vec().unwrap();
        assert_eq!(Chunk::from_slice(&bytes).unwrap(), chunk);
    }

    /// 缺文件时要报错，而不是当成空块。
    #[test]
    fn missing_blob_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = Chunk {
            hash: blake3::hash(b"never written"),
            size: 3,
            offset: 0,
        };
        assert!(chunk.read(dir.path()).is_err());
    }
}
