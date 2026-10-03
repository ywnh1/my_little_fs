//! 元数据存储：redb 里的表结构、[`Index`] 选取语义，以及读写两个 trait。
//!
//! # 表结构
//!
//! - **`file_<id>`**（多值表 `u64 -> &[u8]`）：一个逻辑文件的全部历史版本。
//!   键是**版本号**（毫秒时间戳，见 [`write_version`]），值是块列表
//!   —— 每个块是 postcard 序列化后的 [`Chunk`]。
//!   同一个键下的多个值合起来才构成「一个版本」，因此键必须被某个版本独占。
//! - **`gc`**（单值表 `&[u8] -> u64`）：内容地址（blake3 原始字节）到**引用计数**的映射。
//!   每次 [`FsDbWrite::insert`] 让相关块 +1，删除历史时 -1；
//!   计数降到 0 的条目保留在表里，等 [`FsGc::release`] 时连同磁盘上的块文件一起清掉。
//!
//! [`FsDbWrite::insert`]: FsDbWrite::insert

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
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// `gc` 表：内容地址 -> 引用计数
const GC_TABLE: TableDefinition<&[u8], u64> = TableDefinition::new("gc");

/// 逻辑文件表的公共前缀，表名形如 `file_<id>`。
const FILE_TABLE_PREFIX: &str = "file_";

/// 逻辑文件 `id` 对应的表名。
///
/// 返回 `String` 而不是表定义：表定义会借用这个名字，
/// 直接返回定义会引用到一个已经析构的临时值。
fn file_table_name(id: &str) -> String {
    format!("{FILE_TABLE_PREFIX}{id}")
}

/// 用 `name` 建出逻辑文件表的定义。
///
/// `name` 必须由调用方持有：表定义借用它，不能指向临时值。
fn file_table_def(name: &str) -> MultimapTableDefinition<'_, u64, &'static [u8]> {
    MultimapTableDefinition::new(name)
}

/// 取出一个逻辑文件表的**全部版本号**，按升序排列。
fn versions(table: &MultimapTable<'_, u64, &[u8]>) -> Result<Vec<u64>, FsDbError> {
    let mut keys = Vec::new();
    for entry in table.iter()? {
        let (key, _value) = entry?;
        keys.push(key.value());
    }
    Ok(keys)
}

/// 把一个多值表的值（若干块）解码成一个 [`FsFile`]。
///
/// 读表与写表的 `MultimapValue` 是同一种类型，所以这个函数两边都能用。
/// 空的值列表会解出一个空文件，调用方需自行判断「该键压根不存在」的情况。
fn decode_chunks<'a, I>(values: I) -> Result<FsFile, FsDbError>
where
    I: IntoIterator<Item = Result<redb::AccessGuard<'a, &'static [u8]>, redb::StorageError>>,
{
    let mut chunks = Vec::new();
    for chunk in values {
        chunks.push(Chunk::from_slice(chunk?.value())?);
    }
    Ok(chunks.into())
}

/// 选取历史版本的策略。
///
/// 「第 n 个」一律按**版本号升序**（也就是时间从旧到新）计数，
/// 下标越界不会报错，只会选出 0 个版本。
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Default, Hash)]
pub enum Index {
    /// 全部版本，按时间升序
    All,
    /// 最老的版本，等价于 `Index(0)`
    First,
    /// 最新的版本，等价于 `ReIndex(0)`
    #[default]
    Latest,
    /// 指定版本号（毫秒时间戳）的那一个
    TimeStamp(u64),
    /// 正数第 n 个（0 起）
    Index(usize),
    /// 倒数第 n 个（0 起）
    ReIndex(usize),
    /// 多个策略的组合，按给定顺序拼接结果；可以嵌套
    Many(Vec<Self>),
}

impl Index {
    /// 把可能嵌套的 [`Index::Many`] 摊平成一串叶子策略。
    fn flatten(&self) -> Vec<Self> {
        if let Self::Many(v) = self {
            v.iter().flat_map(|idx| idx.flatten()).collect()
        } else {
            vec![self.clone()]
        }
    }

