//! # db 储存规则
//!
//! - **gc**:   file id   -> used count 用于处理 GC，引用计数
//! - **file**: timestamp -> Chunks     从文件找到历史表，命名规则为 `file_{id}`
//!

use crate::{chunk::Chunk, file::FsFile, fs::Fs};
use blake3::Hash;
use redb::{
    MultimapTable, MultimapTableDefinition, MultimapTableHandle, ReadOnlyMultimapTable,
    ReadableDatabase, ReadableMultimapTable, ReadableTable, TableDefinition, WriteTransaction,
};
use std::{
    array::TryFromSliceError,
    fs, io,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const GC_TABLE: TableDefinition<&[u8], u64> = TableDefinition::new("gc");

#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub enum Index {
    /// 返回所有，按照时间戳升序
    All,
    /// 返回最老的，相当于 `Index::Index(0)`
    First,
    /// 返回最新的，相当于 `Index::ReIndex(0)`
    #[default]
    Latest,
    /// 返回指定毫秒时间戳的
    TimeStamp(u64),
    /// 返回第 n 个
    Index(usize),
    /// 返回倒序第 n 个
    ReIndex(usize),
    /// 返回多个
    Many(Vec<Self>),
}

impl Index {
    fn flatten(&self) -> Vec<Self> {
        if let Self::Many(v) = self {
            v.iter().flat_map(|idx| idx.flatten()).collect()
        } else {
            vec![self.clone()]
        }
    }
    fn select_read(
        &self,
        table: &ReadOnlyMultimapTable<u64, &[u8]>,
        buf: &mut Vec<FsFile>,
    ) -> Result<(), FsDbError> {
        match self {
            Index::All => {
                for kv in table.iter()? {
                    let (_key, value) = kv?;
                    let mut chunks = Vec::new();
                    for chunk in value {
                        let chunk = Chunk::from_slice(chunk?.value())?;
                        chunks.push(chunk);
                    }
                    buf.push(chunks.into());
                }
            }
            Index::Index(idx) => {
                let kv = if let Some(kv) = table.iter()?.nth(*idx) {
                    kv
                } else {
                    return Ok(());
                };
                let (_key, value) = kv?;
                let mut chunks = Vec::new();
                for chunk in value {
                    let chunk = Chunk::from_slice(chunk?.value())?;
                    chunks.push(chunk);
                }
                buf.push(chunks.into());
            }
            Index::ReIndex(idx) => {
                let kv = if let Some(kv) = table.iter()?.rev().nth(*idx) {
                    kv
                } else {
                    return Ok(());
                };
                let (_key, value) = kv?;
                let mut chunks = Vec::new();
                for chunk in value {
                    let chunk = Chunk::from_slice(chunk?.value())?;
                    chunks.push(chunk);
                }
                buf.push(chunks.into());
            }
            Index::First => {
                let kv = if let Some(kv) = table.iter()?.next() {
                    kv
                } else {
                    return Ok(());
                };
                let (_key, value) = kv?;
                let mut chunks = Vec::new();
                for chunk in value {
                    let chunk = Chunk::from_slice(chunk?.value())?;
                    chunks.push(chunk);
                }
                buf.push(chunks.into());
            }
            Index::Latest => {
                let kv = if let Some(kv) = table.iter()?.next_back() {
                    kv
                } else {
                    return Ok(());
                };
                let (_key, value) = kv?;
                let mut chunks = Vec::new();
                for chunk in value {
                    let chunk = Chunk::from_slice(chunk?.value())?;
                    chunks.push(chunk);
                }
                buf.push(chunks.into());
            }
            Index::TimeStamp(timestamp) => {
                let value = table.get(timestamp)?;
                let mut chunks = Vec::new();
                for chunk in value {
                    let chunk = Chunk::from_slice(chunk?.value())?;
                    chunks.push(chunk);
                }
                buf.push(chunks.into());
            }
            many => {
                let many = many.flatten();
                for idx in many {
                    idx.select_read(table, buf)?;
                }
            }
        }
        Ok(())
    }
    fn select_write(
        &self,
        table: &mut MultimapTable<u64, &[u8]>,
        buf: &mut Vec<FsFile>,
    ) -> Result<(), FsDbError> {
        let mut to_rm = Vec::new();
        match self {
            Index::All => {
                for kv in table.iter()? {
                    let (key, _) = kv?;
                    to_rm.push(key.value());
                }
            }
            Index::Index(idx) => {
                if let Some(kv) = table.iter()?.nth(*idx) {
                    to_rm.push(kv?.0.value());
                }
            }
            Index::ReIndex(idx) => {
                if let Some(kv) = table.iter()?.rev().nth(*idx) {
                    to_rm.push(kv?.0.value());
                }
            }
            Index::First => {
                if let Some(kv) = table.iter()?.next() {
                    to_rm.push(kv?.0.value());
                }
            }
            Index::Latest => {
                if let Some(kv) = table.iter()?.next_back() {
                    to_rm.push(kv?.0.value());
                }
            }
            Index::TimeStamp(timestamp) => {
                to_rm.push(*timestamp);
            }
            many => {
                let many = many.flatten();
                for idx in many {
                    idx.select_write(table, buf)?;
                }
            }
        }
        for key in to_rm {
            let mut chunks = Vec::new();
            let res = table.remove_all(key)?;
            for chunk in res {
                chunks.push(Chunk::from_slice(chunk?.value())?);
            }
            buf.push(FsFile::from(chunks));
        }
        Ok(())
    }
}

