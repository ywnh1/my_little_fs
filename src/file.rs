use crate::chunk::Chunk;
use std::{
    io::{self, Read, Seek},
    sync::Arc,
};

/// `Fs` 的文件，实现 `Read` 和 `Seek`，不可读
///
/// 如果要读，可以复制出去读
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsFile {
    /// 所有索引
    pub(crate) chunks: Arc<Vec<Chunk>>,
    /// 文件总大小，懒加载
    pub(crate) size: Option<u64>,
    /// 当前光标位置
    pub(crate) pos: (usize, usize), // 第几个文件第几个
    /// 缓存的一个文件
    pub(crate) cache: Option<Arc<Vec<u8>>>,
}

impl From<Vec<Chunk>> for FsFile {
    fn from(v: Vec<Chunk>) -> Self {
        Self {
            chunks: Arc::new(v),
            size: None,
            pos: (0, 0),
            cache: None,
        }
    }
}

impl FsFile {
    /// 懒加载 `size`，如果加载过了，就直接取
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
    /// 寻找某一个位置，是否存在，如果存在，就转化成 (分片idx,字节idx) 的的格式
    #[inline]
    pub fn find(&self, pos: u64) -> Option<(usize, usize)> {
        let (num, chunk) = self
            .chunks
            .iter()
            .enumerate()
            .find(|(_, chunk)| chunk.offset >= pos)?;
        Some((num, (pos - chunk.offset) as usize))
    }
    /// 所有情况
    /// 1. 没有 `cache`: 需要读
    /// 2. `cache` 已经结束: 需要读
    /// 3. `cache` 存在且没有结束: 直接读
    /// 4. 读完了: 返回 `None`
    pub fn next_byte(&mut self) -> std::io::Result<Option<u8>> {
        if let Some(mut cache) = self.cache.clone() {
            if self.pos.1 + 1 < cache.len() {
                // 情况 3
                self.pos.1 += 1;
                Ok(Some(cache[self.pos.1]))
            } else if self.pos.0 < self.chunks.len() {
                // 情况 2
                self.pos.0 += 1;
                self.pos.1 = 0;
                let data = self.chunks[self.pos.0].read()?;
                cache = Arc::new(data);
                Ok(Some(cache[0]))
            } else {
                // 情况 4
                Ok(None)
            }
        } else {
            // 情况 1
            self.pos.0 = 0;
            self.pos.1 = 0;
            let data = self.chunks[0].read()?;
            let cache = Arc::new(data);
            self.cache = Some(cache.clone());
            Ok(Some(cache[0]))
        }
    }
}
impl Read for FsFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read_size = 0;
        while read_size < buf.len() {
            buf[read_size] = if let Some(b) = self.next_byte()? {
                b
            } else {
                break;
            };
            read_size += 1;
        }
        Ok(read_size)
    }
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let length = self.get_size() - self.stream_position().unwrap();
        for _ in 0..length {
            buf.push(self.next_byte()?.unwrap());
        }
        Ok(length as usize)
    }
}

impl Seek for FsFile {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        use io::SeekFrom::*;
        match pos {
            Start(pos) => {
                self.pos = if let Some(p) = self.find(pos) {
                    p
                } else {
                    (self.chunks.len(), (pos - self.get_size()) as usize)
                };
                Ok(pos)
            }
            End(pos) => {
                let pos = self.get_size() as i128 - pos as i128;
                self.seek(Start(pos as u64))
            }
            Current(pos) => {
                let pos = self.stream_position()? as i128 + pos as i128;
                if pos < 0 {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid seek to a negative or overflowing position",
                    ))
                } else {
                    self.seek(Start(pos as u64))
                }
            }
        }
    }
    /// 随便 unwrap，不会出错
    fn stream_position(&mut self) -> io::Result<u64> {
        let mut pos = self.pos.1 as u64;
        for chunk in 0..self.pos.0 {
            let chunk = self.chunks[chunk];
            pos += chunk.size as u64;
        }
        Ok(pos)
    }
    /// 随便 unwrap，不会出错
    fn rewind(&mut self) -> io::Result<()> {
        self.pos = (0, 0);
        Ok(())
    }
}
