//! 模块说明
//! -
//! 日志初始化模块：基于 tracing 体系同时输出到控制台与文件。
//!
//! 输出位置
//! -
//! - 控制台：stdout
//! - 文件：工作区根目录 `logs/crates_io.log`（按天滚动）
//!
//! 日志级别
//! -
//! - 读取 `RUST_LOG`（例如 `RUST_LOG=debug`）
//! - 未设置时默认 `info`
//! - 级别效果示例：
//!   - `RUST_LOG=debug`：输出 debug/info/warn/error
//!   - `RUST_LOG=info`：输出 info/warn/error（不输出 debug）
//!   - `RUST_LOG=warn`：仅输出 warn/error

use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_subscriber::prelude::*;

static LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

pub fn init_logging() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_env = manifest_dir.parent().unwrap_or(&manifest_dir).join(".env");
    let _ = dotenvy::from_path(workspace_env);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(true);

    let workspace_root = manifest_dir.parent().unwrap_or(&manifest_dir);
    let logs_dir = workspace_root.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);

    let file_appender = tracing_appender::rolling::daily(logs_dir, "crates_io.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_GUARD.set(guard);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer);

    let _ = tracing::subscriber::set_global_default(subscriber);
}
