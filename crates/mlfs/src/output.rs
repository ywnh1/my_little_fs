//! 统一的输出出口。
//!
//! 三个开关在这一层收口：`--json` 决定列表类命令的版式，`--quiet` 压掉非错误输出，
//! `--no-progress`（以及 JSON 模式）关掉进度条 —— 进度条往 stderr 写，不该污染
//! 管道里的 JSON。

use crate::config::Output as OutputConfig;
use indicatif::{ProgressBar, ProgressStyle};

/// 命令输出用的句柄。
pub struct Output {
    json: bool,
    quiet: bool,
    verbose: u8,
    progress: bool,
}

impl Output {
    #[must_use]
    pub fn new(cfg: &OutputConfig) -> Self {
        Self {
            json: cfg.json,
            quiet: cfg.quiet,
            verbose: cfg.verbose,
            progress: cfg.progress && !cfg.json,
        }
    }

    /// 是否输出 JSON。
    #[must_use]
    pub fn json(&self) -> bool {
        self.json
    }

    /// 静音模式下也不该丢的结果（比如 `cat` 的内容、`--json` 的结果），
    /// 用这个绕过 quiet 判断。
    pub fn emit(&self, line: impl AsRef<str>) {
        println!("{}", line.as_ref());
    }

    /// 普通信息，`--quiet` 时压掉。
    pub fn info(&self, line: impl AsRef<str>) {
        if !self.quiet {
            println!("{}", line.as_ref());
        }
    }

    /// 过程信息，只有 `-v` 才显示。
    pub fn debug(&self, line: impl AsRef<str>) {
        if self.verbose > 0 && !self.quiet {
            eprintln!("{}", line.as_ref());
        }
    }

    /// 建一个进度条。被关掉时返回一个隐藏的，调用方不用到处写 if。
    #[must_use]
    pub fn progress(&self, total: u64, label: &str) -> ProgressBar {
        if !self.progress {
            return ProgressBar::hidden();
        }
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template("{msg} [{bar:40}] {pos}/{len} {per_bar}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        bar.set_message(label.to_string());
        bar
    }
}
