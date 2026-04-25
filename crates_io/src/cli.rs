//! 模块说明
//! -
//! CLI 参数定义模块（基于 clap derive）。
//!
//! 设计约定
//! -
//! - 子命令只做“选择功能”的分发，不承载大量参数
//! - 具体参数尽量通过 `.env` / 环境变量读取（由 `config` 模块负责 Fail Fast 校验）

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "crates_io")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Download,
    Build,
}
