//! 功能
//! -
//! 编译下载的 crate，并对单个依赖进行不同版本的更新和编译验证。

use crate::commands::download::get_crate_file_path;
use crate::config::{self};
use crate::pgdatahandle::PgDataHandle;
use anyhow::Context;
use semver::{Version, VersionReq};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Semaphore;

const COMPILE_BATCH_SIZE: u64 = 1000; //每次从数据库拉取未编译的 crate 数量
const COMPILE_GROUP_SIZE: usize = 1; // 每次取COMPILE_GROUP_SIZE作为一个并发组
const COMPILE_CONCURRENCY: usize = 1; // 编译非常吃 CPU，并发数不宜过高

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepType {
    Normal,
    Dev,
    Build,
}

#[derive(Debug, Clone)]
#[allow(unused)]
pub struct DependencyInfo {
    pub name: String,
    pub dep_type: DepType,
    pub req: String,
}

#[derive(Debug, Default)]
struct IndexDepsByKind {
    normal: Vec<DependencyInfo>,
    build: Vec<DependencyInfo>,
    dev: Vec<DependencyInfo>,
}

/// 确保指定的 rust toolchain 已安装，避免并发时多个进程同时触发 rustup 导致报错
#[allow(unused)]
async fn ensure_rust_toolchain(toolchain: &str) -> anyhow::Result<()> {
    // 1. 先检查是否已安装
    let list_output = Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .context("failed to execute rustup toolchain list")?;

    if list_output.status.success() {
        let stdout = String::from_utf8_lossy(&list_output.stdout);
        // rustup toolchain list 输出格式通常为: "1.95.0-x86_64-pc-windows-msvc (default)"
        if stdout.lines().any(|line| line.starts_with(toolchain)) {
            tracing::info!("Rust toolchain {} is already installed.", toolchain);
            return Ok(());
        }
    }

    tracing::info!("Ensuring rust toolchain {} is installed...", toolchain);

    // 2. 如果没安装，再执行安装
    let status = Command::new("rustup")
        .args(["toolchain", "install", toolchain])
        .status()
        .context("failed to execute rustup toolchain install")?;

    if !status.success() {
        return Err(anyhow::anyhow!(
            "Failed to install rust toolchain: {}",
            toolchain
        ));
    }

    tracing::info!("Rust toolchain {} is ready.", toolchain);
    Ok(())
}