/// `Fs` 数据库交互的只读方法
///
/// # API
/// - **list_file**: 列出数据库当中的所有文件
/// - **get**: 根据文件 ID 和 `Index`，获取 `FsFile`
pub trait FsDbReadOnly {
    fn list_file(&self) -> Result<Vec<String>, FsDbError>;
    fn get<T: AsRef<str>>(&self, id: T, index: Index) -> Result<Vec<FsFile>, FsDbError>;
}

impl FsDbReadOnly for Fs {
    fn list_file(&self) -> Result<Vec<String>, FsDbError> {
        let reader = self.db.begin_read()?;
        let tables = reader.list_multimap_tables()?;
        let res = tables.map(|t| t.name()[6..].to_string()).collect();
        Ok(res)
    }
    fn get<T: AsRef<str>>(&self, id: T, index: Index) -> Result<Vec<FsFile>, FsDbError> {
        let reader = self.db.begin_read()?;
        let name = &format!("file_{}", id.as_ref());
        let definition: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new(name);
        let table = reader.open_multimap_table(definition)?;
        let mut res = Vec::new();
        index.select_read(&table, &mut res)?;
        Ok(res)
    }
}

/// `Fs` 数据库交互的写方法
///
/// 所有的交互只作用于数据库，不会删除或创建其他文件
///
/// # API
///
/// ## 删除操作
/// - **remove**: 根据文件 ID 删除文件和其下所有历史
/// - **remove_many**: 根据多个文件 ID，删除多个文件和其下所有历史
/// - **remove_history**: 根据文件 ID 和 `Index`，删除历史记录
///
/// ## 写入操作
/// - **insert**: 根据文件 ID 和 `FsFile`，插入一条记录
/// - **insert_many**: 根据文件 ID 和 `FsFile`，插入多条记录
///
/// ## 其他操作
/// - **rename**: 根据新旧文件 ID 重命名
/// - **rename_many**: 根据多个新旧文件 ID 重命名
pub trait FsDbWrite {
    fn remove<I: AsRef<str>>(&self, id: I) -> Result<bool, FsDbError>;
    fn remove_many<T, I>(&self, ids: T) -> Result<Vec<bool>, FsDbError>
    where
        T: IntoIterator<Item = I>,
        I: AsRef<str>;
    fn remove_history<I: AsRef<str>>(&self, id: I, index: Index) -> Result<Vec<FsFile>, FsDbError>;
    fn insert<I: AsRef<str>>(&self, id: I, file: FsFile) -> Result<(), FsDbError>;
    fn insert_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, FsFile)>,
        I: AsRef<str>;
    fn rename<I: AsRef<str>>(&self, old: I, new: I) -> Result<(), FsDbError>;
    fn rename_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, I)>,
        I: AsRef<str>;
}