    /// 只读地把选中的版本追加到 `buf`。
    fn select_read(
        &self,
        table: &ReadOnlyMultimapTable<u64, &[u8]>,
        buf: &mut Vec<FsFile>,
    ) -> Result<(), FsDbError> {
        match self {
            Index::All => {
                for entry in table.iter()? {
                    let (_key, value) = entry?;
                    buf.push(decode_chunks(value)?);
                }
            }
            Index::TimeStamp(timestamp) => {
                let value = table.get(*timestamp)?;
                // 键不存在时 `get` 返回空迭代器；不能因此凭空造出一个空文件
                if !value.is_empty() {
                    buf.push(decode_chunks(value)?);
                }
            }
            Index::Many(_) => {
                for idx in self.flatten() {
                    idx.select_read(table, buf)?;
                }
            }
            // 剩下四种都是「按序数取一个版本」，只是方向与序号不同。
            // 这里穷尽列出：将来给 `Index` 加变体时，编译器会在此报错。
            Index::First => read_nth(table, true, 0, buf)?,
            Index::Index(n) => read_nth(table, true, *n, buf)?,
            Index::Latest => read_nth(table, false, 0, buf)?,
            Index::ReIndex(n) => read_nth(table, false, *n, buf)?,
        }
        Ok(())
    }

    /// 删除选中的版本，并把被删掉的版本内容追加到 `buf`（供调用方做引用计数）。
    fn select_write(
        &self,
        table: &mut MultimapTable<u64, &[u8]>,
        buf: &mut Vec<FsFile>,
    ) -> Result<(), FsDbError> {
        // 先把要删的版本号收集起来，再统一删：避免边遍历边改表
        let mut to_remove = Vec::new();
        match self {
            Index::All => to_remove.extend(versions(table)?),
            Index::TimeStamp(timestamp) => {
                // 同样要注意：键不存在时不该产出一个空文件
                if !table.get(*timestamp)?.is_empty() {
                    to_remove.push(*timestamp);
                }
            }
            Index::Many(_) => {
                for idx in self.flatten() {
                    idx.select_write(table, buf)?;
                }
            }
            Index::First => pick_nth(table, true, 0, &mut to_remove)?,
            Index::Index(n) => pick_nth(table, true, *n, &mut to_remove)?,
            Index::Latest => pick_nth(table, false, 0, &mut to_remove)?,
            Index::ReIndex(n) => pick_nth(table, false, *n, &mut to_remove)?,
        }
        for key in to_remove {
            buf.push(decode_chunks(table.remove_all(key)?)?);
        }
        Ok(())
    }
}

/// 取第 `n` 个版本（`forward` 为 false 时从最新往回数）解码后追加到 `buf`；
/// 序号越界时不追加任何内容。
fn read_nth(
    table: &ReadOnlyMultimapTable<u64, &[u8]>,
    forward: bool,
    n: usize,
    buf: &mut Vec<FsFile>,
) -> Result<(), FsDbError> {
    let mut iter = table.iter()?;
    let entry = if forward {
        iter.nth(n)
    } else {
        iter.rev().nth(n)
    };
    if let Some(entry) = entry {
        let (_key, value) = entry?;
        buf.push(decode_chunks(value)?);
    }
    Ok(())
}

/// 取第 `n` 个版本的**版本号**追加到 `keys`；序号越界时不追加。
fn pick_nth(
    table: &mut MultimapTable<u64, &[u8]>,
    forward: bool,
    n: usize,
    keys: &mut Vec<u64>,
) -> Result<(), FsDbError> {
    let mut iter = table.iter()?;
    let entry = if forward {
        iter.nth(n)
    } else {
        iter.rev().nth(n)
    };
    if let Some(entry) = entry {
        keys.push(entry?.0.value());
    }
    Ok(())
}

/// [`Fs`] 数据库的只读接口。
///
/// # API
/// - [`list_file`](FsDbReadOnly::list_file)：列出数据库里所有逻辑文件的 id
/// - [`get`](FsDbReadOnly::get)：按 id 与 [`Index`] 取出文件内容
pub trait FsDbReadOnly {
    /// 列出所有逻辑文件的 id。
    ///
    /// 顺序由 redb 的表名顺序决定，不做保证。
    fn list_file(&self) -> Result<Vec<String>, FsDbError>;

    /// 按 id 取出被 `index` 选中的版本。
    ///
    /// 返回的 [`FsFile`] 已经带上数据根目录，可以直接读出内容。
    /// id 不存在时返回 [`FsDbError::RedbTableError`]（表未创建），而不是空列表。
    fn get<T: AsRef<str>>(&self, id: T, index: Index) -> Result<Vec<FsFile>, FsDbError>;

    /// 按时间顺序列出某个 id 的全部版本，连同版本号（毫秒时间戳）。
    ///
    /// 和 [`get`](FsDbReadOnly::get) 的区别只有一点：这里把版本号也交出来，
    /// 供调用方展示「哪个版本是什么时候存的」。
    fn history<T: AsRef<str>>(&self, id: T) -> Result<Vec<(u64, FsFile)>, FsDbError>;
}