/// 并发编译框架入口
///
/// 从数据库批量拉取待编译的 crate，按组并发执行单 crate 的编译与依赖升级实验，
/// 并使用隔离的 CARGO_HOME 避免污染全局缓存。
pub async fn compile_run(db: &PgDataHandle) -> anyhow::Result<()> {
    // 提前确保所需的 toolchain 已经安装，防止并发编译时触发 rustup 导致冲突
    // ensure_rust_toolchain("1.95.0-x86_64-pc-windows-msvc").await?;

    let download_dir =
        config::get_config_once(&config::ConfigLoad::new())?.require("DOWNLOAD_DIR")?;
    let download_dir = PathBuf::from(download_dir);

    // 为编译实验设置独立的 CARGO_HOME，防止污染全局缓存和降低全局锁争用
    let experiment_cargo_home = download_dir.join(".cargo_experiment_home");
    if !experiment_cargo_home.exists() {
        fs::create_dir_all(&experiment_cargo_home).await?;
    }
    tracing::info!(
        "using isolated CARGO_HOME for experiments: {}",
        experiment_cargo_home.display()
    );

    tracing::info!("start compile batch run");

    let semaphore = Arc::new(Semaphore::new(COMPILE_CONCURRENCY));

    loop {
        let crates = db
            .get_uncompiled_crates(COMPILE_BATCH_SIZE)
            .await
            .context("failed to load uncompiled crates")?;

        if crates.is_empty() {
            break;
        }

        for (group_no, group) in crates.chunks(COMPILE_GROUP_SIZE).enumerate() {
            let group_no: u64 = (group_no + 1).try_into().unwrap_or(u64::MAX);
            tracing::info!(
                group_no,
                group_size = group.len(),
                concurrency = COMPILE_CONCURRENCY,
                "start handling compile group"
            );

            let mut tasks = Vec::with_capacity(group.len());

            for crate_model in group {
                let db = db.clone();
                let download_dir = download_dir.to_path_buf();
                let cargo_home = experiment_cargo_home.clone();
                let semaphore = semaphore.clone();

                // 克隆所需数据用于 spawned task
                let crate_id = crate_model.id;
                let crate_name = crate_model.name.clone();
                let version_new = crate_model.version_new.clone();

                let task = tokio::spawn(async move {
                    let _permit = match semaphore.acquire().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };

                    let crate_dir = get_crate_file_path(&download_dir, &crate_name, &version_new)
                        .with_extension("");
                    let process_result = process_single_crate_compile(
                        &db,
                        &download_dir,
                        &cargo_home,
                        crate_id,
                        &crate_name,
                        &version_new,
                    )
                    .await;

                    if let Err(err) = process_result {
                        tracing::error!(
                            crate_id,
                            crate_name = %crate_name,
                            error = ?err,
                            "failed to compile crate"
                        );
                    }

                    if crate_dir.exists() {
                        if let Err(err) = cargo_clean(&crate_dir, &cargo_home).await {
                            tracing::warn!(
                                crate_id,
                                crate_name = %crate_name,
                                error = ?err,
                                "failed to cargo clean crate dir"
                            );
                        }
                    }

                    // 标记这个crate已经完成编译流程
                    if let Err(err) = db.mark_crate_compile_handled(crate_id).await {
                        tracing::error!(
                            crate_id,
                            crate_name = %crate_name,
                            error = ?err,
                            "failed to mark crate as compile handled"
                        );
                    }
                });

                tasks.push(task);
            }

            for task in tasks {
                if let Err(err) = task.await {
                    tracing::error!(error = ?err, "compile task panicked");
                }
            }

            tracing::info!(
                group_no,
                "group finished, cleaning up isolated CARGO_HOME cache"
            );
            cleanup_cargo_cache(&experiment_cargo_home).await;
        }
    }

    tracing::info!("compile batch finished, removing isolated CARGO_HOME entirely");
    let _ = fs::remove_dir_all(&experiment_cargo_home).await;

    Ok(())
}

async fn cargo_clean(crate_dir: &Path, cargo_home: &Path) -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .args(["+1.95.0", "clean"])
        .current_dir(crate_dir)
        .env("CARGO_HOME", cargo_home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("cargo clean failed"))
    }
}

/// 清理给定 CARGO_HOME 中的源码缓存，保留 index 索引以加速后续构建
async fn cleanup_cargo_cache(cargo_home: &Path) {
    let registry_cache = cargo_home.join("registry").join("cache");
    let registry_src = cargo_home.join("registry").join("src");
    let git_checkouts = cargo_home.join("git").join("checkouts");

    let _ = fs::remove_dir_all(&registry_cache).await;
    let _ = fs::remove_dir_all(&registry_src).await;
    let _ = fs::remove_dir_all(&git_checkouts).await;
}

