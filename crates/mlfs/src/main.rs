//! `mlfs` —— my_little_fs 的命令行界面。
//!
//! 装配顺序：解析命令行 → 叠出配置（默认 ← 配置文件 ← 环境变量 ← 命令行）
//! → 交给 [`commands`] 执行。
//!
//! 配置文件路径与优先级见 [`config`] 的模块文档。

mod cli;
mod commands;
mod config;
mod output;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let config = config::load(&cli)?;
    let out = output::Output::new(&config.output);

    out.debug(format!("fs 根目录：{}", config.root.display()));
    commands::run(&cli, &config, &out)
}