impl FsDbReadOnly for Fs {
    fn list_file(&self) -> Result<Vec<String>, FsDbError> {
        let reader = self.db.begin_read()?;
        let tables = reader.list_multimap_tables()?;
        // 用 strip_prefix 而不是硬编码切片：表名短于前缀时切片会 panic
        let res = tables
            .filter_map(|t| t.name().strip_prefix(FILE_TABLE_PREFIX).map(str::to_string))
            .collect();
        Ok(res)
    }

    fn get<T: AsRef<str>>(&self, id: T, index: Index) -> Result<Vec<FsFile>, FsDbError> {
        let reader = self.db.begin_read()?;
        let name = file_table_name(id.as_ref());
        let definition = file_table_def(&name);
        let table = reader.open_multimap_table(definition)?;
        let mut res = Vec::new();
        index.select_read(&table, &mut res)?;
        // 数据库里只有块的元数据；要把内容读出来还得知道数据根目录在哪
        for file in &mut res {
            file.data_path = Arc::new(self.data_path.clone());
        }
        Ok(res)
    }

    fn history<T: AsRef<str>>(&self, id: T) -> Result<Vec<(u64, FsFile)>, FsDbError> {
        let reader = self.db.begin_read()?;
        let name = file_table_name(id.as_ref());
        let table = reader.open_multimap_table(file_table_def(&name))?;
        let mut res = Vec::new();
        for entry in table.iter()? {
            let (version, value) = entry?;
            let mut file = decode_chunks(value)?;
            file.data_path = Arc::new(self.data_path.clone());
            res.push((version.value(), file));
        }
        Ok(res)
    }
}

/// [`Fs`] 数据库的写入接口。
///
/// 这一层只动数据库，**不创建也不删除任何数据文件**
/// （唯一的例外是 [`FsGc::release`]）。
///
/// # API
///
/// ## 删除操作
/// - [`remove`](FsDbWrite::remove)：按 id 删除整个逻辑文件及其全部历史
/// - [`remove_many`](FsDbWrite::remove_many)：批量版本，返回每个 id 是否存在
/// - [`remove_history`](FsDbWrite::remove_history)：按 id 与 [`Index`] 删除部分历史
///
/// ## 写入操作
/// - [`insert`](FsDbWrite::insert)：插入一个新版本
/// - [`insert_many`](FsDbWrite::insert_many)：批量插入
///
/// ## 其他操作
/// - [`rename`](FsDbWrite::rename)：重命名逻辑文件
/// - [`rename_many`](FsDbWrite::rename_many)：批量重命名
pub trait FsDbWrite {
    /// 删除整个逻辑文件，返回它是否存在过。
    fn remove<I: AsRef<str>>(&self, id: I) -> Result<bool, FsDbError>;

    /// 批量删除，返回每个 id 是否存在过。
    fn remove_many<T, I>(&self, ids: T) -> Result<Vec<bool>, FsDbError>
    where
        T: IntoIterator<Item = I>,
        I: AsRef<str>;

    /// 删除部分历史，返回被删掉的那些版本。
    fn remove_history<I: AsRef<str>>(&self, id: I, index: Index) -> Result<Vec<FsFile>, FsDbError>;

    /// 为 `id` 追加一个新版本。
    fn insert<I: AsRef<str>>(&self, id: I, file: FsFile) -> Result<(), FsDbError>;

    /// 批量追加新版本，所有元数据在同一次事务里提交。
    fn insert_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, FsFile)>,
        I: AsRef<str>;

    /// 重命名为 `new`，原有的历史一并带过去。
    fn rename<I: AsRef<str>>(&self, old: I, new: I) -> Result<(), FsDbError>;

    /// 批量重命名。
    fn rename_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, I)>,
        I: AsRef<str>;
}

/// 逻辑文件表是否已经存在。
///
/// 必须显式判断：redb 的 `open_multimap_table` 会在表不存在时顺手创建它，
/// 因此不能靠「打开成功」或 `delete_multimap_table` 的返回值来判断是否存在过。
fn table_exists(writer: &WriteTransaction, name: &str) -> Result<bool, FsDbError> {
    Ok(writer.list_multimap_tables()?.any(|t| t.name() == name))
}

