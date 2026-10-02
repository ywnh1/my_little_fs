# my_little_fs

一个用 Rust 写的内容寻址文件系统。文件按内容切块、用 blake3 摘要寻址，相同的内容在磁盘上只存一份。

## 它是怎么工作的

一个大文件被切成若干小块，每块的「名字」就是它内容的 blake3 哈希。地址由内容决定，所以：

- 同样的内容天然只存一份，不需要额外查重
- 两个文件哪怕只共享一小段，那一段也只占一份空间
- 切分点由内容本身决定（内容定义分块），而不是按固定长度切 —— 在文件开头插入几个字节，不会让后面所有块全部错位、全部需要重存

元数据放在 redb 里，记录「某个 id 的某个历史版本由哪些块按什么顺序组成」。

## 特性

- **内容定义分块**：fastcdc v2020，分块粒度可调
- **自动去重**：块按内容寻址，重复内容不重复落盘
- **可选压缩**：zstd，压不动的内容就存原始字节，两种形态可以共存于同一目录
- **历史版本**：同一个 id 每次导入追加一个版本，可按位置或时间戳取回
- **引用计数 + 显式回收**：删除版本只减计数，真正清磁盘由 `release()` 统一执行
- **随机访问**：`FsFile` 实现了 `Read` 和 `Seek`，可以不落盘直接按偏移读取

## 快速开始

```toml
[dependencies]
my_little_fs = { git = "https://github.com/ywnh1/my_little_fs" }
```

```rust
use my_little_fs::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 打开（不存在则创建）一个文件系统
    let fs = Fs::builder("/tmp/my-fs".into()).build()?;

    // 导入：把真实文件切块存进去，登记为 id "album/photo"
    fs.copy_in("/tmp/photo.png", "album/photo")?;

    // 拿出最新版本，导出成真实文件
    let file = fs.get("album/photo", Index::Latest)?.remove(0);
    fs.copy_out("/tmp/restored.png", file)?;

    Ok(())
}
```

> `copy_in`、`copy_out`、`get`、`remove` 这些方法都定义在 trait 上，
> 记得 `use my_little_fs::prelude::*;`，否则方法找不到。

### 调整分块与压缩

```rust
use my_little_fs::prelude::*;

let fs = Fs::builder("/tmp/my-fs".into())
    .with_min_size(Some(16 * 1024))
    .with_avg_size(Some(64 * 1024))
    .with_max_size(Some(256 * 1024))
    .with_compress(Some(3))          // zstd 级别，不设则完全不压缩
    .build()?;
```

三个 `*_size` 只填一部分也可以，没填的会按 1:4:16 推导；一个都不填用 fastcdc 推荐的 16 KiB / 32 KiB / 64 KiB。注意这几个 setter 接收的是 `Option`，所以要写 `Some(..)`。

## 磁盘布局

```
<root>/
├── db.redb                        # 元数据：版本历史 + 引用计数
└── data/
    └── ab/
        └── cdef0123...            # 块实体，文件名是内容的 blake3 hex
```

块路径 = `<数据目录>/<哈希 hex 的前 2 位>/<哈希 hex 的其余 62 位>`，用前 2 位做一层分片目录，避免单个目录里堆几十万个文件。

数据库里有两张表：

| 表 | 结构 | 用途 |
| --- | --- | --- |
| `file_<id>` | 多值表 `u64 -> &[u8]` | 键是版本号（毫秒时间戳），值是块列表（postcard 序列化的 `Chunk`） |
| `gc` | 单值表 `&[u8] -> u64` | 内容地址 → 引用计数 |

## 模块

| 模块 | 职责 |
| --- | --- |
| [`fs`](src/fs.rs) | 门面。[`FsBuilder`](src/fs.rs) 负责打开与配置，`Fs` 负责分块落盘 |
| [`db`](src/db.rs) | 元数据。读写 trait、`Index` 选取语义、引用计数与回收 |
| [`chunk`](src/chunk.rs) | 块的元数据与磁盘布局，读取时自动解压 |
| [`file`](src/file.rs) | 逻辑文件 `FsFile`，实现 `Read` + `Seek` |
| [`io`](src/io.rs) | 与外界真实文件的双向复制 |

## API 一览

导入导出：

