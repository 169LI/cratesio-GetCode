//! 功能
//! -
//! 下载 crates.io 源码的命令模块。
//!
//! 约定
//! -
//! - 命令参数尽量通过环境变量/配置读取，CLI 子命令本身不承载过多参数
//! - 数据库相关读写通过 `PgDataHandle` 完成（后续在此模块内补充具体查询/更新逻辑）

use crate::pgdatahandle::PgDataHandle;

pub async fn download_run(_db: &PgDataHandle) -> anyhow::Result<()> {
    let _ = _db.get_connection();
    println!("download: not implemented yet");
    Ok(())
}
