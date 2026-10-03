//! 各子命令的实现。
//!
//! 这一层只做三件事：把命令行参数与配置合成出「本次运行的有效设置」、
//! 调用库、把结果按 `--json` / `--quiet` 的要求吐出来。业务逻辑不在这里。

mod export;
mod import;
mod inspect;
mod manage;

use anyhow::{Context, Result, bail};
use my_little_fs::prelude::*;
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use crate::{
    cli::{Cli, Command, VersionArgs},
    config::Config,
    output::Output,
};

/// 按子命令分发。
pub fn run(cli: &Cli, config: &Config, out: &Output) -> Result<()> {
    match &cli.command {
        Command::Import(args) => import::run(args, config, out),
        Command::Export(args) => export::run_export(args, config, out),
        Command::Cat(args) => export::run_cat(args, config),
        Command::Ls => inspect::list(config, out),
        Command::History(args) => inspect::history(args, config, out),
        Command::Rm(args) => manage::remove(args, config, out),
        Command::RmHistory(args) => manage::remove_history(args, config, out),
        Command::Mv(args) => manage::rename(args, config, out),
        Command::Gc(args) => manage::gc(args, config, out),
    }
}

/// id 规则：文件在磁盘上的**绝对路径**（顺带解析符号链接）。
///
/// 单文件与递归走同一套规则，`--id` 只是显式覆盖它。
pub(crate) fn path_id(path: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("无法确定 {} 的绝对路径", path.display()))?;
    Ok(abs.to_string_lossy().into_owned())
}

/// 确认这个 id 存在。
///
/// 库的 `get` 对未知 id 抛的是 redb 的「表不存在」，直接把内部错误透给使用者
/// 毫无帮助，所以先在这里用 `list_file` 判一下，换成一句人话。
pub(crate) fn ensure_id_exists(fs: &Fs, id: &str) -> Result<()> {
    if !fs.list_file()?.iter().any(|existing| existing == id) {
        bail!("没有 id 为 {id} 的文件");
    }
    Ok(())
}

/// 取出所选版本的内容。
pub(crate) fn take_version(fs: &Fs, id: &str, version: &VersionArgs) -> Result<FsFile> {
    ensure_id_exists(fs, id)?;
    let mut files = fs.get(id, version.to_index())?;
    if files.is_empty() {
        bail!("id {id} 没有符合所选条件的版本");
    }
    Ok(files.remove(0))
}

/// 危险操作前的确认。
///
/// `--yes` 与配置里关掉确认都直接放行。标准输入不是终端时**必须**显式
/// `--yes`：否则脚本会把命令行卡在等待输入上。
pub(crate) fn confirm(config: &Config, yes: bool, question: &str) -> Result<()> {
    if yes || !config.behavior.confirm_remove {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("需要确认但标准输入不是终端，请加 --yes");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        Ok(())
    } else {
        bail!("已取消")
    }
}

/// 人类可读的字节数，只用于展示。
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 把条目按输入顺序去重，保留第一次出现的位置。
///
/// 递归导入时同一个文件可能被多个输入路径带进来（比如 `mlfs import a a/b`），
/// 重复导入虽然不会出错，但计数会虚高，没必要。
pub(crate) fn dedup_by_id(items: Vec<(String, PathBuf)>) -> Vec<(String, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|(id, _)| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_switches_units_at_1024() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn dedup_keeps_the_first_occurrence() {
        let items = vec![
            ("a".to_string(), PathBuf::from("1")),
            ("b".to_string(), PathBuf::from("2")),
            ("a".to_string(), PathBuf::from("3")),
        ];
        let deduped = dedup_by_id(items);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0], ("a".to_string(), PathBuf::from("1")));
        assert_eq!(deduped[1], ("b".to_string(), PathBuf::from("2")));
    }
}
