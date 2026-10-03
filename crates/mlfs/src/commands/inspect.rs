//! `ls` / `history`：看有什么、每个东西有哪些版本。

use anyhow::Result;
use my_little_fs::prelude::*;

use super::{ensure_id_exists, human_size};
use crate::{cli::HistoryArgs, config::Config, output::Output};

pub fn list(config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;
    let mut ids = fs.list_file()?;
    // 库不保证顺序，这里排一下让输出稳定、可 diff
    ids.sort();

    if out.json() {
        out.emit(serde_json::to_string_pretty(&ids)?);
    } else {
        for id in ids {
            out.emit(id);
        }
    }
    Ok(())
}

pub fn history(args: &HistoryArgs, config: &Config, out: &Output) -> Result<()> {
    let fs = config.open_fs()?;
    ensure_id_exists(&fs, &args.id)?;
    let versions = fs.history(&args.id)?;

    if out.json() {
        let rows: Vec<_> = versions
            .iter()
            .map(|(version, file)| {
                serde_json::json!({
                    "version": version,
                    "size": file_size(file),
                    "chunks": chunk_count(file),
                })
            })
            .collect();
        out.emit(serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if versions.is_empty() {
        out.info(format!("{} 没有任何版本", args.id));
        return Ok(());
    }

    out.emit(format!(
        "{:<6} {:<16} {:>12} {:>8}",
        "序号", "版本号(ms)", "大小", "块数"
    ));
    for (i, (version, file)) in versions.iter().enumerate() {
        let marker = if i + 1 == versions.len() {
            " (最新)"
        } else {
            ""
        };
        out.emit(format!(
            "{i:<6} {version:<16} {:>12} {:>8}{marker}",
            human_size(file_size(file)),
            chunk_count(file)
        ));
    }
    Ok(())
}

/// `FsFile::get_size` 需要 `&mut`，读起来顺手包一层。
fn file_size(file: &FsFile) -> u64 {
    let mut file = file.clone();
    file.get_size()
}

fn chunk_count(file: &FsFile) -> usize {
    file.chunk_count()
}