/// 取当前毫秒时间戳。系统时钟早于 1970 时按 0 处理，不 panic。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把 `chunks` 作为一个**独占**版本写进表里，返回所用的版本号。
///
/// 版本号取自当前毫秒时间戳；若该键已被占用（同一毫秒内连续写了多个版本），
/// 就向后顺延 1ms —— 一个键下的所有值会被读成同一个文件，
/// 因此键绝不能被两个版本共用。
fn write_version(
    table: &mut MultimapTable<u64, &[u8]>,
    chunks: &[Chunk],
) -> Result<u64, FsDbError> {
    let mut version = now_millis();
    while !table.get(version)?.is_empty() {
        version += 1;
    }
    for chunk in chunks {
        let bytes = chunk.to_vec()?;
        table.insert(version, bytes.as_slice())?;
    }
    Ok(version)
}

impl FsDbWrite for Fs {
    fn remove<T: AsRef<str>>(&self, id: T) -> Result<bool, FsDbError> {
        let writer = self.db.begin_write()?;
        let name = file_table_name(id.as_ref());
        let mut hashes = Vec::new();
        let existed = if table_exists(&writer, &name)? {
            // 读引用计数用的 hash：`table` 必须在删除前离开作用域，
            // 否则 redb 会以 TableAlreadyOpen 拒绝删除
            {
                let table = writer.open_multimap_table(file_table_def(&name))?;
                for entry in table.iter()? {
                    let (_version, value) = entry?;
                    for chunk in value {
                        hashes.push(Chunk::from_slice(chunk?.value())?.hash);
                    }
                }
            }
            writer.delete_multimap_table(file_table_def(&name))?
        } else {
            false
        };
        gc_min(&writer, &hashes)?;
        writer.commit()?;
        Ok(existed)
    }

    fn remove_many<T, I>(&self, ids: T) -> Result<Vec<bool>, FsDbError>
    where
        T: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let writer = self.db.begin_write()?;
        let mut existed = Vec::new();
        let mut hashes = Vec::new();
        for id in ids {
            let name = file_table_name(id.as_ref());
            if !table_exists(&writer, &name)? {
                existed.push(false);
                continue;
            }
            {
                let table = writer.open_multimap_table(file_table_def(&name))?;
                for entry in table.iter()? {
                    let (_version, value) = entry?;
                    for chunk in value {
                        hashes.push(Chunk::from_slice(chunk?.value())?.hash);
                    }
                }
            }
            existed.push(writer.delete_multimap_table(file_table_def(&name))?);
        }
        gc_min(&writer, &hashes)?;
        writer.commit()?;
        Ok(existed)
    }

    fn remove_history<T: AsRef<str>>(&self, id: T, index: Index) -> Result<Vec<FsFile>, FsDbError> {
        let writer = self.db.begin_write()?;
        let name = file_table_name(id.as_ref());
        let mut removed = Vec::new();
        // 表不存在时直接返回空：若先 open，redb 会顺手新建一张空表，
        // 这个空 id 就会凭空出现在 `list_file` 里
        if table_exists(&writer, &name)? {
            {
                let mut table = writer.open_multimap_table(file_table_def(&name))?;
                index.select_write(&mut table, &mut removed)?;
            }
            let hashes: Vec<Hash> = removed
                .iter()
                .flat_map(|f| f.chunks.iter().map(|c| c.hash))
                .collect();
            gc_min(&writer, &hashes)?;
        }
        writer.commit()?;
        Ok(removed)
    }

    fn insert<T: AsRef<str>>(&self, id: T, file: FsFile) -> Result<(), FsDbError> {
        let writer = self.db.begin_write()?;
        {
            let name = file_table_name(id.as_ref());
            let mut table = writer.open_multimap_table(file_table_def(&name))?;
            write_version(&mut table, &file.chunks)?;
        }
        let hashes: Vec<Hash> = file.chunks.iter().map(|c| c.hash).collect();
        gc_add(&writer, &hashes)?;
        writer.commit()?;
        Ok(())
    }

    fn insert_many<I, T>(&self, item: T) -> Result<(), FsDbError>
    where
        T: IntoIterator<Item = (I, FsFile)>,
        I: AsRef<str>,
    {
        let writer = self.db.begin_write()?;
        let mut hashes = Vec::new();
        for (id, file) in item {
            {
                let name = file_table_name(id.as_ref());
                let mut table = writer.open_multimap_table(file_table_def(&name))?;
                write_version(&mut table, &file.chunks)?;
            }
            // 直接在原文件上收集，省掉一次 clone
            hashes.extend(file.chunks.iter().map(|c| c.hash));
        }
        gc_add(&writer, &hashes)?;
        writer.commit()?;
        Ok(())
    }

    fn rename<T: AsRef<str>>(&self, old: T, new: T) -> Result<(), FsDbError> {
        let writer = self.db.begin_write()?;
        let old_name = file_table_name(old.as_ref());
        let new_name = file_table_name(new.as_ref());
        writer.rename_multimap_table(file_table_def(&old_name), file_table_def(&new_name))?;
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
            let old_name = file_table_name(old.as_ref());
            let new_name = file_table_name(new.as_ref());
            writer.rename_multimap_table(file_table_def(&old_name), file_table_def(&new_name))?;
        }
        writer.commit()?;
        Ok(())
    }
}

