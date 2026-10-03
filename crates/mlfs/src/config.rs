//! 配置的合成：**内置默认 ← 配置文件 ← 环境变量 ← 命令行**。
//!
//! 优先级由 Figment 的 merge 顺序决定 —— 后 merge 的覆盖先 merge 的。
//! 命令行那一层的处理有个细节：只有用户**显式给出**的项才参与覆盖，
//! 否则「没写 `--json`」会因为 bool 的默认值 `false` 把配置文件里的
//! `json = true` 顶掉。
//!
//! # 位置
//!
//! - 配置文件：`--config` 指定 > `$XDG_CONFIG_HOME/mlfs/config.toml`
//!   （默认 `~/.config/mlfs/config.toml`）
//! - 环境变量：`MLFS_` 前缀，嵌套用 `__` 分隔，例如
//!   `MLFS_ROOT=/data/fs`、`MLFS_CDC__AVG_SIZE=65536`

use anyhow::{Context, Result};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use my_little_fs::prelude::{Compress, Fs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::cli::{Cli, CodecName, OverwriteMode};

/// 全部可配置项。
///
/// 除了「必须由命令行临时给出的输入」（要导入的路径、要导出的 id 之类），
/// 其余选项在这里都有一份默认值，也都能写进配置文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// fs 根目录。数据库、数据目录、临时目录都挂在它下面
    pub root: PathBuf,
    /// 默认压缩后端
    pub codec: CodecName,
    /// 默认压缩级别；对 lz4 / snappy 无意义
    pub level: i32,
    /// 分块粒度
    pub cdc: Cdc,
    /// 存储位置
    pub storage: Storage,
    /// 递归导入时的遍历规则
    pub walk: Walk,
    /// 输出与进度
    pub output: Output,
    /// 行为开关
    pub behavior: Behavior,
}

/// 内容定义分块（CDC）的三个尺寸，单位字节。
///
/// 都可以留空：留空的部分由库按 1:4:16 推导，三个都空就用 fastcdc 的推荐值
/// （16 KiB / 32 KiB / 64 KiB）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cdc {
    pub min_size: Option<usize>,
    pub avg_size: Option<usize>,
    pub max_size: Option<usize>,
}

/// 数据放在哪。留空表示跟着 `root` 推导。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Storage {
    /// 数据库文件，默认 `<root>/db.redb`
    pub db_path: Option<PathBuf>,
    /// 块数据目录，默认 `<root>/data`
    pub data_path: Option<PathBuf>,
    /// 临时文件目录，默认 `<root>/.tmp`；仅在与目标同文件系统时才会被采用
    pub temp_dir: Option<PathBuf>,
}

/// 递归导入目录时的遍历规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Walk {
    /// 遵守 `.gitignore` / `.ignore` / 全局 gitignore
    pub ignore: bool,
    /// 连隐藏文件一起导入
    pub hidden: bool,
    /// 跟随符号链接（可能走出被导入的目录，甚至成环）
    pub follow_links: bool,
    /// 最大递归深度，0 表示不限
    pub max_depth: usize,
}

impl Default for Walk {
    fn default() -> Self {
        Self {
            ignore: true,
            hidden: false,
            follow_links: false,
            max_depth: 0,
        }
    }
}

/// 输出相关的开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Output {
    /// 列表类命令输出 JSON
    pub json: bool,
    /// 显示进度条
    pub progress: bool,
    /// 只输出错误
    pub quiet: bool,
    /// 过程信息详细度
    pub verbose: u8,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            json: false,
            progress: true,
            quiet: false,
            verbose: 0,
        }
    }
}

/// 行为开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Behavior {
    /// 导出时目标已存在怎么办
    pub overwrite: OverwriteMode,
    /// 删除前是否要交互确认
    pub confirm_remove: bool,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            overwrite: OverwriteMode::Archive,
            confirm_remove: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: default_root(),
            codec: CodecName::Zstd,
            level: 3,
            cdc: Cdc::default(),
            storage: Storage::default(),
            walk: Walk::default(),
            output: Output::default(),
            behavior: Behavior::default(),
        }
    }
}

impl CodecName {
    /// 把配置里的后端转成库要的压缩设置；`none` 对应「不压缩」。
    ///
    /// 命令行界面把所有后端都编了进来，所以这里不会有「本构建未启用」的情况
    /// —— 那种错误只会在直接用库、且关掉了对应 feature 时出现。
    #[must_use]
    pub fn to_compress(self, level: i32) -> Option<Compress> {
        match self {
            Self::None => None,
            Self::Zstd => Some(Compress::zstd(level)),
            Self::Gzip => Some(Compress::gzip(level)),
            Self::Brotli => Some(Compress::brotli(level)),
            Self::Lz4 => Some(Compress::lz4()),
            Self::Snappy => Some(Compress::snappy()),
        }
    }
}

impl Config {
    /// 按当前配置打开（不存在则创建）一个 `Fs`。
    pub fn open_fs(&self) -> Result<Fs> {
        let mut builder =
            Fs::builder(self.root.clone()).with_compress(self.codec.to_compress(self.level));

        if let Some(v) = self.cdc.min_size {
            builder = builder.with_min_size(Some(v));
        }
        if let Some(v) = self.cdc.avg_size {
            builder = builder.with_avg_size(Some(v));
        }
        if let Some(v) = self.cdc.max_size {
            builder = builder.with_max_size(Some(v));
        }

        if let Some(p) = &self.storage.db_path {
            builder = builder.with_db_path(Some(p.clone()));
        }
        if let Some(p) = &self.storage.data_path {
            builder = builder.with_data_path(Some(p.clone()));
        }
        if let Some(p) = &self.storage.temp_dir {
            builder = builder.with_temp_dir(Some(p.clone()));
        }

        builder
            .build()
            .with_context(|| format!("打开 fs 失败：{}", self.root.display()))
    }