/// 核心：单个 Crate 的编译验证与依赖更新
///
/// 对单个 crate 执行：
/// - 从下载目录定位源码
/// - 从数据库读取最新版本依赖索引
/// - 先做 baseline 编译验证
/// - 再逐依赖逐版本执行 update + verify，并写回失败数据集
///
/// 参数：
/// - db: 数据库句柄，用于查询和更新 crate 数据
/// - download_dir: 下载目录路径，用于查找 crate 目录
/// - cargo_home: 独立使用的 CARGO_HOME 目录，避免污染全局
/// - crate_id: 要编译的 crate 的 ID
/// - crate_name: 要编译的 crate 的名称
/// - latest_version: 要编译的 crate 的最新版本
///
async fn process_single_crate_compile(
    db: &PgDataHandle,
    download_dir: &Path,
    cargo_home: &Path,
    crate_id: i32,
    crate_name: &str,
    latest_version: &str,
) -> anyhow::Result<()> {
    // 1. 查找下载目录中的对应版本目录
    let crate_archive_path = get_crate_file_path(download_dir, crate_name, latest_version);
    let crate_dir = crate_archive_path.with_extension("");

    if !crate_dir.exists() || !crate_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Crate directory not found: {}",
            crate_dir.display()
        ));
    }

    tracing::info!("开始验证 Crate: {}", crate_dir.display());

    // 2. 从数据库查询该版本的依赖信息
    let index_row = db
        .get_crate_latest_dependencies(crate_id, latest_version)
        .await?;

    let deps_json = match index_row {
        Some(row) => row.deps,
        None => {
            tracing::warn!(
                "未找到 {} v{} 的依赖索引信息，跳过",
                crate_name,
                latest_version
            );
            return Ok(());
        }
    };

    let deps_by_kind = collect_deps_by_kind_from_index_deps(&deps_json);

    if deps_by_kind.normal.is_empty()
        && deps_by_kind.build.is_empty()
        && deps_by_kind.dev.is_empty()
    {
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            "no dependencies found in index deps, skip baseline and update experiments"
        );
        return Ok(());
    }

    let baseline_ok =
        initial_compile_check(db, crate_id, &crate_dir, cargo_home, &deps_by_kind).await?;

    // 2. 初始编译验证 (Baseline)
    if !baseline_ok {
        tracing::warn!("初始编译失败，跳过依赖更新实验: {}", crate_name);
        db.mark_initial_compile_failed(crate_id).await?;
        return Ok(());
    }

    //3. 逐版本升级实验
    let summary =
        run_stepwise_upgrade_experiments(db, &crate_dir, cargo_home, &deps_by_kind).await?;

    db.record_compile_result(crate_id, summary.errors_json)
        .await?;

    // // 4. 依赖更新验证
    // let mut success_count = 0;
    // let mut failed_count = 0;
    // let mut update_errors = serde_json::Map::new();

    // // 5. 记录最终结果到数据库
    // db.record_compile_result(
    //     crate_id,
    //     success_count,
    //     failed_count,
    //     if update_errors.is_empty() {
    //         None
    //     } else {
    //         Some(serde_json::Value::Object(update_errors))
    //     },
    // )
    // .await?;

    Ok(())
}

/// 从 index deps 的 JSON 中提取依赖信息，并按 normal/build/dev 分类汇总。
fn collect_deps_by_kind_from_index_deps(deps_json: &serde_json::Value) -> IndexDepsByKind {
    let deps = match deps_json.as_array() {
        Some(v) => v,
        None => return IndexDepsByKind::default(),
    };

    let mut out = IndexDepsByKind::default();

    for dep in deps {
        let name = match dep.get("name").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v,
            _ => continue,
        };
        let req = dep.get("req").and_then(|v| v.as_str()).unwrap_or("");

        let kind = dep.get("kind").and_then(|v| v.as_str()).unwrap_or("normal");
        let dep_type = match kind {
            "dev" | "development" => DepType::Dev,
            "build" => DepType::Build,
            "normal" => DepType::Normal,
            _ => DepType::Normal,
        };

        let info = DependencyInfo {
            name: name.to_string(),
            dep_type: dep_type.clone(),
            req: req.to_string(),
        };

        match dep_type {
            DepType::Normal => out.normal.push(info),
            DepType::Build => out.build.push(info),
            DepType::Dev => out.dev.push(info),
        }
    }

    out
}