/// 垃圾回收：把引用计数归零的块从磁盘上真正删掉。
pub trait FsGc {
    /// 删除所有引用计数为 0 的块文件，返回删掉的文件个数。
    ///
    /// 需要单独调用的原因：删除历史只会把计数减到 0，并不会立刻动磁盘
    /// —— 这样反复删了又加的场景不会反复做无用的 IO。
    ///
    /// 注意：引用计数只在 `insert` / `remove*` 时维护。若一次导入在写入块数据之后、
    /// 提交数据库之前失败，那些块从未被计数，**不会**被这里回收。
    fn release(&self) -> Result<usize, FsDbError>;

    /// 数一下引用计数已经归零、下次 [`release`](FsGc::release) 会清掉的块：
    /// 返回 `(块数, 总字节数)`。**不会改动任何东西**，供 `--dry-run` 一类场景使用。
    fn garbage(&self) -> Result<(usize, u64), FsDbError>;
}

/// 数据库层的错误。
#[derive(Debug, thiserror::Error)]
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
    fn garbage(&self) -> Result<(usize, u64), FsDbError> {
        let reader = self.db.begin_read()?;
        // 从没写过东西时 gc 表还不存在，那本来就没有垃圾
        let Ok(table) = reader.open_table(GC_TABLE) else {
            return Ok((0, 0));
        };
        let mut blocks = 0;
        let mut bytes = 0u64;
        for entry in table.iter()? {
            let (key, count) = entry?;
            if count.value() != 0 {
                continue;
            }
            blocks += 1;
            let path = self
                .data_path
                .join(hash2path(&Hash::from_slice(key.value())?));
            // 文件可能压根没写成功，那时按 0 字节算
            bytes += path.metadata().map(|m| m.len()).unwrap_or(0);
        }
        Ok((blocks, bytes))
    }

    fn release(&self) -> Result<usize, FsDbError> {
        let writer = self.db.begin_write()?;
        let mut removed = 0;
        if !self.data_path.exists() {
            fs::create_dir_all(&self.data_path)?;
        }
        {
            let mut pending = Vec::new();
            let mut table = writer.open_table(GC_TABLE)?;
            for entry in table.iter()? {
                let (key, count) = entry?;
                if count.value() == 0 {
                    pending.push(key.value().to_owned());
                    let path = self
                        .data_path
                        .join(hash2path(&Hash::from_slice(key.value())?));
                    if path.is_file() {
                        fs::remove_file(path)?;
                        removed += 1;
                    }
                }
            }
            // 收集完再删，避免边遍历边改表
            for key in pending {
                table.remove(key.as_slice())?;
            }
        }
        writer.commit()?;
        Ok(removed)
    }
}

/// 为 `hashes` 里的每个内容地址把引用计数 +1。
fn gc_add(writer: &WriteTransaction, hashes: &[Hash]) -> Result<(), FsDbError> {
    let mut to_init = Vec::new();
    let mut table = writer.open_table(GC_TABLE)?;
    for hash in hashes {
        let key = hash.as_slice();
        if let Some(mut count) = table.get_mut(key)? {
            count.insert(count.value() + 1)?;
        } else {
            to_init.push(key);
        }
    }
    // 收集完再插入，避免借用冲突
    for key in to_init {
        table.insert(key, 1)?;
    }
    Ok(())
}

/// 为 `hashes` 里的每个内容地址把引用计数 -1（已经为 0 的保持 0）。
///
/// 计数降到 0 的条目会**留在表里**并记成 0，交给 [`FsGc::release`] 清理。
/// 表里没有的地址直接跳过。
///
/// 注意必须把 0 真的写回表里：`release` 正是靠「读到 0」来决定删哪些文件的，
/// 若在这里跳过写入，计数就会永远停在 1，块文件再也回收不掉。
fn gc_min(writer: &WriteTransaction, hashes: &[Hash]) -> Result<(), FsDbError> {
    let mut table = writer.open_table(GC_TABLE)?;
    for hash in hashes {
        let key = hash.as_slice();
        if let Some(mut count) = table.get_mut(key)? {
            let n = count.value().saturating_sub(1);
            count.insert(n)?;
        }
    }
    Ok(())
}

