# mlfs

[my_little_fs](../) 的命令行界面：把文件按内容切块、用 blake3 寻址存储，相同内容只存一份，并保留每一次导入的历史版本。

底层是同一个库（[根目录的 README](../README.md) 讲的是它怎么工作），这里只讲怎么用。

## 安装

```sh
cargo install --path crates/mlfs     # 从源码装到 ~/.cargo/bin
cargo build -p mlfs --release        # 或者只构建，产物在 target/release/mlfs
```

## 快速开始

```sh
# 导入：id 默认取文件的绝对路径
mlfs import ./photo.jpg

# 看现在都有什么
mlfs ls

# 原样读回来
mlfs cat /home/u/photo.jpg > copy.jpg

# 导出成文件
mlfs export /home/u/photo.jpg ./restored.jpg

# 整个目录，遵守 .gitignore
mlfs import -R ./photos

# 删掉并回收磁盘
mlfs rm -y /home/u/photo.jpg
mlfs gc
```

所有命令都可以用 `mlfs help <命令>` 看它自己的选项，例如 `mlfs help import`。

## 全局选项

下面这些所有命令都能用，写在命令前面或后面都行（`-v` 可以重复，越多次越啰嗦）。

| 选项 | 说明 |
| --- | --- |
| `-r`, `--root <DIR>` | fs 根目录，覆盖配置与环境变量 |
| `-c`, `--config <FILE>` | 指定配置文件，取代默认位置 |
| `--json` | 列表类命令输出 JSON |
| `--no-progress` | 不显示进度条 |
| `-q`, `--quiet` | 只输出错误 |
| `-v`, `--verbose` | 多输出过程信息，可重复 |
| `-h`, `--help` | 帮助；`mlfs help <命令>` 看某个命令的选项 |
| `-V`, `--version` | 版本号 |

`--json` 时进度条会自动关掉 —— 它往标准错误写，本来也不会污染管道，
但输出给机器看的时候没必要再刷屏。

## 命令

### import

把文件或目录导入 fs。

```
mlfs import <PATH>... [选项]
```

多个路径可以一次给，也可以一次给同一个文件多次（会变成多个版本）。

| 选项 | 说明 |
| --- | --- |
| `-R`, `--recursive` | 递归导入目录。**不给它就不能导入目录** |
| `--id <ID>` | 指定 id。只能配单个输入，不能和 `-R` 同用 |
| `--codec <NAME>` | 本次导入用的压缩后端，覆盖配置：`none` / `zstd` / `gzip` / `brotli` / `lz4` / `snappy` |
| `--level <N>` | 本次导入的压缩级别，覆盖配置 |
| `--hidden` | 递归时连隐藏文件一起导入 |
| `--follow-links` | 递归时跟随符号链接 |
| `--no-ignore` | 递归时不理会 `.gitignore` / `.ignore` |

```sh
mlfs import -R ./photos                    # 目录里每个文件各记在自己的绝对路径上
mlfs import ./notes.txt --id notes/today   # 起个别名
mlfs import -R ./logs --codec brotli --level 9
```