- `copy_in(path, id)` —— 导入一个文件
- `copy_in_many([(id, path), ..])` —— 批量导入，元数据一次提交
- `copy_out(path, file)` —— 导出到真实文件

读取：

- `list_file()` —— 列出所有 id
- `get(id, index)` —— 按 `Index` 取出文件内容

`Index` 的取值：

| 变体 | 含义 |
| --- | --- |
| `All` | 全部版本，按时间升序 |
| `First` | 最老的一个，等价 `Index(0)` |
| `Latest` | 最新的一个，等价 `ReIndex(0)`（默认值） |
| `TimeStamp(u64)` | 指定版本号（毫秒时间戳） |
| `Index(usize)` | 正数第 n 个 |
| `ReIndex(usize)` | 倒数第 n 个 |
| `Many(Vec<Index>)` | 组合若干策略，按给定顺序拼接结果 |

下标越界返回空的 `Vec`，不报错；但 `get` 一个从不存在的 id 会返回错误（表都没建）。

写入与删除：

- `insert(id, file)` / `insert_many([(id, file), ..])` —— 直接插入一个版本
- `remove(id)` / `remove_many([ids])` —— 删掉整个 id 及其全部历史，返回它是否存在过
- `remove_history(id, index)` —— 只删掉指定的版本，返回被删的那些
- `rename(old, new)` / `rename_many([(old, new)])` —— 重命名，历史一并带走

回收：

- `release()` —— 删除所有引用计数为 0 的块文件，返回删掉的文件数

删除历史只会把引用计数减到 0，不会立刻动磁盘。这样「删了又加」的场景不会反复做无用 IO，磁盘清理由你在合适的时机调一次 `release()` 完成。

### 例子：多版本与回收

```rust
use my_little_fs::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fs = Fs::builder("/tmp/my-fs".into()).build()?;
    fs.copy_in("/tmp/draft-1.txt", "notes")?;
    fs.copy_in("/tmp/draft-2.txt", "notes")?;

    // 两个版本都在
    assert_eq!(fs.get("notes", Index::All)?.len(), 2);

    // 回滚到第一版
    let original = fs.get("notes", Index::First)?.remove(0);
    drop(original);

    // 丢掉最新版并回收空间
    fs.remove_history("notes", Index::Latest)?;
    fs.release()?;
    Ok(())
}
```

## 测试

```sh
cargo test
```

55 个测试：38 个单元测试（分块参数推导、块的压缩嗅探、`FsFile` 的定位/寻址/跨块读取、数据库的各种 `Index` 语义与引用计数），16 个端到端测试（走完整的导入导出路径，覆盖多块大文件、去重、历史回滚、压缩与未压缩块共存、跨进程重开根目录），外加 1 个文档测试。

## 设计上的取舍

写清楚边界，用的时候好判断：

- **一个根目录同时只能被一个 `Fs` 打开**。redb 会对数据库文件加锁，重复打开会返回 `DatabaseAlreadyOpen`，要并发访问先 `drop` 掉前一个实例。
- **写块之后没有 `fsync`**。块数据可能还在页缓存里，而数据库引用已经提交，断电存在指向空文件的记录的风险。逐块 `fsync` 在手机上代价过高，建议批量导入结束后自行同步目录。
- **导入中途失败会留下孤儿块**。这些块从未被记入引用计数，`release()` 不会回收它们；不过下次导入同样内容时会被直接复用，不会重复写入。
- **版本号是毫秒时间戳**。同一毫秒内连续写入多个版本时，版本号会向后顺延，保证每个版本独占一个键 —— 否则同一毫秒的两个版本会被读成同一个文件。
- **空文件用一个长度为 0 的哨兵块表示**。因为数据库里「一个版本」就是「一个键下的若干值」，一个值都没有等于版本不存在，空文件会连同 id 一起消失。
- **`FsFile` 的 `PartialEq` 把读取缓存也算在内**。两个内容相同、只是缓存命中情况不同的 `FsFile` 会被判为不相等；要比内容请比 `get_size()` 和读出的字节。
- **没有加密，没有访问控制**。地址是内容哈希，拿到数据文件就能读出内容。

## 环境

Rust edition 2024。

## 许可

尚未添加 LICENSE 文件。
