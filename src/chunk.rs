//! 数据块（`Chunk`）的元数据表示。
//!
//! `Fs` 把一个逻辑文件按内容定义分块（CDC）切开，每块用 blake3 摘要寻址，
//! 相同内容的块在磁盘上只存一份 —— 去重就发生在这一层。
//!
//! 磁盘布局固定为 `<data_path>/<hash 的 hex 前 2 位>/<hash 的 hex 其余 62 位>`，
//! 由 [`crate::db::hash2path`] 统一计算。写入侧见 [`crate::fs::Fs::file_processing`]，
//! 读取侧见 [`Chunk::read`]：**两侧必须使用同一套布局规则**。

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
    /// 块的**原始（解压后）**长度。拼接文件、计算偏移都以此为准，与磁盘上的实际字节数无关
    pub(crate) size: usize,
    /// 块在所属逻辑文件中的起始偏移
    pub(crate) offset: u64,
}

impl Chunk {
    /// zstd 帧魔数 `0xFD2FB528`，磁盘上按小端写作 `28 B5 2F FD`。
    ///
    /// 用它嗅探磁盘上的数据有没有被压缩过，从而让「压缩」与「未压缩」两种块
    /// 能共存在同一个数据目录里（`Fs` 的压缩设置可以中途改变）。
    const ZSTD_MAGIC: u32 = 0xFD2FB528;

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

    /// 读出该块的**原始**内容（必要时自动解压）。
    ///
    /// `data_root` 是 `Fs` 的数据根目录。块路径本身是相对路径，
    /// 调用方必须传入与写入时相同的根目录；传空路径会退化成相对
    /// **进程当前工作目录**查找，那只适合测试。
    pub(crate) fn read(&self, data_root: &Path) -> io::Result<Vec<u8>> {
        let raw = std::fs::read(data_root.join(self.path()))?;
        // 不足 4 字节不可能带魔数，原样返回
        if raw.len() >= 4 && u32::from_le_bytes(raw[..4].try_into().unwrap()) == Self::ZSTD_MAGIC {
            zstd::decode_all(raw.as_slice())
        } else {
            Ok(raw)
        }
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

    /// 在临时目录里按 `.path()` 摆好一块数据，然后用 `read` 读回来。
    fn place(root: &Path, data: &[u8]) -> Chunk {
        let hash = blake3::hash(data);
        let chunk = Chunk {
            hash,
            size: data.len(),
            offset: 0,
        };
        let path = root.join(chunk.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
        chunk
    }

    #[test]
    fn uncompressed_data_reads_back_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"hello, chunk".to_vec();
        let chunk = place(dir.path(), &data);
        assert_eq!(chunk.read(dir.path()).unwrap(), data);
    }

    #[test]
    fn compressed_data_is_transparently_decompressed() {
        let dir = tempfile::tempdir().unwrap();
        // 高度可压缩的内容，保证压缩后一定带 zstd 魔数
        let data = vec![7u8; 4096];
        let compressed = zstd::encode_all(data.as_slice(), 3).unwrap();
        assert_eq!(
            u32::from_le_bytes(compressed[..4].try_into().unwrap()),
            Chunk::ZSTD_MAGIC,
            "前提：压缩产物必须带 zstd 魔数，否则本测试没有意义"
        );

        let hash = blake3::hash(&data);
        let chunk = Chunk {
            hash,
            size: data.len(),
            offset: 0,
        };
        let path = dir.path().join(chunk.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &compressed).unwrap();

        // 注意：读回来的是**原始**数据，块大小也按原始长度记账
        assert_eq!(chunk.read(dir.path()).unwrap(), data);
    }

    #[test]
    fn empty_chunk_is_not_mistaken_for_a_stream() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = place(dir.path(), b"");
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
}
