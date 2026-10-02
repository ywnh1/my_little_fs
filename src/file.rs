//! 逻辑文件在 `Fs` 中的表示：一串 [`Chunk`] 的引用列表。
//!
//! [`FsFile`] 实现了 [`Read`] 与 [`Seek`]，语义刻意对齐 [`std::fs::File`]：
//! 位置是一个**绝对字节偏移**，允许 seek 到文件末尾之后（此后读到 0 字节）。

use crate::chunk::Chunk;
use std::{
    io::{self, Read, Seek},
    path::PathBuf,
    sync::Arc,
};

/// `Fs` 中的一份文件快照（某个 id 在某个历史版本下的内容）。
///
/// # 位置与缓存
///
/// - `pos` 是**下一个待读字节**的绝对偏移，取值 `0..=size` 都合法；
///   `pos == size` 表示已到末尾，此时 `read` 返回 `0`。
/// - `cache` 最多缓存一个已经解压好的块，即 `(块下标, 块内容)`。
///   顺序读取时只在跨块边界处发生一次磁盘 IO 与解压。
///
/// # 数据从哪来
///
/// 块的实体数据不在 `FsFile` 里，而在 `data_path` 下的内容寻址文件里。
/// `data_path` 由 [`crate::db::FsDbReadOnly::get`] 取出文件时注入。
/// 手工用 [`Default`] 或 [`From<Vec<Chunk>>`](From) 构造的 `FsFile` 其
/// `data_path` 为空路径，会相对**进程当前工作目录**去找块文件 —— 仅适合测试。
///
/// # 相等性
///
/// `PartialEq` 由派生实现，因此 `cache` 也参与比较：两个内容相同、
/// 只是缓存命中情况不同的 `FsFile` 会被判为不相等。判断内容是否相同请比较
/// [`get_size`](FsFile::get_size) 与读出的字节。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsFile {
    /// 组成该文件的所有块，按 `offset` 升序
    pub(crate) chunks: Arc<Vec<Chunk>>,
    /// 块实体数据所在的数据根目录，由 `Fs` 注入
    pub(crate) data_path: Arc<PathBuf>,
    /// 文件总大小，首次计算后缓存（懒加载）
    pub(crate) size: Option<u64>,
    /// 下一个待读字节的绝对偏移
    pub(crate) pos: u64,
    /// 当前缓存的块：`(块下标, 已解压内容)`
    pub(crate) cache: Option<(usize, Arc<Vec<u8>>)>,
}

impl From<Vec<Chunk>> for FsFile {
    /// 用一组块构造文件，位置归零、无缓存、`data_path` 为空。
    fn from(v: Vec<Chunk>) -> Self {
        Self {
            chunks: Arc::new(v),
            ..Default::default()
        }
    }
}

impl FsFile {
    /// 文件总大小 = 所有块的原始大小之和。首次调用后缓存结果。
    #[inline]
    pub fn get_size(&mut self) -> u64 {
        if let Some(n) = self.size {
            n
        } else {
            let size = self.chunks.iter().map(|c| c.size as u64).sum();
            self.size = Some(size);
            size
        }
    }

    /// 定位包含绝对偏移 `pos` 的块：返回 `(块下标, 块内偏移)`。
    /// `pos` 落在文件末尾之后（含末尾）时返回 `None`。
    #[inline]
    pub fn find(&self, pos: u64) -> Option<(usize, usize)> {
        self.chunks.iter().enumerate().find_map(|(idx, chunk)| {
            let start = chunk.offset;
            let end = start + chunk.size as u64; // 半开区间 [start, end)
            (pos >= start && pos < end).then(|| (idx, (pos - start) as usize))
        })
    }

    /// 取出第 `idx` 块已解压的内容；若它已在缓存里则直接复用，不复制字节。
    fn chunk_data(&mut self, idx: usize) -> io::Result<Arc<Vec<u8>>> {
        if let Some((cached_idx, data)) = &self.cache
            && *cached_idx == idx
        {
            return Ok(Arc::clone(data));
        }
        let data = Arc::new(self.chunks[idx].read(&self.data_path)?);
        self.cache = Some((idx, Arc::clone(&data)));
        Ok(data)
    }

