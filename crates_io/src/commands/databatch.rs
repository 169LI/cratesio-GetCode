//! 功能
//! -
//! 数据预处理/导入批处理模块。
//!
//! 用途
//! -
//! - 预留用于执行数据导入、清洗、数据结构转换等批处理任务
//! - 数据库相关读写通过 `PgDataHandle` 完成
//!
//! 已实现
//! -
//! - 导入基础数据（`import-base`）：crate.txt、data.txt
//!

use crate::cli::DataBatchCli;
use crate::config::{self, ConfigLoad};
use crate::pgdatahandle::{CrateImportRow, CrateVersionIndexRow, PgDataHandle};
use anyhow::Context;
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Semaphore;
use tokio::time::{Duration, sleep, timeout};

const VERSION_HANDLE_GROUP_SIZE: usize = 1000;
const HANDLE_VERSION_CONCURRENCY: usize = 16;
const HANDLE_VERSION_MAX_RETRIES: u32 = 3;
const HANDLE_VERSION_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone)]
pub struct ParsedCrateVersionRow {
    pub version: String,
    pub deps: Value,
    pub features2: Option<Value>,
    pub pubtime: Option<NaiveDateTime>,
}

/// 批量数据预处理/导入
/// - 支持导入基础数据（`import-base`）
/// - 可扩展为更多批处理的任务
pub async fn batch_run(db: &PgDataHandle, cli: &DataBatchCli) -> anyhow::Result<()> {
    match cli.category.as_str() {
        "import-base" => import_base(db).await?,
        "handle-version" => handle_version(db).await?,
        other => {
            tracing::warn!(category = %other, "unknown DataBatch category");
        }
    }
    Ok(())
}

/// 批量处理 crate.io_index提供的版本数据填充到数据库
/// - 数据来源目录通过 `.env` 中的 `CRATESIO_INDEX_DIR` 指定
///     - `git clone https://github.com/rust-lang/crates.io-index.git`
///     - `git switch --detach aa05e85408df56286b0c8de5591b70dd7a1ffc19`
async fn handle_version(db: &PgDataHandle) -> anyhow::Result<()> {
    tracing::info!("start handle crate version index");
    let config = config::get_config_once(&ConfigLoad::new())?;
    let index_root = PathBuf::from(config.require("CRATESIO_INDEX_DIR")?);

    let crates = db
        .get_all_unhandled_version_crates()
        .await
        .context("failed to load unhandled crates")?;

    if crates.is_empty() {
        tracing::info!("no unhandled crates found for version handling");
        return Ok(());
    }

    tracing::info!(count = crates.len(), "loaded crates for version handling");

    let mut success_count: u64 = 0;
    let mut failed_count: u64 = 0;
    let mut total_upserted: u64 = 0;

    let semaphore = Arc::new(Semaphore::new(HANDLE_VERSION_CONCURRENCY));

    for (group_no, group) in crates.chunks(VERSION_HANDLE_GROUP_SIZE).enumerate() {
        let group_no: u64 = (group_no + 1).try_into().unwrap_or(u64::MAX);
        tracing::info!(
            group_no,
            group_size = group.len(),
            concurrency = HANDLE_VERSION_CONCURRENCY,
            "start handling crate version group"
        );

        let mut tasks = Vec::with_capacity(group.len());

        for crate_model in group {
            let db = db.clone();
            let index_root = index_root.clone();
            let semaphore = semaphore.clone();
            let crate_id = crate_model.id;
            let crate_name = crate_model.name.clone();

            let task = tokio::spawn(async move {
                let permit = semaphore.acquire().await;
                if permit.is_err() {
                    return (
                        crate_id,
                        crate_name,
                        Err(anyhow::anyhow!("failed to acquire semaphore permit")),
                        Err(anyhow::anyhow!("skipped mark handled due to permit error")),
                    );
                }
                let permit = permit.unwrap();

                let process_result = handle_single_crate_version_with_retry_and_timeout(
                    &db,
                    &index_root,
                    crate_id,
                    &crate_name,
                )
                .await;

                drop(permit);

                let mark_result = db
                    .mark_crate_version_handled(crate_id)
                    .await
                    .map_err(|e| anyhow::anyhow!(e));

                (crate_id, crate_name, process_result, mark_result)
            });

            tasks.push(task);
        }

        for task in tasks {
            match task.await {
                Ok((crate_id, crate_name, process_result, mark_result)) => {
                    match process_result {
                        Ok(upserted) => {
                            success_count += 1;
                            total_upserted += upserted;
                        }
                        Err(err) => {
                            failed_count += 1;
                            tracing::error!(
                                crate_id,
                                crate_name = %crate_name,
                                error = ?err,
                                "failed to handle crate version index"
                            );
                        }
                    }

                    if let Err(err) = mark_result {
                        tracing::error!(
                            crate_id,
                            crate_name = %crate_name,
                            error = ?err,
                            "failed to mark crate as version handled"
                        );
                    }
                }
                Err(err) => {
                    failed_count += 1;
                    tracing::error!(error = ?err, "handle-version task panicked");
                }
            }
        }
    }

    tracing::info!(
        success_count,
        failed_count,
        total_upserted,
        "crate version batch finished"
    );

    Ok(())
}