/// 对 crate 当前状态做 baseline 编译验证，并生成/刷新 baseline 的 Cargo.toml/Cargo.lock 备份。
async fn initial_compile_check(
    db: &PgDataHandle,
    crate_id: i32,
    crate_dir: &Path,
    cargo_home: &Path,
    deps_by_kind: &IndexDepsByKind,
) -> anyhow::Result<bool> {
    let cargo_toml = crate_dir.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Ok(false);
    }

    let cargo_lock = crate_dir.join("Cargo.lock");
    let cargo_lock_exists = if cargo_lock.exists() { 1 } else { 2 };
    db.record_cargo_lock_exists(crate_id, cargo_lock_exists)
        .await?;

    let need_build = !deps_by_kind.normal.is_empty() || !deps_by_kind.build.is_empty();
    let need_dev = !deps_by_kind.dev.is_empty();
    if !need_build && !need_dev {
        return Ok(true);
    }

    let cargo_lock_baseline = crate_dir.join("Cargo.lock.baseline");

    if cargo_lock_baseline.exists() {
        let _ = fs::remove_file(&cargo_lock_baseline).await;
    }

    if cargo_lock.exists() {
        fs::copy(&cargo_lock, &cargo_lock_baseline).await?;

        let status = Command::new("cargo")
            .args(["+1.95.0", "build", "--locked"])
            .current_dir(crate_dir)
            .env("CARGO_HOME", cargo_home)
            .status()?;
        if !status.success() {
            return Ok(false);
        }
    } else {
        let init_status = Command::new("cargo")
            .args(["+1.95.0", "build"])
            .current_dir(crate_dir)
            .env("CARGO_HOME", cargo_home)
            .status()?;
        if !init_status.success() || !cargo_lock.exists() {
            return Ok(false);
        }
        fs::copy(&cargo_lock, &cargo_lock_baseline).await?;
    }

    if need_dev {
        let test_status = Command::new("cargo")
            .args(["+1.95.0", "test", "--no-run", "--locked"])
            .current_dir(crate_dir)
            .env("CARGO_HOME", cargo_home)
            .status()?;
        if !test_status.success() {
            return Ok(false);
        }

        let bench_status = Command::new("cargo")
            .args(["+1.95.0", "bench", "--no-run", "--locked"])
            .current_dir(crate_dir)
            .env("CARGO_HOME", cargo_home)
            .status()?;
        if !bench_status.success() {
            return Ok(false);
        }
    }
    let cargo_toml_baseline = crate_dir.join("Cargo.toml.baseline");
    if cargo_toml_baseline.exists() {
        let _ = fs::remove_file(&cargo_toml_baseline).await;
    }
    fs::copy(&cargo_toml, &cargo_toml_baseline).await?;

    Ok(true)
}

#[derive(Debug, Default)]
struct UpgradeExperimentSummary {
    errors_json: Option<sea_orm::prelude::Json>,
}

/// 逐依赖、逐版本执行 `cargo update --precise` 后再编译验证，只记录 update 成功但 verify 失败的 target_version。
async fn run_stepwise_upgrade_experiments(
    db: &PgDataHandle,
    crate_dir: &Path,
    cargo_home: &Path,
    deps_by_kind: &IndexDepsByKind,
) -> anyhow::Result<UpgradeExperimentSummary> {
    let deps = flatten_index_deps(deps_by_kind);
    if deps.is_empty() {
        return Ok(UpgradeExperimentSummary::default());
    }

    ensure_baseline_files(crate_dir).await?; //多余

    let mut errors = serde_json::Map::new();

    for dep in deps {
        let version_req = match VersionReq::parse(&dep.req) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    dep_name = %dep.name,
                    dep_req = %dep.req,
                    error = %err,
                    "failed to parse dependency version requirement, skip this dependency"
                );
                continue;
            }
        };

        let versions = match get_available_versions(db, &dep.name, &version_req).await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    dep_name = %dep.name,
                    error = ?err,
                    "failed to fetch available versions, skip this dependency"
                );
                continue;
            }
        };

        for version in versions {
            restore_baseline_project_state(crate_dir).await?;

            let update_ok = cargo_update_precise(crate_dir, cargo_home, &dep.name, &version)
                .await
                .with_context(|| {
                    format!(
                        "cargo update -p {} --precise {} failed to execute",
                        dep.name, version
                    )
                })?;

            if !update_ok {
                continue;
            }

            let verify_ok = verify_after_update(crate_dir, cargo_home, &dep.dep_type).await?;

            if verify_ok {
                continue;
            }

            let dep_key = dep.name.clone();
            let entry = errors.entry(dep_key).or_insert_with(|| {
                serde_json::json!({
                    "dep_type": dep_type_to_str(&dep.dep_type),
                    "req": dep.req,
                    "failed_targets": []
                })
            });

            if let Some(arr) = entry
                .get_mut("failed_targets")
                .and_then(|v| v.as_array_mut())
            {
                if !arr.iter().any(|v| v.as_str() == Some(&version)) {
                    arr.push(serde_json::Value::String(version));
                }
            }
        }
    }

    Ok(UpgradeExperimentSummary {
        errors_json: if errors.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(errors))
        },
    })
}