impl FsDbWrite for Fs {
    fn remove<T: AsRef<str>>(&self, id: T) -> Result<bool, FsDbError> {
        let writer = self.db.begin_write()?;
        let name = &format!("file_{}", id.as_ref());
        let definition: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new(name);
        let mut hashes = Vec::new();
        let res = {
            let table = writer.open_multimap_table(definition)?;
            for kv in table.iter()? {
                let (_k, v) = kv?;
                for value in v {
                    let v = value?;
                    let hash = Chunk::from_slice(v.value())?.hash;
                    hashes.push(hash);
                }
            }
            writer.delete_multimap_table(definition)?
        };
        gc_min(&writer, &hashes)?;
        writer.commit()?;
        Ok(res)
    }

    fn remove_many<T, I>(&self, ids: T) -> Result<Vec<bool>, FsDbError>
    where
        T: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let writer = self.db.begin_write()?;
        let mut res = Vec::new();
        let mut hashes = Vec::new();
        for id in ids {
            let name = &format!("file_{}", id.as_ref());
            let definition: MultimapTableDefinition<u64, &[u8]> =
                MultimapTableDefinition::new(name);
            let table = writer.open_multimap_table(definition)?;
            for kv in table.iter()? {
                let (_k, v) = kv?;
                for value in v {
                    let v = value?;
                    let hash = Chunk::from_slice(v.value())?.hash;
                    hashes.push(hash);
                }
            }
            res.push(writer.delete_multimap_table(definition)?);
        }
        gc_min(&writer, &hashes)?;
        writer.commit()?;
        Ok(res)
    }
    fn remove_history<T: AsRef<str>>(&self, id: T, index: Index) -> Result<Vec<FsFile>, FsDbError> {
        let writer = self.db.begin_write()?;
        let name = &format!("file_{}", id.as_ref());
        let definition: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new(name);
        let mut res = Vec::new();
        {
            let mut table = writer.open_multimap_table(definition)?;
            index.select_write(&mut table, &mut res)?;
        }
        let hashes: Vec<Hash> = res
            .iter()
            .flat_map(|f| f.chunks.iter().map(|c| c.hash))
            .collect();
        gc_min(&writer, &hashes)?;
        writer.commit()?;
        Ok(res)
    }
    fn insert<T: AsRef<str>>(&self, id: T, file: FsFile) -> Result<(), FsDbError> {
        let writer = self.db.begin_write()?;
        let name = &format!("file_{}", id.as_ref());
        let definition: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new(name);
        {
            let mut table = writer.open_multimap_table(definition)?;
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let values: Vec<Vec<u8>> = file.chunks.iter().filter_map(|c| c.to_vec().ok()).collect();
            for value in values {
                table.insert(time, value.as_slice())?;
            }
        }
        let hashes: Vec<_> = file.chunks.iter().map(|c| c.hash).collect();
        gc_add(&writer, &hashes)?;
        writer.commit()?;
        Ok(())
    }
    fn insert_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, FsFile)>,
        I: AsRef<str>,
    {
        let mut files = Vec::new();
        let writer = self.db.begin_write()?;
        for (id, file) in item {
            files.push(file.clone());
            let name = &format!("file_{}", id.as_ref());
            let definition: MultimapTableDefinition<u64, &[u8]> =
                MultimapTableDefinition::new(name);
            {
                let mut table = writer.open_multimap_table(definition)?;
                let time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let values: Vec<Vec<u8>> =
                    file.chunks.iter().filter_map(|c| c.to_vec().ok()).collect();
                for value in values {
                    table.insert(time, value.as_slice())?;
                }
            }
        }
        let hashes: Vec<_> = files
            .into_iter()
            .flat_map(|f| f.chunks.iter().map(|c| c.hash).collect::<Vec<_>>())
            .collect();
        gc_add(&writer, &hashes)?;
        writer.commit()?;
        Ok(())
    }
    fn rename<T: AsRef<str>>(&self, old: T, new: T) -> Result<(), FsDbError> {
        let writer = self.db.begin_write()?;
        let name = &format!("file_{}", old.as_ref());
        let definition: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new(name);
        let name = &format!("file_{}", new.as_ref());
        let new_definition: MultimapTableDefinition<u64, &[u8]> =
            MultimapTableDefinition::new(name);
        writer.rename_multimap_table(definition, new_definition)?;
        writer.commit()?;
        Ok(())
    }
    fn rename_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, I)>,
        I: AsRef<str>,
    {
        let writer = self.db.begin_write()?;
        for (old, new) in item {
            let name = &format!("file_{}", old.as_ref());
            let definition: MultimapTableDefinition<u64, &[u8]> =
                MultimapTableDefinition::new(name);
            let name = &format!("file_{}", new.as_ref());
            let new_definition: MultimapTableDefinition<u64, &[u8]> =
                MultimapTableDefinition::new(name);
            writer.rename_multimap_table(definition, new_definition)?;
        }
        writer.commit()?;
        Ok(())
    }
}