    /// 读下一个字节；已到末尾返回 `Ok(None)`。
    ///
    /// 这是 [`Read`] 的便捷封装，行为与 `read` 一致：读完之后 `pos` 停在末尾。
    pub fn next_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0u8; 1];
        Ok(if self.read(&mut byte)? == 1 {
            Some(byte[0])
        } else {
            None
        })
    }
}

impl Read for FsFile {
    /// 从当前位置读入 `buf`，返回实际读到的字节数；到末尾时返回 `Ok(0)`。
    ///
    /// 一次调用可能跨越多个块，跨块时会按需换出缓存。
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.get_size();
        let mut read = 0;
        while read < buf.len() && self.pos < size {
            // `pos < size` 时必然落在某个块内，None 说明块的 offset/size 不自洽
            let Some((idx, offset)) = self.find(self.pos) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "块索引不连续，无法定位文件位置",
                ));
            };
            let data = self.chunk_data(idx)?;
            // 块数据比元数据短说明磁盘上的内容被损坏或被截断
            let take = (data.len().saturating_sub(offset)).min(buf.len() - read);
            if take == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "块数据长度小于其元数据记录的原始大小",
                ));
            }
            buf[read..read + take].copy_from_slice(&data[offset..offset + take]);
            read += take;
            self.pos += take as u64;
        }
        Ok(read)
    }
}

impl Seek for FsFile {
    /// 移动到指定位置，返回移动后的绝对偏移。
    ///
    /// - `Start(n)`：从文件头起第 n 字节
    /// - `End(n)`：文件末尾再加 n（`n` 为负即从末尾往前）
    /// - `Current(n)`：当前位置再加 n
    ///
    /// 允许越过文件末尾（与 [`std::fs::File`] 一致），越过之后再读会得到 `Ok(0)`。
    /// 定位到文件开头之前会报 [`io::ErrorKind::InvalidInput`]。
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        use io::SeekFrom::*;
        // 用 i128 中转，避免 usize/u64 与负偏移混算时溢出
        let target: i128 = match pos {
            Start(n) => n as i128,
            End(n) => self.get_size() as i128 + n as i128,
            Current(n) => self.pos as i128 + n as i128,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative position",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

// `stream_position`、`rewind`、`read_to_end` 一律使用 `Seek`/`Read` 的默认实现：
// 默认实现都建立在 `seek` 与 `read` 之上，语义正确且无需维护第二份逻辑。

#[cfg(test)]
mod tests {
    use super::*;

    /// 把若干数据块真实落盘到一个临时目录，返回 (临时目录, 文件, 原始内容)。
    ///
    /// 这里刻意把 `data_path` 指向临时目录，而不是依赖进程当前工作目录。
    fn fixture(parts: &[&[u8]]) -> (tempfile::TempDir, FsFile, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let mut chunks = Vec::new();
        let mut whole = Vec::new();
        let mut offset = 0u64;
        for part in parts {
            let hash = blake3::hash(part);
            let chunk = Chunk {
                hash,
                size: part.len(),
                offset,
            };
            let path = dir.path().join(chunk.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, part).unwrap();

            whole.extend_from_slice(part);
            offset += part.len() as u64;
            chunks.push(chunk);
        }
        let mut file = FsFile::from(chunks);
        file.data_path = Arc::new(dir.path().to_path_buf());
        (dir, file, whole)
    }

    #[test]
    fn find_locates_the_block_containing_the_offset() {
        let (_dir, file, whole) = fixture(&[b"aaaa", b"bbbb", b"cccc"]);
        assert_eq!(whole.len(), 12);
        // 每个位置都应落在「包含」它的块里，而不是下一个块
        assert_eq!(file.find(0), Some((0, 0)));
        assert_eq!(file.find(3), Some((0, 3)));
        assert_eq!(file.find(4), Some((1, 0)));
        assert_eq!(file.find(7), Some((1, 3)));
        assert_eq!(file.find(8), Some((2, 0)));
        // 末尾及之后都不属于任何块
        assert_eq!(file.find(12), None);
        assert_eq!(file.find(13), None);
    }

    #[test]
    fn read_to_end_reassembles_the_whole_file_across_blocks() {
        let (_dir, mut file, whole) = fixture(&[b"hello ", b"little ", b"fs"]);
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert_eq!(got, whole);
        assert_eq!(got, b"hello little fs");
    }

    #[test]
    fn reading_past_the_end_yields_zero_bytes() {
        let (_dir, mut file, whole) = fixture(&[b"abc"]);
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), whole.len());
        let mut extra = [0u8; 4];
        assert_eq!(file.read(&mut extra).unwrap(), 0);
        assert_eq!(file.next_byte().unwrap(), None);
    }