/// 把 normal/build/dev 三类依赖合并成一个列表，便于统一遍历。
fn flatten_index_deps(deps_by_kind: &IndexDepsByKind) -> Vec<DependencyInfo> {
    let mut out = Vec::with_capacity(
        deps_by_kind.normal.len() + deps_by_kind.build.len() + deps_by_kind.dev.len(),
    );
    out.extend(deps_by_kind.normal.iter().cloned());
    out.extend(deps_by_kind.build.iter().cloned());
    out.extend(deps_by_kind.dev.iter().cloned());
    out
}

/// 确保 Cargo.toml.baseline 与 Cargo.lock.baseline 存在（缺失则从当前文件复制生成）
async fn ensure_baseline_files(crate_dir: &Path) -> anyhow::Result<()> {
    let cargo_toml = crate_dir.join("Cargo.toml");
    let cargo_lock = crate_dir.join("Cargo.lock");
    let cargo_toml_baseline = crate_dir.join("Cargo.toml.baseline");
    let cargo_lock_baseline = crate_dir.join("Cargo.lock.baseline");

    if !cargo_toml_baseline.exists() {
        fs::copy(&cargo_toml, &cargo_toml_baseline)
            .await
            .with_context(|| format!("failed to create {}", cargo_toml_baseline.display()))?;
    }

    if !cargo_lock_baseline.exists() {
        if !cargo_lock.exists() {
            return Err(anyhow::anyhow!(
                "missing Cargo.lock after initial compile check: {}",
                cargo_lock.display()
            ));
        }
        fs::copy(&cargo_lock, &cargo_lock_baseline)
            .await
            .with_context(|| format!("failed to create {}", cargo_lock_baseline.display()))?;
    }

    Ok(())
}

/// 将 Cargo.toml/Cargo.lock 还原到 baseline 状态，保证每轮实验都从同一基线开始
async fn restore_baseline_project_state(crate_dir: &Path) -> anyhow::Result<()> {
    let cargo_toml = crate_dir.join("Cargo.toml");
    let cargo_lock = crate_dir.join("Cargo.lock");
    let cargo_toml_baseline = crate_dir.join("Cargo.toml.baseline");
    let cargo_lock_baseline = crate_dir.join("Cargo.lock.baseline");

    if cargo_toml_baseline.exists() {
        fs::copy(&cargo_toml_baseline, &cargo_toml).await?;
    }

    if cargo_lock_baseline.exists() {
        fs::copy(&cargo_lock_baseline, &cargo_lock).await?;
    }

    Ok(())
}

/// 执行 `cargo update -p <dep> --precise <target_version>`，返回更新是否成功
async fn cargo_update_precise(
    crate_dir: &Path,
    cargo_home: &Path,
    dep_name: &str,
    target_version: &str,
) -> anyhow::Result<bool> {
    let status = Command::new("cargo")
        .args([
            "+1.95.0",
            "update",
            "-p",
            dep_name,
            "--precise",
            target_version,
        ])
        .current_dir(crate_dir)
        .env("CARGO_HOME", cargo_home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Ok(status.success())
}

/// 更新依赖后执行编译验证：normal/build 跑 build，dev 跑 test+bench（均使用 --locked）
async fn verify_after_update(
    crate_dir: &Path,
    cargo_home: &Path,
    dep_type: &DepType,
) -> anyhow::Result<bool> {
    match dep_type {
        DepType::Normal | DepType::Build => {
            let status = Command::new("cargo")
                .args(["+1.95.0", "build", "--locked"])
                .current_dir(crate_dir)
                .env("CARGO_HOME", cargo_home)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            Ok(status.success())
        }
        DepType::Dev => {
            let test_status = Command::new("cargo")
                .args(["+1.95.0", "test", "--no-run", "--locked"])
                .current_dir(crate_dir)
                .env("CARGO_HOME", cargo_home)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;

            if !test_status.success() {
                return Ok(false);
            }

            let bench_status = Command::new("cargo")
                .args(["+1.95.0", "bench", "--no-run", "--locked"])
                .current_dir(crate_dir)
                .env("CARGO_HOME", cargo_home)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;

            Ok(bench_status.success())
        }
    }
}

/// 将 DepType 转成字符串标签，便于写入 JSON。
fn dep_type_to_str(dep_type: &DepType) -> &'static str {
    match dep_type {
        DepType::Normal => "normal",
        DepType::Dev => "dev",
        DepType::Build => "build",
    }
}