    /// 导出时该用哪种覆盖策略：命令行给了就用命令行的。
    #[must_use]
    pub fn overwrite_mode(&self, from_cli: Option<OverwriteMode>) -> OverwriteMode {
        from_cli.unwrap_or(self.behavior.overwrite)
    }
}

/// 内置默认根目录：优先 XDG 数据目录，实在没有就退到当前目录下的 `.mlfs`。
fn default_root() -> PathBuf {
    match config_base("XDG_DATA_HOME", ".local/share") {
        Some(dir) => dir.join("mlfs"),
        None => PathBuf::from(".mlfs"),
    }
}

/// 默认配置文件位置。
#[must_use]
pub fn default_config_path() -> PathBuf {
    match config_base("XDG_CONFIG_HOME", ".config") {
        Some(dir) => dir.join("mlfs/config.toml"),
        None => PathBuf::from(".mlfs.toml"),
    }
}

/// 取一个 XDG 风格的基础目录：先看环境变量，再退回 `$HOME/<fallback>`。
fn config_base(env_var: &str, fallback: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(env_var)
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(fallback))
}

/// 把默认值、配置文件、环境变量、命令行依次叠起来。
///
/// 命令行只贡献用户显式给出的项 —— 详见本模块文档。
pub fn load(cli: &Cli) -> Result<Config> {
    let mut figment = Figment::from(Serialized::defaults(Config::default()));

    // ---- 配置文件 ----
    let explicit = cli.global.config.clone();
    let path = explicit.clone().unwrap_or_else(default_config_path);
    if path.exists() {
        figment = figment.merge(Toml::file(&path));
    } else if explicit.is_some() {
        // 明确指定了却找不到，多半是打错了路径，不该静默按默认值跑
        anyhow::bail!("配置文件不存在：{}", path.display());
    }

    // ---- 环境变量 ----
    figment = figment.merge(Env::prefixed("MLFS_").split("__"));

    // ---- 命令行（只叠加显式给出的）----
    if let Some(root) = &cli.global.root {
        figment = figment.merge(("root", root.clone()));
    }
    if cli.global.json {
        figment = figment.merge(("output.json", true));
    }
    if cli.global.quiet {
        figment = figment.merge(("output.quiet", true));
    }
    if cli.global.no_progress {
        figment = figment.merge(("output.progress", false));
    }
    if cli.global.verbose > 0 {
        figment = figment.merge(("output.verbose", cli.global.verbose));
    }

    figment.extract().context("配置不合法")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.codec, CodecName::Zstd);
        assert_eq!(c.level, 3);
        assert!(c.walk.ignore, "默认应当遵守 .gitignore");
        assert!(!c.walk.hidden, "默认不该把隐藏文件卷进来");
        assert!(c.output.progress);
        assert_eq!(c.behavior.overwrite, OverwriteMode::Archive);
        assert!(c.behavior.confirm_remove);
        assert_eq!(c.cdc.min_size, None, "留空表示交给库去推导");
    }

    #[test]
    fn codec_names_roundtrip_through_toml_style_strings() {
        for (name, text) in [
            (CodecName::None, "\"none\""),
            (CodecName::Zstd, "\"zstd\""),
            (CodecName::Gzip, "\"gzip\""),
            (CodecName::Brotli, "\"brotli\""),
            (CodecName::Lz4, "\"lz4\""),
            (CodecName::Snappy, "\"snappy\""),
        ] {
            let json = serde_json::to_string(&name).unwrap();
            assert_eq!(json, text);
            assert_eq!(serde_json::from_str::<CodecName>(&json).unwrap(), name);
        }
    }

    #[test]
    fn none_codec_means_no_compression() {
        assert!(CodecName::None.to_compress(9).is_none());
    }

    #[test]
    fn leveled_codecs_carry_the_level_and_level_less_ones_ignore_it() {
        assert_eq!(CodecName::Zstd.to_compress(3).unwrap().level, 3);
        assert_eq!(CodecName::Gzip.to_compress(6).unwrap().level, 6);
        assert_eq!(CodecName::Brotli.to_compress(9).unwrap().level, 9);
        // lz4 / snappy 没有级别，传什么都按 0 记
        assert_eq!(CodecName::Lz4.to_compress(9).unwrap().level, 0);
        assert_eq!(CodecName::Snappy.to_compress(9).unwrap().level, 0);
    }

    #[test]
    fn cli_overwrite_wins_over_config() {
        let c = Config::default();
        assert_eq!(c.overwrite_mode(None), OverwriteMode::Archive);
        assert_eq!(
            c.overwrite_mode(Some(OverwriteMode::Force)),
            OverwriteMode::Force
        );
    }

    #[test]
    fn config_file_path_follows_xdg() {
        // 该测试只验证拼接规则，不动真实环境变量
        let path = default_config_path();
        assert!(path.to_string_lossy().contains("config.toml"));
    }
}