/// 把内容地址映射成数据目录下的**相对**路径：`<hex 前 2 位>/<hex 其余 62 位>`。
///
/// 用前 2 位做一级子目录，避免单个目录下堆积过多文件。
/// 返回值不含数据根目录，调用方需自行拼接（写入见 `Fs::file_processing`，
/// 读取见 `Chunk::read`）。
pub(crate) fn hash2path(hash: &Hash) -> PathBuf {
    let hash = hash.to_string();
    PathBuf::from(&hash[0..2]).join(&hash[2..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::FsDbReadOnly;

    /// 建一个空的 `Fs`，返回临时目录（必须持有，否则会被删掉）。
    fn fixture() -> (tempfile::TempDir, Fs) {
        let dir = tempfile::tempdir().unwrap();
        let fs = Fs::builder(dir.path().to_path_buf()).build().unwrap();
        (dir, fs)
    }

    /// 用若干段字节造一个块列表。块数据**不落盘**，只用到元数据。
    fn file_of(parts: &[&[u8]]) -> FsFile {
        let mut offset = 0u64;
        let chunks: Vec<Chunk> = parts
            .iter()
            .map(|part| {
                let chunk = Chunk {
                    hash: blake3::hash(part),
                    size: part.len(),
                    offset,
                };
                offset += part.len() as u64;
                chunk
            })
            .collect();
        FsFile::from(chunks)
    }

    /// 把一块数据真实地摆到数据目录里（`release` 的测试需要）。
    fn place(fs: &Fs, data: &[u8]) -> Chunk {
        let chunk = Chunk {
            hash: blake3::hash(data),
            size: data.len(),
            offset: 0,
        };
        let path = fs.data_path.join(chunk.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 走正规编码：磁盘上的块是「标签 + 载荷」，裸字节会被当成标签
        let blob = crate::codec::encode(data.to_vec(), None).unwrap();
        std::fs::write(path, blob).unwrap();
        chunk
    }

    #[test]
    fn insert_then_get_returns_the_same_chunks() {
        let (_dir, fs) = fixture();
        let file = file_of(&[b"hello ", b"world"]);
        fs.insert("doc", file.clone()).unwrap();

        let got = fs.get("doc", Index::Latest).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].chunks, file.chunks);
    }

    #[test]
    fn repeated_inserts_accumulate_history_in_time_order() {
        let (_dir, fs) = fixture();
        let v1 = file_of(&[b"version one"]);
        let v2 = file_of(&[b"version two"]);
        let v3 = file_of(&[b"version three"]);
        fs.insert("doc", v1.clone()).unwrap();
        fs.insert("doc", v2.clone()).unwrap();
        fs.insert("doc", v3.clone()).unwrap();

        // All 按时间升序
        let all = fs.get("doc", Index::All).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].chunks, v1.chunks);
        assert_eq!(all[1].chunks, v2.chunks);
        assert_eq!(all[2].chunks, v3.chunks);

        // First/Latest 与 Index(0)/ReIndex(0) 一致
        assert_eq!(fs.get("doc", Index::First).unwrap()[0].chunks, v1.chunks);
        assert_eq!(fs.get("doc", Index::Latest).unwrap()[0].chunks, v3.chunks);
        assert_eq!(fs.get("doc", Index::Index(1)).unwrap()[0].chunks, v2.chunks);
        assert_eq!(
            fs.get("doc", Index::ReIndex(1)).unwrap()[0].chunks,
            v2.chunks
        );
    }

    #[test]
    fn versions_written_in_the_same_millisecond_stay_separate() {
        // 回归点：版本号以毫秒为键，若不去重，同一毫秒的两个版本会被读成一个文件
        let (_dir, fs) = fixture();
        let a = file_of(&[b"AAAA"]);
        let b = file_of(&[b"BBBB"]);
        fs.insert("doc", a.clone()).unwrap();
        fs.insert("doc", b.clone()).unwrap();

        let all = fs.get("doc", Index::All).unwrap();
        assert_eq!(all.len(), 2, "同一毫秒内写入的两个版本必须各占一个版本号");
        assert_eq!(all[0].chunks, a.chunks);
        assert_eq!(all[1].chunks, b.chunks);
    }

    #[test]
    fn out_of_range_indices_select_nothing() {
        let (_dir, fs) = fixture();
        fs.insert("doc", file_of(&[b"x"])).unwrap();
        assert!(fs.get("doc", Index::Index(5)).unwrap().is_empty());
        assert!(fs.get("doc", Index::ReIndex(5)).unwrap().is_empty());
    }

    #[test]
    fn unknown_timestamp_selects_nothing_instead_of_an_empty_file() {
        // 回归点：查一个不存在的版本号，曾经会返回一个「空文件」
        let (_dir, fs) = fixture();
        fs.insert("doc", file_of(&[b"x"])).unwrap();
        assert!(fs.get("doc", Index::TimeStamp(1)).unwrap().is_empty());
        assert!(
            fs.remove_history("doc", Index::TimeStamp(1))
                .unwrap()
                .is_empty()
        );
        // 真版本还在
        assert_eq!(fs.get("doc", Index::All).unwrap().len(), 1);
    }

    #[test]
    fn many_index_concatenates_in_given_order() {
        let (_dir, fs) = fixture();
        let v1 = file_of(&[b"one"]);
        let v2 = file_of(&[b"two"]);
        fs.insert("doc", v1.clone()).unwrap();
        fs.insert("doc", v2.clone()).unwrap();

        let got = fs
            .get("doc", Index::Many(vec![Index::Latest, Index::First]))
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].chunks, v2.chunks);
        assert_eq!(got[1].chunks, v1.chunks);
    }

    #[test]
    fn get_on_unknown_id_is_an_error_not_an_empty_list() {
        let (_dir, fs) = fixture();
        assert!(fs.get("nobody", Index::Latest).is_err());
    }

    #[test]
    fn list_file_returns_every_id_without_the_table_prefix() {
        let (_dir, fs) = fixture();
        fs.insert("alpha", file_of(&[b"a"])).unwrap();
        fs.insert("beta", file_of(&[b"b"])).unwrap();
        // 触发 gc 表的创建，确保它不会混进结果里
        fs.insert("gamma", file_of(&[b"c"])).unwrap();

        let mut ids = fs.list_file().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["alpha", "beta", "gamma"]);
        assert!(
            !ids.iter().any(|id| id.starts_with("file_") || id == "gc"),
            "表前缀与 gc 表都不应出现在 id 列表里"
        );
    }

    #[test]
    fn list_file_on_empty_db_is_empty() {
        let (_dir, fs) = fixture();
        assert!(fs.list_file().unwrap().is_empty());
    }

    #[test]
    fn remove_drops_the_whole_file_and_reports_existence() {
        let (_dir, fs) = fixture();
        fs.insert("doc", file_of(&[b"x"])).unwrap();
        assert!(fs.remove("doc").unwrap(), "已存在的 id 应返回 true");
        assert!(!fs.remove("doc").unwrap(), "重复删除应返回 false");
        assert!(fs.list_file().unwrap().is_empty());
    }

    #[test]
    fn remove_many_handles_missing_ids() {
        let (_dir, fs) = fixture();
        fs.insert("a", file_of(&[b"a"])).unwrap();
        fs.insert("b", file_of(&[b"b"])).unwrap();
        let res = fs.remove_many(["a", "ghost", "b"]).unwrap();
        assert_eq!(res, vec![true, false, true]);
        assert!(fs.list_file().unwrap().is_empty());
    }

    #[test]
    fn remove_history_only_drops_the_selected_version() {
        let (_dir, fs) = fixture();
        let v1 = file_of(&[b"keep"]);
        let v2 = file_of(&[b"drop"]);
        fs.insert("doc", v1.clone()).unwrap();
        fs.insert("doc", v2.clone()).unwrap();

        let removed = fs.remove_history("doc", Index::Latest).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].chunks, v2.chunks);

        let left = fs.get("doc", Index::All).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].chunks, v1.chunks);
    }

    #[test]
    fn rename_carries_the_history_over() {
        let (_dir, fs) = fixture();
        fs.insert("before", file_of(&[b"payload"])).unwrap();
        fs.rename("before", "after").unwrap();

        let mut ids = fs.list_file().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["after"]);
        assert!(fs.get("before", Index::Latest).is_err());
        assert_eq!(fs.get("after", Index::Latest).unwrap().len(), 1);
    }

    #[test]
    fn rename_many_carries_all_histories_over() {
        let (_dir, fs) = fixture();
        fs.insert("a", file_of(&[b"a"])).unwrap();
        fs.insert("b", file_of(&[b"b"])).unwrap();
        fs.rename_many([("a", "x"), ("b", "y")]).unwrap();

        let mut ids = fs.list_file().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["x", "y"]);
    }

    #[test]
    fn insert_many_commits_every_id_in_one_go() {
        let (_dir, fs) = fixture();
        let v1 = file_of(&[b"first"]);
        let v2 = file_of(&[b"second"]);
        fs.insert_many([("a", v1.clone()), ("b", v2.clone())])
            .unwrap();

        assert_eq!(fs.get("a", Index::Latest).unwrap()[0].chunks, v1.chunks);
        assert_eq!(fs.get("b", Index::Latest).unwrap()[0].chunks, v2.chunks);
    }

    #[test]
    fn gc_counts_references_across_ids_and_release_deletes_the_payload() {
        let (_dir, fs) = fixture();
        // 两个逻辑文件共用同一块内容
        let shared = place(&fs, b"shared payload");
        let blob = fs.data_path.join(shared.path());
        assert!(blob.is_file());

        let one = FsFile::from(vec![shared]);
        let two = FsFile::from(vec![shared]);
        fs.insert("one", one).unwrap();
        fs.insert("two", two).unwrap();

        // 删掉其中一个：块仍被另一个引用，不能回收
        fs.remove("one").unwrap();
        assert_eq!(fs.release().unwrap(), 0, "还有引用时不应删除块文件");
        assert!(blob.is_file());

        // 再删掉最后一个引用：这时才真正回收
        fs.remove("two").unwrap();
        assert_eq!(fs.release().unwrap(), 1, "引用归零后应删除块文件");
        assert!(!blob.exists(), "块文件应当已被删除");
    }

    #[test]
    fn release_leaves_orphan_blocks_alone_and_is_idempotent() {
        let (_dir, fs) = fixture();
        // 从未被任何元数据引用过的块（例如导入中途失败留下的）
        let orphan = place(&fs, b"orphan payload");
        let blob = fs.data_path.join(orphan.path());

        // 它不在 gc 表里，release 不会碰它 —— 这是当前实现的已知行为
        assert_eq!(fs.release().unwrap(), 0);
        assert!(blob.is_file());
        // 反复 release 是安全的
        assert_eq!(fs.release().unwrap(), 0);
        assert!(blob.is_file());
    }

    #[test]
    fn release_works_when_the_data_directory_does_not_exist() {
        let (_dir, fs) = fixture();
        assert!(!fs.data_path.exists());
        assert_eq!(fs.release().unwrap(), 0);
    }

    #[test]
    fn history_returns_every_version_with_its_stamp_in_order() {
        let (_dir, fs) = fixture();
        let v1 = file_of(&[b"one"]);
        let v2 = file_of(&[b"two"]);
        let v3 = file_of(&[b"three"]);
        fs.insert("doc", v1.clone()).unwrap();
        fs.insert("doc", v2.clone()).unwrap();
        fs.insert("doc", v3.clone()).unwrap();

        let versions = fs.history("doc").unwrap();
        assert_eq!(versions.len(), 3);
        // 版本号必须递增，调用方才能拿它当「时间顺序」用
        assert!(versions[0].0 < versions[1].0 && versions[1].0 < versions[2].0);
        assert_eq!(versions[0].1.chunks, v1.chunks);
        assert_eq!(versions[1].1.chunks, v2.chunks);
        assert_eq!(versions[2].1.chunks, v3.chunks);
    }

    #[test]
    fn history_of_an_unknown_id_is_an_error() {
        let (_dir, fs) = fixture();
        assert!(fs.history("nobody").is_err());
    }

    #[test]
    fn garbage_counts_unreferenced_blocks_without_deleting_them() {
        let (_dir, fs) = fixture();
        // 从没写过东西时 gc 表还不存在，本来就没有垃圾
        assert_eq!(fs.garbage().unwrap(), (0, 0));

        let payload = place(&fs, b"payload to be orphaned");
        let blob = fs.data_path.join(payload.path());
        fs.insert("doc", FsFile::from(vec![payload])).unwrap();
        assert_eq!(fs.garbage().unwrap(), (0, 0), "还有引用时不该算作垃圾");

        fs.remove("doc").unwrap();
        let (blocks, bytes) = fs.garbage().unwrap();
        assert_eq!(blocks, 1);
        assert_eq!(bytes, blob.metadata().unwrap().len());
        assert!(blob.is_file(), "garbage 只负责数，不能真的动手删");
    }

    #[test]
    fn hash2path_shards_by_the_first_byte_of_the_hex_digest() {
        let hash = blake3::hash(b"whatever");
        let path = hash2path(&hash);
        let hex = hash.to_string();
        assert_eq!(path.parent().unwrap().to_str().unwrap(), &hex[0..2]);
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), &hex[2..]);
        assert_eq!(hex.len(), 64);
    }
}