/// 通过 数据库 + crates.io API  获取依赖可升级的版本列表
async fn get_available_versions(
    db: &PgDataHandle,
    crate_name: &str,
    req: &VersionReq,
) -> anyhow::Result<Vec<String>> {
    let versions = if let Some(versions) = db.get_available_versions_from_db(crate_name).await? {
        versions
    } else {
        get_available_versions_via_api(crate_name).await?
    };

    Ok(filter_versions_by_req(versions, req))
}

/// 过滤版本列表：保留能解析为 semver 且满足给定 VersionReq 的版本。
fn filter_versions_by_req(versions: Vec<String>, req: &VersionReq) -> Vec<String> {
    versions
        .into_iter()
        .filter(|v| Version::parse(v).is_ok_and(|ver| req.matches(&ver)))
        .collect()
}

/// 通过 crates.io HTTP API 获取指定 crate 的所有非 yanked 版本号。
async fn get_available_versions_via_api(crate_name: &str) -> anyhow::Result<Vec<String>> {
    let url = format!("https://crates.io/api/v1/crates/{}", crate_name);

    let client = reqwest::Client::builder()
        .user_agent("cratesio-GetCode-Bot (https://github.com/rust-lang)")
        .build()?;

    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "获取 crate {} 信息失败: HTTP {}",
            crate_name,
            resp.status()
        ));
    }

    let json: serde_json::Value = resp.json().await?;

    let mut versions = Vec::new();
    if let Some(versions_array) = json.get("versions").and_then(|v| v.as_array()) {
        for v in versions_array {
            // 跳过 yanked 版本
            if let Some(yanked) = v.get("yanked").and_then(|y| y.as_bool()) {
                if yanked {
                    continue;
                }
            }
            if let Some(num) = v.get("num").and_then(|n| n.as_str()) {
                versions.push(num.to_string());
            }
        }
    }

    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 semver 约束（caret 等）对版本列表的过滤行为。
    #[test]
    fn test_filter_versions_by_req_caret() {
        let versions = vec![
            "2.0.0".to_string(),
            "1.5.0".to_string(),
            "1.0.51".to_string(),
            "1.0.50".to_string(),
            "0.9.9".to_string(),
        ];

        let req = VersionReq::parse("^1.0.51").unwrap();
        let filtered = filter_versions_by_req(versions, &req);
        assert_eq!(filtered, vec!["1.5.0".to_string(), "1.0.51".to_string()]);

        let versions2 = vec![
            "0.9.9".to_string(),
            "0.10.0".to_string(),
            "0.10.1".to_string(),
            "0.11.0".to_string(),
        ];

        let req2 = VersionReq::parse("^0.10.0").unwrap();
        let filtered2 = filter_versions_by_req(versions2, &req2);
        assert_eq!(filtered2, vec!["0.10.0".to_string(), "0.10.1".to_string()]);
    }

    /// 验证 crates.io API 可用且能返回非空版本列表（用于回退到 API 的场景）。
    #[tokio::test]
    async fn test_get_available_versions_via_api() {
        let crate_name = "log";
        let result = get_available_versions_via_api(crate_name).await;

        assert!(result.is_ok(), "API request failed: {:?}", result.err());
        let versions = result.unwrap();

        assert!(!versions.is_empty(), "versions list should not be empty");
        assert!(
            versions
                .iter()
                .any(|v| v.starts_with('0') || v.starts_with('1') || v.starts_with('2')),
            "versions do not seem to contain valid semver strings: {:?}",
            versions
        );
    }

    /// 验证工具链预装逻辑
    #[tokio::test]
    async fn test_ensure_rust_toolchain() {
        let toolchain = "1.95.0";
        let result = ensure_rust_toolchain(toolchain).await;
        assert!(
            result.is_ok(),
            "确保 toolchain 安装应该成功: {:?}",
            result.err()
        );

        // 再次执行，应该立刻返回成功且不会报错
        let result2 = ensure_rust_toolchain(toolchain).await;
        assert!(
            result2.is_ok(),
            "第二次确保 toolchain 安装应该也成功: {:?}",
            result2.err()
        );
    }
}
