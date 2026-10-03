//! 命令行界面的形状：全局选项、子命令、以及各命令自己的参数。
//!
//! 这里只负责**解析**，不碰任何业务逻辑。解析结果交给 [`crate::config`] 合成配置，
//! 再由 [`crate::commands`] 执行。

use clap::{Args, Parser, Subcommand, ValueEnum};
use my_little_fs::prelude::Index;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 内容寻址文件系统：导入、导出、浏览历史与回收。
#[derive(Debug, Parser)]
#[command(
    name = "mlfs",
    version,
    about = "内容寻址文件系统",
    long_about = "把文件按内容切块、用 blake3 寻址存储，相同内容只存一份。\n\n\
                  配置优先级：命令行 > 环境变量（MLFS_ 前缀）> 配置文件 > 内置默认。",
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Command,
}

/// 所有子命令都能用的选项。
#[derive(Debug, Args)]
pub struct GlobalOpts {
    /// fs 根目录，覆盖配置与环境变量
    #[arg(short, long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// 配置文件路径，默认 $XDG_CONFIG_HOME/mlfs/config.toml
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// 列表类命令输出 JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// 不显示进度条
    #[arg(long, global = true)]
    pub no_progress: bool,

    /// 只输出错误
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// 多输出一些过程信息（可重复）
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 导入文件或目录
    Import(ImportArgs),
    /// 按 id 导出到文件
    Export(ExportArgs),
    /// 按 id 导出到标准输出
    Cat(CatArgs),
    /// 列出所有 id
    Ls,
    /// 列出一个 id 的全部历史版本
    History(HistoryArgs),
    /// 删除 id 及其全部历史
    Rm(RmArgs),
    /// 只删除指定版本
    RmHistory(RmHistoryArgs),
    /// 重命名 id，历史一并带走
    Mv(MvArgs),
    /// 回收不再被引用的块
    Gc(GcArgs),
}

/// 压缩后端名。配置文件里写字符串，命令行上写同名取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CodecName {
    /// 不压缩
    None,
    Zstd,
    Gzip,
    Brotli,
    Lz4,
    Snappy,
}

/// 目标已存在时的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OverwriteMode {
    /// 先把已存在的目标存入 fs 存档，再覆盖（默认）
    Archive,
    /// 目标已存在就报错退出
    Refuse,
    /// 直接覆盖，不留档
    Force,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// 要导入的文件或目录
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 递归导入目录
    #[arg(short = 'R', long)]
    pub recursive: bool,

    /// 指定 id（只能配单个输入，不能与 --recursive 同用）
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,

    /// 本次导入使用的压缩后端，覆盖配置
    #[arg(long, value_enum)]
    pub codec: Option<CodecName>,

    /// 本次导入使用的压缩级别，覆盖配置
    #[arg(long)]
    pub level: Option<i32>,

    /// 递归时跟随符号链接
    #[arg(long)]
    pub follow_links: bool,

    /// 递归时连隐藏文件一起导入
    #[arg(long)]
    pub hidden: bool,

    /// 递归时不理会 .gitignore / .ignore
    #[arg(long)]
    pub no_ignore: bool,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// 要导出的 id
    #[arg(value_name = "ID")]
    pub id: String,

    /// 目标文件路径
    #[arg(value_name = "PATH")]
    pub dest: PathBuf,

    #[command(flatten)]
    pub version: VersionArgs,

    /// 目标已存在时怎么办，覆盖配置
    #[arg(long, value_enum)]
    pub overwrite: Option<OverwriteMode>,
}

#[derive(Debug, Args)]
pub struct CatArgs {
    /// 要输出的 id
    #[arg(value_name = "ID")]
    pub id: String,

    #[command(flatten)]
    pub version: VersionArgs,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// 要查看的 id
    #[arg(value_name = "ID")]
    pub id: String,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// 要删除的 id
    #[arg(required = true, value_name = "ID")]
    pub ids: Vec<String>,

    /// 不询问，直接删除
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct RmHistoryArgs {
    /// 要删版本的目标 id
    #[arg(value_name = "ID")]
    pub id: String,

    #[command(flatten)]
    pub version: VersionArgs,

    /// 不询问，直接删除
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct MvArgs {
    /// 现有 id
    #[arg(value_name = "OLD")]
    pub old: String,

    /// 新 id
    #[arg(value_name = "NEW")]
    pub new: String,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// 只报告会删掉什么，不真的删
    #[arg(long)]
    pub dry_run: bool,
}

/// 选版本的通用参数，不写就是最新版本。
///
/// 默认取最新版本，所以这几个是互斥的。
#[derive(Debug, Clone, Args)]
#[group(multiple = false)]
pub struct VersionArgs {
    /// 最老的版本
    #[arg(long)]
    pub first: bool,

    /// 最新的版本（默认）
    #[arg(long)]
    pub latest: bool,

    /// 正数第 n 个版本，从 0 开始
    #[arg(long, value_name = "N")]
    pub index: Option<usize>,

    /// 倒数第 n 个版本，从 0 开始
    #[arg(long, value_name = "N")]
    pub reindex: Option<usize>,

    /// 指定版本号（毫秒时间戳）
    #[arg(long, value_name = "MS")]
    pub timestamp: Option<u64>,

    /// 全部版本
    #[arg(long)]
    pub all: bool,
}

impl VersionArgs {
    /// 是否选了「全部版本」。
    ///
    /// `export` / `cat` 一次只能处理一个版本，靠这个把 `--all` 挡回去；
    /// `rm-history` 那边它是有意义的（删光所有版本）。
    #[must_use]
    pub fn is_all(&self) -> bool {
        self.all
    }

    /// 翻成库里的 [`Index`]。什么都没给就是最新版本。
    #[must_use]
    pub fn to_index(&self) -> Index {
        if self.first {
            Index::First
        } else if let Some(n) = self.index {
            Index::Index(n)
        } else if let Some(n) = self.reindex {
            Index::ReIndex(n)
        } else if let Some(ts) = self.timestamp {
            Index::TimeStamp(ts)
        } else if self.all {
            Index::All
        } else {
            Index::Latest
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // clap 自己的结构校验：参数冲突、帮助文本等
        Cli::command().debug_assert();
    }

    #[test]
    fn default_version_is_latest() {
        let args = VersionArgs {
            first: false,
            latest: false,
            index: None,
            reindex: None,
            timestamp: None,
            all: false,
        };
        assert_eq!(args.to_index(), Index::Latest);
    }

    #[test]
    fn version_flags_map_to_index_variants() {
        let base = VersionArgs {
            first: false,
            latest: false,
            index: None,
            reindex: None,
            timestamp: None,
            all: false,
        };

        let cases = [
            (
                VersionArgs {
                    first: true,
                    ..base.clone()
                },
                Index::First,
            ),
            (
                VersionArgs {
                    index: Some(2),
                    ..base.clone()
                },
                Index::Index(2),
            ),
            (
                VersionArgs {
                    reindex: Some(1),
                    ..base.clone()
                },
                Index::ReIndex(1),
            ),
            (
                VersionArgs {
                    timestamp: Some(42),
                    ..base.clone()
                },
                Index::TimeStamp(42),
            ),
            (
                VersionArgs {
                    all: true,
                    ..base.clone()
                },
                Index::All,
            ),
        ];

        for (args, want) in cases {
            assert_eq!(args.to_index(), want);
        }
    }
}
