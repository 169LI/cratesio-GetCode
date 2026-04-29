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
//! - 数据预处理/导入（需要先进行前两次数据库迁移建表，见：/datahandle/migrations/src/main.rs、准备数据：crates.txt、data.txt）：`cargo run -p crates_io -- data-batch import-base`
//! - 版本以及依赖的预处理（需要先进行前3、4次数据库迁移建表，见：/datahandle/migrations/src/main.rs、准备数据：cratesio_index\）：`cargo run -p crates_io -- data-batch handle-version`
//! - 编译阶段的预处理（需要先进行前5次数据库迁移建表，见：/datahandle/migrations/src/main.rs）：`cargo run -p crates_io -- data-batch precompile-skip-no-deps`
//! - 编译 crate（需要确保数据库迁移建表，见：/datahandle/migrations/src/main.rs）：`cargo run -p crates_io -- compile`
//! 
//! 目前的处理顺序
//! 
//! 1. 导入基础数据:cargo run -- up (后续每次更新代码时都要先进行数据库迁移、以及.env变量的调整和相对文件的下载)
//! 2. 下载文件：cargo run -p crates_io -- download   (不要全部下载，下载一分钟的文件量就可以，全部下载需要20多G)
//! 3. 处理依赖版本信息:cargo run -p crates_io -- data-batch handle-version
//! 4. 预处理编译状态（compile_handled）:cargo run -p crates_io -- data-batch precompile-skip-no-deps
//! 5. 处理依赖更新失败错误信息（dep_update_errors）：cargo run -p crates_io -- compile
//! 

mod cli;
mod commands;
mod config;
mod logging;
mod pgdatahandle;

use clap::Parser;
use cli::{Cli, Commands};
use commands::{compile, databatch, download};
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
        Commands::Compile => compile::compile_run(&db).await?,
        Commands::DataBatch(args) => databatch::batch_run(&db, &args).await?,
    }

    Ok(())
}
