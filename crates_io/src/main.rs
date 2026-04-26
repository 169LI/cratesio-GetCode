//! 功能
//! -
//! crates_io CLI 程序入口，负责：
//! - 初始化日志
//! - 解析命令行
//! - 预加载配置（Fail Fast，从工作区根目录 `.env` / 环境变量读取）
//! - 初始化数据库连接句柄 `PgDataHandle`
//! - 将子命令分发到对应的业务模块
//!
//! 启动方式（在工作区根目录执行）
//! -
//! - 批量下载：`cargo run -p crates_io -- download`
//! - 批量构建：`cargo run -p crates_io -- build`
//! - 数据预处理/导入（需要先进行前两次数据库迁移建表，见：/datahandle/migrations/src/main.rs）：`cargo run -p crates_io -- data-batch import-base` 
//!
//! 环境变量（.env / 环境变量读取）
//! -
//! - `DATABASE_URL`: Postgres 连接串（必填）

mod cli;
mod commands;
mod config;
mod logging;
mod pgdatahandle;

use clap::Parser;
use cli::{Cli, Commands};
use commands::{build, databatch, download};
use std::path::PathBuf;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::init_logging();
    let cli = Cli::parse();
    let config = config::get_config_once(&config::ConfigLoad::new())?;
    let database_url = config.require("DATABASE_URL")?;
    let download_dir = PathBuf::from(config.require("DOWNLOAD_DIR")?);
    let db = pgdatahandle::PgDataHandle::new(&database_url).await?;

    match cli.command {
        Commands::Download => download::download_run(&db, &download_dir).await?,
        Commands::Build => build::build_run(&db).await?,
        Commands::DataBatch(args) => databatch::batch_run(&db, &args).await?,
    }

    Ok(())
}
