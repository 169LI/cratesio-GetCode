//! 功能
//! -
//! 构建相关命令模块（占位）。
//!
//! 用途
//! -
//! - 预留与“下载”解耦的后续处理步骤（例如构建/编译/分析等）
//! - 数据库相关读写通过 `PgDataHandle` 完成（具体逻辑后续补充）

use crate::pgdatahandle::PgDataHandle;

pub async fn build_run(_db: &PgDataHandle) -> anyhow::Result<()> {
    let _ = _db.get_connection();
    println!("download: not implemented yet");
    Ok(())
}
