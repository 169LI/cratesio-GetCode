//! 功能
//! -
//! 这是 `datahandle-migration` 的入口程序，用于执行 SeaORM 的数据库迁移，并在需要时自动生成/更新
//! `datahandle/src/entities` 下的实体代码。
//!
//! 环境变量加载
//! -
//! 程序优先读取工作区根目录的 `.env`（`../../.env`），失败时回退到当前目录的默认 `.env` 解析逻辑。
//! 需要至少包含 `DATABASE_URL`（Postgres 连接串），用于执行迁移与生成实体。
//!
//! 运行方式
//! -
//! 在 `datahandle/migration` 目录下执行：
//! - 执行未执行的迁移：`cargo run -- up`
//! - 执行未执行的迁移（指定次数）：`cargo run -- up -n 1`（只执行 1 个迁移）
//! - 查看状态：`cargo run -- status`
//! - 回滚一步：`cargo run -- down`
//! - 只生成实体：`cargo run -- update`
//! - 当前迁移状态（哪些 pending）`cargo run -- status`
//!
//! 自动生成实体的触发条件
//! -
//! 当命令参数包含 `up` / `refresh` / `fresh` 时，迁移执行完成后会调用 `sea-orm-cli generate entity`
//! 重新生成 `datahandle/src/entities` 下的实体代码；`down` 不会自动生成实体（如需更新请手动运行 `update`）。

use dotenvy::dotenv;
use sea_orm_migration::prelude::*;
use std::path::PathBuf;

fn should_update_entities(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "up" | "refresh" | "fresh"))
}

fn load_env() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_env = manifest_dir.join("..").join("..").join(".env");

    if dotenvy::from_path(workspace_env).is_ok() {
        return;
    }

    dotenv().ok();
}

fn entities_output_dir() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let datahandle_dir = manifest_dir.parent().ok_or("invalid manifest dir")?;
    Ok(datahandle_dir.join("src").join("entities"))
}

fn update_entities() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("DATABASE_URL")?;
    let output_dir = entities_output_dir()?;
    std::fs::create_dir_all(&output_dir)?;

    let status = std::process::Command::new("sea-orm-cli")
        .args([
            "generate",
            "entity",
            "-u",
            database_url.as_str(),
            "-o",
            output_dir.to_str().ok_or("invalid output dir")?,
            "--with-serde",
            "both",
            "--expanded-format",
        ])
        .status()?;

    if !status.success() {
        return Err("sea-orm-cli failed".into());
    }

    Ok(())
}

// - 执行迁移： cargo run -- up
// - 查看状态： cargo run -- status
// - 回滚： cargo run -- down
// - 只生成实体： cargo run -- update
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    load_env();
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|arg| arg == "update") {
        update_entities()?;
        return Ok(());
    }

    cli::run_cli(datahandle_migration::Migrator).await;

    if should_update_entities(&args) {
        update_entities()?;
    }

    Ok(())
}