递归用的是 [`ignore`](https://crates.io/crates/ignore) 那套规则，所以 `.gitignore`、`.ignore`、隐藏文件、符号链接的行为和 `ripgrep` 是一致的。

### export

把某个 id 的内容写到文件。

```
mlfs export <ID> <PATH> [选项]
```

| 选项 | 说明 |
| --- | --- |
| `--first` | 最老的版本 |
| `--latest` | 最新的版本（默认） |
| `--index <N>` | 正数第 N 个，从 0 开始 |
| `--reindex <N>` | 倒数第 N 个，从 0 开始 |
| `--timestamp <MS>` | 指定版本号（毫秒时间戳） |
| `--all` | 这个命令里会被拒绝：`export` 一次只能导出一个版本，硬要挑一个会让你拿到不是自己要的东西 |
| `--overwrite <MODE>` | 目标已存在时怎么办，见下一节 |

目标路径的父目录不存在时会按需创建。

### cat

把某个 id 的内容写到标准输出，版本选项和 `export` 一样。

```sh
mlfs cat /home/u/photo.jpg > copy.jpg
mlfs cat /home/u/notes.txt --first          # 第一版
mlfs cat /home/u/notes.txt --timestamp 1791002258265
```

### ls

列出所有 id。加 `--json` 输出 JSON 数组，方便脚本处理。

```sh
mlfs ls
mlfs ls --json | jq -r '.[]'
```

### history

列出一个 id 的全部版本，带版本号、大小和块数。

```
序号     版本号(ms)                    大小       块数
0      1791002258265            12 B        1
1      1791002258316            12 B        1 (最新)
```

`--json` 时输出对象数组，字段是 `version` / `size` / `chunks`。

### rm

删除 id 及其全部历史。

```
mlfs rm <ID>... [-y]
```

删除前会问一句；标准输入不是终端（脚本里）时**必须**显式 `-y/--yes`，否则直接报错退出 —— 免得命令卡在等输入上。

删除只减引用计数，不会立刻动磁盘；命令末尾会提示还有多少可以回收。

### rm-history

只删掉某个版本，版本选项和 `export` 一样。

```sh
mlfs rm-history /home/u/notes.txt --latest -y
mlfs rm-history /home/u/notes.txt --timestamp 1791002258265 -y
```

### mv

给 id 改名，历史一并带走。

```sh
mlfs mv /home/u/draft.txt notes/final
```

目标 id 已存在时会直接报错，不会覆盖。

### gc

回收不再被任何版本引用的块。

```sh
mlfs gc --dry-run    # 先看看能回收多少
mlfs gc              # 真删
```

回收之所以要单独一步：反复「删了又加」时不必反复做无用的磁盘 IO。

## id 是怎么定的

**默认是文件在磁盘上的绝对路径**，单文件和递归共用这一套规则。

这样导入一个目录之后，里面的文件各自记在自己原来的位置上 —— 换个目录再导入同一批文件，命中的是同一份内容，不会变成第二份。`--id` 只是显式覆盖它。

```sh
$ mlfs import -R ./photos
$ mlfs ls
/home/u/photos/2024/a.jpg
/home/u/photos/2024/b.jpg
```

## 配置

### 优先级

```
命令行  >  环境变量（MLFS_ 前缀）  >  配置文件  >  内置默认
```

`--root` 覆盖 `MLFS_ROOT` 覆盖配置文件里的 `root`，以此类推。

### 配置文件

默认在 `$XDG_CONFIG_HOME/mlfs/config.toml`（通常是 `~/.config/mlfs/config.toml`），可以用 `--config <FILE>` 指到别处。**显式指定的文件不存在会报错**，而不是静默按默认值跑。

```toml
root = "/home/u/.local/share/mlfs"
codec = "zstd"
level = 3

[cdc]
min_size = 16384
avg_size = 32768
max_size = 65536

# 下面三个不写就跟着 root 推导：<root>/db.redb、<root>/data、<root>/.tmp。
# 写空字符串等同于没写。
# [storage]
# db_path = "/elsewhere/db.redb"
# data_path = "/elsewhere/data"
# temp_dir = "/elsewhere/.tmp"

[walk]
ignore = true
hidden = false
follow_links = false
max_depth = 0     # 0 = 不限

[output]
json = false
progress = true

[behavior]
overwrite = "archive"
confirm_remove = true
```

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `root` | `$XDG_DATA_HOME/mlfs` | fs 根目录 |
| `codec` | `zstd` | 默认压缩后端 |
| `level` | `3` | 默认压缩级别；`lz4` / `snappy` 没有级别，会被忽略 |
| `cdc.*` | 留空 | 分块粒度。留空的部分由库按 1:4:16 推导，全空则用 fastcdc 的 16/32/64 KiB |
| `storage.*` | 留空 | 数据库、数据目录、临时目录的位置；不写或写空字符串都表示跟着 `root` 推导 |
| `walk.ignore` | `true` | 递归时遵守 `.gitignore` / `.ignore` |
| `walk.hidden` | `false` | 递归时连隐藏文件一起导入 |
| `walk.follow_links` | `false` | 递归时跟随符号链接 |
| `walk.max_depth` | `0` | 最大递归深度，0 表示不限 |
| `output.json` | `false` | 列表类命令输出 JSON |
| `output.progress` | `true` | 显示进度条 |
| `behavior.overwrite` | `archive` | 导出时目标已存在的处理方式 |
| `behavior.confirm_remove` | `true` | 删除前是否确认 |

级别超出后端能接受的范围时会被夹住（gzip 0-9、brotli 0-11、zstd 1-22），不会 panic。

### 环境变量

前缀 `MLFS_`，嵌套层级用 `__` 分隔。变量名大小写不敏感。

```sh
export MLFS_ROOT=/data/fs
export MLFS_CODEC=brotli
export MLFS_LEVEL=9
export MLFS_CDC__AVG_SIZE=65536
export MLFS_BEHAVIOR__OVERWRITE=refuse
```

## 覆盖已有文件

`export` 会碰真实文件，所以目标已存在时不能装看不见。由 `behavior.overwrite` 或命令行的 `--overwrite` 决定：

| 取值 | 行为 |
| --- | --- |
| `archive`（默认） | **先把目标文件存进 fs**，再覆盖它。存档用的 id 就是目标的绝对路径 |
| `refuse` | 目标已存在就报错退出，什么都不做 |
| `force` | 直接覆盖，不留档 |

`archive` 的意义是：导出这个动作不会让任何数据消失。被覆盖掉的那份内容随时能找回来：

```sh
$ echo "LOCAL EDITS" > out.jpg
$ mlfs export /home/u/photo.jpg out.jpg
原文件已存档为 /home/u/out.jpg
已导出 /home/u/photo.jpg -> out.jpg

$ mlfs cat /home/u/out.jpg
LOCAL EDITS
```

## 退出码

- `0`：成功
- `1`：任何失败（参数错误、路径不存在、磁盘错误……），原因写在标准错误里

## 与库的关系

命令行是把库的几个接口包了一层：`copy_in` / `copy_out` / `list_file` / `get` / `history` / `remove` / `remove_history` / `rename` / `release` / `garbage`。

它默认把所有压缩后端都编进来（工具应当开箱即用），而库本身默认只带 `zstd` —— 想直接用库请见[根 README](../README.md)。

## 许可

MIT，见 [LICENSE](../../LICENSE)。
