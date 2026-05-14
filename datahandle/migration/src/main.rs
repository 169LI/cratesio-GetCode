//! 功能
//! -
//! 这是 `datahandle-migration` 的入口程序，用于执行 SeaORM 的数据库迁移，并在需要时自动生成/更新
//! `datahandle/src/entities` 下的实体代码。
//! 
//! **注意**
//! 如果每次拉取代码的时候发现有新的迁移文件，务必手动执行 `cargo run -- up` 来应用迁移
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
//!
//!
//! 现有数据表对应字段的说明
//! ## crates
//!  Id: 主键，自增
//!  Name: crate 名称
//!  Homepage: crate 官方首页
//!  Analyzed: 是否已分析   (默认 false)
//!  Download: 是否已下载   (默认 false)
//!  CreatedAt: 创建时间
//!  UpdatedAt: 更新时间
//!  VersionNew: 最新版本(稳定发布且符合语义化版本控制)
//!  DownloadFailed: 是否下载失败   (默认 false)
//!  VersionHandled: 是否已处理依赖版本信息的提取   (默认 false)
//!  CompileHandled: 是否进入过“编译”流程   (默认 false)
//!  InitialCompileFailed: 是否初始编译失败   (默认 false)
//!  CargoLockExists: 源码目录中是否存在 Cargo.lock（记录编译前的状态）
//!  DepUpdateErrors: 依赖更新失败错误信息
//!  HeavyDepsSkipped: 是否跳过重依赖   (默认 false)
//!  HeavyDepsCount: 重依赖数量   (默认 0)
//!
//! ## crate_versions_index
//!  Id: 主键，自增
//!  CrateId: crate 主键
//!  Version: 版本号
//!  Deps: 依赖信息(JSON 格式)
//!  Features2: 功能信息(JSON 格式)
//!  Pubtime: 发布时间
//!


/*datahandle-migration是一个单独可执行程序 ，专门做两件事：
执行数据库迁移 ：把 migration/src/m2026...*.rs 里定义的建表/改表脚本按顺序应用到 Postgres。
可选：生成实体代码（entities）：迁移完成后，调用外部工具sea-orm-cli generate entity ，
把数据库表结构反向生成到 datahandle/src/entities/ （SeaORM 的实体结构体）。*/

use dotenvy::dotenv;
use sea_orm_migration::prelude::*;
use std::path::PathBuf;

/*- 输入程序启动参数，只要参数里出现 "up" | "fresh" | "fresh" 就返回 true，就代表数据库结构发生变化，需要更新 entities。
输出 是否要在迁移后自动生成 entities*/
fn should_update_entities(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "up" | "refresh" | "fresh"))
}

/*让DATABASE_URL这类环境变量进入进程环境，供后续连接数据库用。
用env!("CARGO_MANIFEST_DIR")找到当前crate（datahandle/migration）的目录。
推导出workspace根目录
按优先级尝试加载两个.env ：
   - workspace_root/.env（不覆盖已有同名环境变量）
   - workspace_root/datahandle/data_import/.env（会覆盖已有同名环境变量）
如果都没加载成功，退回到dotenv()的默认行为（通常是尝试当前工作目录下的 .env ）
保证你放在 data_import 里的连接串能生效，并且优先级最高 （这就是你后来能稳定连上 rust 库的原因）。*/
fn load_env() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..").join("..");
    let env_candidates = [
        (workspace_root.join(".env"), false),
        (workspace_root.join("datahandle").join("data_import").join(".env"), true),
    ];

    let mut loaded_any = false;
    for (env_path, should_override) in env_candidates {
        if !env_path.exists() {
            continue;
        }
        let loaded = if should_override {
            dotenvy::from_path_override(&env_path)
        } else {
            dotenvy::from_path(&env_path)
        };
        loaded_any |= loaded.is_ok();
    }

    if !loaded_any {
        dotenv().ok();
    }
}

/*计算“生成实体代码输出到哪里”。它会取 migration 包的上一级目录（也就是 datahandle/ ）
然后拼成： datahandle/src/entities/
也就是最终输出目录： .../datahandle/src/entities*/
fn entities_output_dir() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let datahandle_dir = manifest_dir.parent().ok_or("invalid manifest dir")?;
    Ok(datahandle_dir.join("src").join("entities"))
}

/*- 调用外部命令 sea-orm-cli ，根据 DATABASE_URL 连接数据库，生成 entities Rust 文件。
从环境变量取连接串：let database_url = std::env::var("DATABASE_URL")?;
获取输出目录并确保存在：entities_output_dir() + create_dir_all
运行外部命令：sea-orm-cli generate entity -u <DATABASE_URL> -o <entities_dir> --with-serde both --expanded-format
如果系统里根本没装 sea-orm-cli ， Command::new("sea-orm-cli") 会返回 NotFound 。
这里选择：打印一句 sea-orm-cli not found, skip entity generation ，然后 直接返回 Ok 。
这能保证：迁移已经成功时，不会因为“没装生成工具”导致整个命令失败。*/
fn update_entities() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("DATABASE_URL")?;
    let output_dir = entities_output_dir()?;
    std::fs::create_dir_all(&output_dir)?;

    let status = match std::process::Command::new("sea-orm-cli")
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
        .status()
    {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("sea-orm-cli not found, skip entity generation");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

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