async fn handle_single_crate_version_with_retry_and_timeout(
    db: &PgDataHandle,
    index_root: &Path,
    crate_id: i32,
    crate_name: &str,
) -> anyhow::Result<u64> {
    let backoffs = [200u64, 500u64, 1000u64];

    for attempt in 1..=HANDLE_VERSION_MAX_RETRIES {
        let result = timeout(
            Duration::from_secs(HANDLE_VERSION_TIMEOUT_SECS),
            process_single_crate_version_with_index_root(db, index_root, crate_id, crate_name),
        )
        .await;

        match result {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => {
                if attempt >= HANDLE_VERSION_MAX_RETRIES {
                    return Err(e);
                }
                let backoff_ms = backoffs
                    .get((attempt - 1) as usize)
                    .copied()
                    .unwrap_or(*backoffs.last().unwrap_or(&1000));
                tracing::warn!(
                    crate_id,
                    crate_name,
                    attempt,
                    backoff_ms,
                    error = ?e,
                    "handle crate failed, retrying"
                );
                sleep(Duration::from_millis(backoff_ms)).await;
            }
            Err(_) => {
                if attempt >= HANDLE_VERSION_MAX_RETRIES {
                    return Err(anyhow::anyhow!(
                        "handle crate timeout after {}s",
                        HANDLE_VERSION_TIMEOUT_SECS
                    ));
                }
                let backoff_ms = backoffs
                    .get((attempt - 1) as usize)
                    .copied()
                    .unwrap_or(*backoffs.last().unwrap_or(&1000));
                tracing::warn!(
                    crate_id,
                    crate_name,
                    attempt,
                    backoff_ms,
                    "handle crate timeout, retrying"
                );
                sleep(Duration::from_millis(backoff_ms)).await;
            }
        }
    }

    Err(anyhow::anyhow!("unreachable"))
}

async fn process_single_crate_version_with_index_root(
    db: &PgDataHandle,
    index_root: &Path,
    crate_id: i32,
    crate_name: &str,
) -> anyhow::Result<u64> {
    let parsed_rows = load_crate_versions_from_index(index_root, crate_name).await?;
    let upsert_rows = parsed_rows
        .into_iter()
        .map(|row| CrateVersionIndexRow {
            crate_id,
            version: row.version,
            deps: row.deps,
            features2: row.features2,
            pubtime: row.pubtime,
        })
        .collect::<Vec<_>>();

    let upserted = db
        .upsert_crate_versions_index_rows(upsert_rows)
        .await
        .with_context(|| format!("failed to upsert crate_versions_index for {}", crate_name))?;

    Ok(upserted)
}

// 从 index 根目录中定位并解析单个 crate 文件
pub async fn load_crate_versions_from_index(
    index_root: &Path,
    crate_name: &str,
) -> anyhow::Result<Vec<ParsedCrateVersionRow>> {
    let index_file = index_root.join(crate_name_to_index_rel_path(crate_name));
    parse_crate_versions_from_file(&index_file).await
}

// 直接解析指定的 crates.io-index 文件
// 适合手动传入路径查看单个 crate 的处理结果
pub async fn parse_crate_versions_from_file(
    index_file: &Path,
) -> anyhow::Result<Vec<ParsedCrateVersionRow>> {
    if !index_file.exists() {
        return Err(anyhow::anyhow!("Missing file: {}", index_file.display()));
    }

    let file = fs::File::open(index_file)
        .await
        .with_context(|| format!("failed to open {}", index_file.display()))?;
    let mut lines = BufReader::new(file).lines();
    let mut rows = Vec::new();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(row) = parse_index_version_line(line)? {
            rows.push(row);
        }
    }

    Ok(rows)
}