/// 释放空间，会删除文件
pub trait FsGc {
    fn release(&self) -> Result<usize, FsDbError>;
}

#[derive(thiserror::Error, Debug)]
pub enum FsDbError {
    #[error("postcard error: {0}")]
    PostCardError(#[from] postcard::Error),
    #[error("redb transaction error: {0}")]
    RedbTransactionError(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    RedbTableError(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    RedbStorageError(#[from] redb::StorageError),
    #[error("io error: {0}")]
    IoError(#[from] io::Error),
    #[error("try from slice error: {0}")]
    TryFromSliceError(#[from] TryFromSliceError),
    #[error("redb commit error: {0}")]
    RedbCommitError(#[from] redb::CommitError),
}

impl FsGc for Fs {
    fn release(&self) -> Result<usize, FsDbError> {
        let writer = self.db.begin_write()?;
        let mut res = 0;
        if !self.data_path.exists() {
            fs::create_dir(&self.data_path)?;
        }
        {
            let mut to_del = Vec::new();
            let mut table = writer.open_table(GC_TABLE)?;
            for kv in table.iter()? {
                let (k, v) = kv?;
                if v.value() == 0 {
                    to_del.push(k.value().to_owned());
                    let path = self
                        .data_path
                        .join(hash2path(&Hash::from_slice(k.value())?));
                    if path.is_file() {
                        fs::remove_file(path)?;
                        res += 1;
                    }
                }
            }
            for key in to_del {
                table.remove(key.as_slice())?;
            }
        }
        writer.commit()?;
        Ok(res)
    }
}

fn gc_add(writer: &WriteTransaction, hashes: &[Hash]) -> Result<(), FsDbError> {
    let mut to_init = Vec::new();
    let mut table = writer.open_table(GC_TABLE)?;
    for hash in hashes {
        let key = hash.as_slice();
        let count = table.get_mut(key)?;
        if let Some(mut count) = count {
            count.insert(count.value() + 1)?;
        } else {
            to_init.push(key);
        }
    }
    for key in to_init {
        table.insert(key, 1)?;
    }

    Ok(())
}
fn gc_min(writer: &WriteTransaction, hashes: &[Hash]) -> Result<(), FsDbError> {
    let mut table = writer.open_table(GC_TABLE)?;
    for hash in hashes {
        let key = hash.as_slice();
        let count = table.get_mut(key)?;
        if let Some(mut count) = count {
            let n = count.value().saturating_sub(1);
            if n != 0 {
                count.insert(n)?;
            }
        }
    }
    Ok(())
}
/// 寻址，需要自己拼接根目录
pub(crate) fn hash2path(hash: &Hash) -> PathBuf {
    let hash = hash.to_string();
    PathBuf::from(&hash[0..2]).join(&hash[2..])
}
