//! 功能
//! -
//! 数据预处理/导入批处理模块。
//!
//! 用途
//! -
//! - 预留用于执行数据导入、清洗、数据结构转换等批处理任务
//! - 数据库相关读写通过 `PgDataHandle` 完成

use crate::cli::DataBatchCli;
use crate::pgdatahandle::{CrateImportRow, PgDataHandle};
use anyhow::Context;
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};

/// 批量数据预处理/导入
/// - 支持导入基础数据（`import-base`）
/// - 可扩展为更多批处理的任务
pub async fn batch_run(_db: &PgDataHandle, cli: &DataBatchCli) -> anyhow::Result<()> {
    let _ = _db.get_connection();
    match cli.category.as_str() {
        "import-base" => import_base(_db).await?,
        other => {
            tracing::warn!(category = %other, "unknown DataBatch category");
        }
    }
    Ok(())
}

/// 导入基础数据
/// - 从 `data.txt` 和 `crates.txt` 导入基础数据
/// - 需要注意，需要先进行两次数据库迁移建表后，再运行此命令
async fn import_base(db: &PgDataHandle) -> anyhow::Result<()> {
    //确定输入文件路径
    let (data_txt, crates_txt) = default_import_paths();

    //检查文件是否存在
    if !data_txt.exists() {
        return Err(anyhow::anyhow!("Missing file: {}", data_txt.display()));
    }
    if !crates_txt.exists() {
        return Err(anyhow::anyhow!("Missing file: {}", crates_txt.display()));
    }

    tracing::info!(
        data_txt = %data_txt.display(),
        crates_txt = %crates_txt.display(),
        "start base import"
    );

    //构建 “crate -> 最新版本号” 的映射
    let latest_versions =
        load_latest_versions(&data_txt).context("failed to load latest versions")?;
    tracing::info!(count = latest_versions.len(), "loaded latest_versions map");

    let file = fs::File::open(&crates_txt)
        .await
        .with_context(|| format!("failed to open {}", crates_txt.display()))?;
    let mut lines = BufReader::new(file).lines();

    let _header = lines.next_line().await?;

    let mut batch: Vec<CrateImportRow> = Vec::with_capacity(2000);
    let mut total_upserted: u64 = 0;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }

        let raw_id = trim_quotes(parts[0]);
        let name = trim_quotes(parts[1]);
        let updated_raw = trim_quotes(parts[2]);
        let created_raw = trim_quotes(parts[3]);

        let id: i32 = match raw_id.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        let now = Utc::now().naive_utc();
        let created_at = parse_crates_datetime(created_raw).unwrap_or(now);
        let updated_at = parse_crates_datetime(updated_raw).unwrap_or(created_at);
        let version_new = match latest_versions.get(name) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => continue,
        };
        let homepage = Some(format!("https://static.crates.io/crates/{}", name));

        batch.push(CrateImportRow {
            id,
            name: name.to_string(),
            homepage,
            analyzed: false,
            download: false,
            created_at,
            updated_at,
            version_new,
            download_failed: false,
        });

        if batch.len() >= 2000 {
            total_upserted += db
                .upsert_crates_import_rows(std::mem::take(&mut batch))
                .await?;
        }
    }

    if !batch.is_empty() {
        total_upserted += db.upsert_crates_import_rows(batch).await?;
    }

    tracing::info!(total_upserted, "base import upsert done");

    Ok(())
}

fn default_import_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap_or(&manifest_dir);
    let import_dir = workspace_root.join("datahandle").join("data_import");
    (import_dir.join("data.txt"), import_dir.join("crates.txt"))
}

fn trim_quotes(s: &str) -> &str {
    s.trim().trim_matches('"')
}

fn parse_crates_datetime(raw: &str) -> Option<NaiveDateTime> {
    let mut s = trim_quotes(raw).to_string();
    if s.is_empty() {
        return None;
    }

    s = s.replace(".-1f", "");

    if let Some((sign_idx, sign)) = s
        .rfind(|c| c == '+' || c == '-')
        .map(|idx| (idx, s.as_bytes()[idx] as char))
    {
        let tz_part = &s[sign_idx + 1..];
        if tz_part.len() == 2 && tz_part.chars().all(|c| c.is_ascii_digit()) {
            s.push_str("00");
        } else if tz_part.len() == 4 && tz_part.chars().all(|c| c.is_ascii_digit()) {
            let _ = sign;
        }
    }

    let dt = DateTime::parse_from_str(&s, "%d/%m/%Y %H:%M:%S%z")
        .ok()
        .map(|dt: DateTime<FixedOffset>| dt.with_timezone(&Utc).naive_utc());
    dt
}

fn load_latest_versions(data_txt: &Path) -> anyhow::Result<HashMap<String, String>> {
    let file = std::fs::File::open(data_txt)
        .with_context(|| format!("failed to open {}", data_txt.display()))?;

    let mut rdr = csv::Reader::from_reader(file);
    let headers = rdr.headers()?.clone();

    let name_idx = headers
        .iter()
        .position(|h| h == "crate_name")
        .ok_or_else(|| anyhow::anyhow!("missing column crate_name in data.txt"))?;
    let version_idx = headers
        .iter()
        .position(|h| h == "crate_version")
        .ok_or_else(|| anyhow::anyhow!("missing column crate_version in data.txt"))?;

    let mut latest: HashMap<String, String> = HashMap::new();
    for record in rdr.records() {
        let record = record?;
        let name = record.get(name_idx).unwrap_or("").trim();
        let version = record.get(version_idx).unwrap_or("").trim();
        if name.is_empty() || version.is_empty() {
            continue;
        }
        if let Some(prev) = latest.get(name) {
            if prev != version {
                tracing::warn!(
                    crate_name = %name,
                    prev_version = %prev,
                    new_version = %version,
                    "data.txt contains multiple different versions for same crate_name, keep the first one"
                );
            }
            continue;
        }

        latest.insert(name.to_string(), version.to_string());
    }

    Ok(latest)
}