fn parse_index_version_line(line: &str) -> anyhow::Result<Option<ParsedCrateVersionRow>> {
    let value: Value = serde_json::from_str(line).context("failed to parse index json line")?;
    let version = match value.get("vers").and_then(Value::as_str) {
        Some(version) if !version.is_empty() => version.to_string(),
        _ => return Ok(None),
    };

    if value.get("yanked").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }

    let deps = extract_minimal_deps_json(value.get("deps"));
    let features2 = value.get("features2").cloned();
    let pubtime = value
        .get("pubtime")
        .and_then(Value::as_str)
        .and_then(parse_pubtime_rfc3339);

    Ok(Some(ParsedCrateVersionRow {
        version,
        deps,
        features2,
        pubtime,
    }))
}

fn extract_minimal_deps_json(deps: Option<&Value>) -> Value {
    let items = deps
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(|dep| {
                    let name = dep.get("name").and_then(Value::as_str)?;
                    let req = dep.get("req").and_then(Value::as_str)?;
                    let kind = dep.get("kind").and_then(Value::as_str).unwrap_or("normal");

                    let mut obj = Map::new();
                    obj.insert("name".to_string(), Value::String(name.to_string()));
                    obj.insert("req".to_string(), Value::String(req.to_string()));
                    obj.insert("kind".to_string(), Value::String(kind.to_string()));
                    Some(Value::Object(obj))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Value::Array(items)
}

fn parse_pubtime_rfc3339(raw: &str) -> Option<NaiveDateTime> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).naive_utc())
}

fn crate_name_to_index_rel_path(crate_name: &str) -> PathBuf {
    let crate_name = crate_name.to_ascii_lowercase();
    let name = crate_name.as_str();

    match name.len() {
        0 => PathBuf::new(),
        1 => PathBuf::from("1").join(name),
        2 => PathBuf::from("2").join(name),
        3 => PathBuf::from("3").join(&name[..1]).join(name),
        _ => PathBuf::from(&name[..2]).join(&name[2..4]).join(name),
    }
}

/// 导入基础数据
/// - 从 `data.txt` 和 `crates.txt` 导入基础数据
/// - 需要注意，需要先进行数据库迁移建表后，再运行此命令
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
            version_handled: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_to_index_rel_path_is_lowercase() {
        assert_eq!(
            crate_name_to_index_rel_path("GRE_dictation")
                .to_string_lossy()
                .replace('\\', "/"),
            "gr/e_/gre_dictation"
        );
    }

    #[test]
    fn parse_index_version_line_extracts_minimal_fields() {
        let line = r#"{"name":"a","vers":"0.1.0","deps":[{"name":"base64","req":"^0.22","features":[],"optional":true,"default_features":true,"target":null,"kind":"normal"},{"name":"chrono","req":"^0.4","features":["serde"],"optional":false,"default_features":true,"target":null,"kind":"normal"}],"features2":{"serde":["dep:serde","chrono/serde"]},"yanked":false,"pubtime":"2026-01-15T12:53:12Z","v":2}"#;

        let parsed = parse_index_version_line(line).unwrap().unwrap();
        assert_eq!(parsed.version, "0.1.0");

        let deps = parsed.deps.as_array().unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].get("name").and_then(Value::as_str), Some("base64"));
        assert_eq!(deps[0].get("req").and_then(Value::as_str), Some("^0.22"));
        assert_eq!(deps[0].get("kind").and_then(Value::as_str), Some("normal"));

        assert!(parsed.features2.is_some());

        let pubtime = parsed.pubtime.unwrap();
        assert_eq!(
            pubtime,
            NaiveDateTime::parse_from_str("2026-01-15 12:53:12", "%Y-%m-%d %H:%M:%S").unwrap()
        );
    }

    #[test]
    fn parse_index_version_line_without_vers_returns_none() {
        let line = r#"{"name":"a","deps":[]}"#;
        let parsed = parse_index_version_line(line).unwrap();
        assert!(parsed.is_none());
    }

    #[tokio::test]
    async fn parse_crate_versions_from_file_skips_yanked_lines() {
        let lines = [
            r#"{"name":"a","vers":"0.1.0","deps":[],"yanked":true,"v":2}"#,
            r#"{"name":"a","vers":"0.2.0","deps":[],"yanked":false,"v":2}"#,
            r#"{"name":"a","vers":"0.3.0","deps":[],"yanked":true,"v":2}"#,
        ]
        .join("\n");
        let file_path =
            std::env::temp_dir().join(format!("crates_io_index_test_{}.json", std::process::id()));
        std::fs::write(&file_path, lines).unwrap();

        let parsed = parse_crate_versions_from_file(&file_path).await.unwrap();
        let _ = std::fs::remove_file(&file_path);

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].version, "0.2.0");
    }
}
