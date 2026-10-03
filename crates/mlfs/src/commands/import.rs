//! `import`：把文件或目录导入 fs。
//!
//! id 默认取文件的**绝对路径**（单文件与递归同一套规则），`--id` 用于显式覆盖。
//! 目录必须显式 `-R` 才会递归 —— 免得「手滑导入了一个大目录」这种事故。

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use my_little_fs::prelude::*;
use std::path::{Path, PathBuf};

use super::{dedup_by_id, human_size, path_id};
use crate::{cli::ImportArgs, config::Config, output::Output};

pub fn run(args: &ImportArgs, config: &Config, out: &Output) -> Result<()> {
    let (effective, items) = prepare(args, config)?;
    if items.is_empty() {
        out.info("没有可导入的文件");
        return Ok(());
    }

    let fs = effective.open_fs()?;
    let bar = out.progress(items.len() as u64, "导入中");
    let mut bytes = 0u64;

    // 逐个导入而不是一次性批量：进度条要如实反映进展，而库的批量接口
    // 中间没有回调可挂。失败时前面的文件已经进去了，重跑会命中去重，代价很小。
    for (id, path) in &items {
        fs.copy_in(path, id)
            .with_context(|| format!("导入 {} 失败", path.display()))?;
        bytes += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        bar.inc(1);
        out.debug(format!("{id}  <-  {}", path.display()));
    }
    bar.finish_and_clear();

    out.info(format!(
        "已导入 {} 个文件，共 {}",
        items.len(),
        human_size(bytes)
    ));
    Ok(())
}

/// 合成本次导入的有效设置，并把输入路径展开成 `(id, 路径)` 列表。
fn prepare(args: &ImportArgs, config: &Config) -> Result<(Config, Vec<(String, PathBuf)>)> {
    if args.id.is_some() && args.paths.len() > 1 {
        bail!("--id 只能配一个输入路径");
    }
    if args.id.is_some() && args.recursive {
        bail!("--id 与 --recursive 不能同时用：递归时每个文件的 id 来自它自己的路径");
    }

    // 命令行对配置的临时覆盖（优先级最高）
    let mut effective = config.clone();
    if let Some(codec) = args.codec {
        effective.codec = codec;
    }
    if let Some(level) = args.level {
        effective.level = level;
    }

    let mut items = Vec::new();
    for path in &args.paths {
        if path.is_dir() {
            if !args.recursive {
                bail!("{} 是目录，要递归导入请加 -R", path.display());
            }
            for file in walk_dir(path, args, config)? {
                items.push((path_id(&file)?, file));
            }
        } else if path.is_file() {
            let id = match &args.id {
                Some(given) => given.clone(),
                None => path_id(path)?,
            };
            items.push((id, path.clone()));
        } else {
            bail!("路径不存在：{}", path.display());
        }
    }

    Ok((effective, dedup_by_id(items)))
}

/// 用 `ignore` 遍历目录，收集其中的普通文件。
///
/// 开关的合成规则：`hidden` / `follow_links` 只要有一边要求就打开；
/// `ignore` 反过来，只要有一边要求关闭就全关（关掉 `.gitignore`、
/// `.ignore`、全局 gitignore 以及父目录的规则）。
fn walk_dir(root: &Path, args: &ImportArgs, config: &Config) -> Result<Vec<PathBuf>> {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(!(config.walk.hidden || args.hidden));
    builder.follow_links(config.walk.follow_links || args.follow_links);
    if config.walk.max_depth > 0 {
        builder.max_depth(Some(config.walk.max_depth));
    }
    if args.no_ignore || !config.walk.ignore {
        builder
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
    }

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry.with_context(|| format!("遍历 {} 失败", root.display()))?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}
