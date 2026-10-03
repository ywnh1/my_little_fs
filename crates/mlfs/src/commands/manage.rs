//! `rm` / `rm-history` / `mv` / `gc`：删除、改名与回收。
//!
//! 删除只减引用计数，真正腾出磁盘是 `gc` 的事 —— 所以删完会顺手提示一句
//! 还有多少可回收，但**不会**自动回收：动磁盘这种事该由使用者明确要求。

use anyhow::{Context, Result, bail};
use my_little_fs::prelude::*;

use super::{confirm, ensure_id_exists, human_size};
use crate::{
    cli::{GcArgs, MvArgs, RmArgs, RmHistoryArgs},
    config::Config,
    output::Output,
};

pub fn remove(args: &RmArgs, config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;

    // 先统统确认存在：删到一半才发现某个 id 不存在，会留下半截状态
    for id in &args.ids {
        ensure_id_exists(&fs, id)?;
    }
    confirm(
        config,
        args.yes,
        &format!("将删除 {} 个 id 及其全部历史，继续？", args.ids.len()),
    )?;

    let mut removed = 0;
    for id in &args.ids {
        if fs.remove(id)? {
            removed += 1;
        }
    }
    out.info(format!("已删除 {removed} 个 id"));
    hint_gc(&fs, out)?;
    Ok(())
}

pub fn remove_history(args: &RmHistoryArgs, config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;
    ensure_id_exists(&fs, &args.id)?;

    let index = args.version.to_index();
    confirm(
        config,
        args.yes,
        &format!("将删除 {} 的{}，继续？", args.id, describe(&index)),
    )?;

    let removed = fs.remove_history(&args.id, index)?;
    out.info(format!("已删除 {} 个版本", removed.len()));
    hint_gc(&fs, out)?;
    Ok(())
}

pub fn rename(args: &MvArgs, config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;
    ensure_id_exists(&fs, &args.old)?;

    // 库底层的 rename 遇到同名表会直接报错，这里提前给一句能看懂的话
    if fs.list_file()?.iter().any(|id| id == &args.new) {
        bail!("{} 已经存在，不会覆盖；想替换请先删除它", args.new);
    }

    fs.rename(&args.old, &args.new)
        .with_context(|| format!("重命名 {} -> {} 失败", args.old, args.new))?;
    out.info(format!("已重命名 {} -> {}", args.old, args.new));
    Ok(())
}

pub fn gc(args: &GcArgs, config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;
    let (blocks, bytes) = fs.garbage()?;

    if args.dry_run {
        out.info(format!(
            "可回收 {blocks} 个块，共 {}（未做任何改动）",
            human_size(bytes)
        ));
        return Ok(());
    }

    if blocks == 0 {
        out.info("没有可回收的块");
        return Ok(());
    }

    let removed = fs.release()?;
    out.info(format!("已回收 {removed} 个块，释放 {}", human_size(bytes)));
    Ok(())
}

/// 删完提示一句还有多少可以回收。
fn hint_gc(fs: &Fs, out: &Output) -> Result<()> {
    let (blocks, bytes) = fs.garbage()?;
    if blocks > 0 {
        out.info(format!(
            "另有 {blocks} 个块不再被引用（{}），运行 mlfs gc 释放",
            human_size(bytes)
        ));
    }
    Ok(())
}

/// 把选取条件说成人话，用在确认提示里。
fn describe(index: &Index) -> String {
    match index {
        Index::All => "全部版本".to_string(),
        Index::First => "最老的版本".to_string(),
        Index::Latest => "最新的版本".to_string(),
        Index::Index(n) => format!("第 {n} 个版本"),
        Index::ReIndex(n) => format!("倒数第 {n} 个版本"),
        Index::TimeStamp(ts) => format!("版本 {ts}"),
        Index::Many(_) => "多个版本".to_string(),
    }
}
