//! `export` / `cat`：把 fs 里的内容写回真实世界。
//!
//! `export` 会碰真实文件，所以目标已存在时按配置决定行为：
//! 默认 `archive` —— **先把那个已存在的文件存进 fs**，再覆盖它，
//! 这样「导出」这个动作不会让任何数据消失。

use anyhow::{Context, Result, bail};
use my_little_fs::prelude::*;
use std::io::Write;

use super::{path_id, take_version};
use crate::{
    cli::{CatArgs, ExportArgs, OverwriteMode},
    config::Config,
    output::Output,
};

pub fn run_export(args: &ExportArgs, config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;
    let dest = &args.dest;
    let mode = config.overwrite_mode(args.overwrite);

    if dest.exists() {
        match mode {
            OverwriteMode::Refuse => bail!(
                "目标已存在：{}（覆盖用 --overwrite force，先存档用 --overwrite archive）",
                dest.display()
            ),
            OverwriteMode::Archive => {
                // 目标文件的 id 就是它的绝对路径，和 import 的规则一致
                let archive_id = path_id(dest)?;
                fs.copy_in(dest, &archive_id)
                    .with_context(|| format!("存档已有文件失败：{}", dest.display()))?;
                out.info(format!("原文件已存档为 {archive_id}"));
            }
            OverwriteMode::Force => {}
        }
    }

    let file = take_version(&fs, &args.id, &args.version)?;
    fs.copy_out(dest, file)
        .with_context(|| format!("导出到 {} 失败", dest.display()))?;
    out.info(format!("已导出 {} -> {}", args.id, dest.display()));
    Ok(())
}

pub fn run_cat(args: &CatArgs, config: &Config) -> Result<()> {
    let fs = config.open_fs()?;
    let mut file = take_version(&fs, &args.id, &args.version)?;

    let mut stdout = std::io::stdout().lock();
    std::io::copy(&mut file, &mut stdout)?;
    stdout.flush()?;
    Ok(())
}