    #[test]
    fn next_byte_walks_across_block_boundaries_in_order() {
        // 回归点：跨块之后必须继续返回**新**块的字节，而不是旧块缓存里的字节
        let (_dir, mut file, whole) = fixture(&[b"0123", b"4567", b"89"]);
        let mut got = Vec::new();
        while let Some(b) = file.next_byte().unwrap() {
            got.push(b);
        }
        assert_eq!(got, whole);
    }

    #[test]
    fn seek_start_then_read_returns_bytes_from_that_offset() {
        let (_dir, mut file, whole) = fixture(&[b"aaaa", b"bbbb"]);
        // 回归点：seek 之后首次读取必须从 seek 的位置开始
        assert_eq!(file.seek(io::SeekFrom::Start(5)).unwrap(), 5);
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert_eq!(got, &whole[5..]);
        assert_eq!(got, b"bbb");
    }

    #[test]
    fn seek_end_and_current_are_relative_to_the_right_origin() {
        let (_dir, mut file, whole) = fixture(&[b"0123456789"]);
        assert_eq!(file.seek(io::SeekFrom::End(-3)).unwrap(), 7);
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"789");

        file.rewind().unwrap();
        assert_eq!(file.seek(io::SeekFrom::Current(4)).unwrap(), 4);
        assert_eq!(file.read_u8_checked(), whole[4]);

        // 越过末尾是合法的，此后读取得到 0 字节
        assert_eq!(file.seek(io::SeekFrom::Start(999)).unwrap(), 999);
        let mut buf = [0u8; 8];
        assert_eq!(file.read(&mut buf).unwrap(), 0);
        assert_eq!(file.stream_position().unwrap(), 999);

        // 定位到开头之前则不合法
        assert!(file.seek(io::SeekFrom::Start(0)).is_ok());
        let err = file.seek(io::SeekFrom::Current(-1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn empty_file_reads_as_empty_and_finds_nothing() {
        // 回归点：空文件曾经在读取时越界 panic
        let (_dir, mut file, _) = fixture(&[]);
        assert_eq!(file.get_size(), 0);
        assert_eq!(file.find(0), None);
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
        assert_eq!(file.next_byte().unwrap(), None);
    }

    #[test]
    fn partial_reads_advance_the_cursor() {
        let (_dir, mut file, whole) = fixture(&[b"abcdefgh"]);
        let mut buf = [0u8; 3];
        assert_eq!(file.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(file.stream_position().unwrap(), 3);
        assert_eq!(file.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"def");
        assert_eq!(file.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"gh");
        // 内容整体一致
        file.rewind().unwrap();
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert_eq!(got, whole);
    }

    /// 测试辅助：读一个字节并断言确实读到了（避免测试里反复写 if let）
    trait ReadOne {
        fn read_u8_checked(&mut self) -> u8;
    }
    impl ReadOne for FsFile {
        fn read_u8_checked(&mut self) -> u8 {
            self.next_byte().unwrap().expect("期望还能读到一个字节")
        }
    }
}
