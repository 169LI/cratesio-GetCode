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
use std::time::Instant;
use tokio::fs;
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex, Semaphore, SemaphorePermit, mpsc};
use tokio::time::{Duration, timeout};

const COMPILE_BATCH_SIZE: u64 = 2000; //每次从数据库拉取未编译的 crate 数量
const HEAVY_DEPS_SKIP_THRESHOLD: usize = 50;
const WORKER_CARGO_HOME_CLEAN_INTERVAL: u64 = 20;
const CARGO_CMD_TIMEOUT_SECS: u64 = 60 * 20;

// 注意：后续在其他服务器上跑时，可通过修改以下两个常量或引入环境变量来调整：
// 推荐公式：COMPILE_CONCURRENCY * CARGO_BUILD_JOBS ≈ CPU逻辑核数 (或略大)
const COMPILE_CONCURRENCY: usize = 10; // 建议值：你的 24 线程(i7-14650HX)机器上，配合 JOBS=3，设为 12 比较稳
const CARGO_BUILD_JOBS: &str = "5"; // 控制每个 cargo build 的内部并发数

/// 辅助函数：清空指定目录下的所有内容（保留目录本身）
async fn cleanup_dir_contents(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        let mut entries = fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(&path).await;
            } else {
                let _ = fs::remove_file(&path).await;
            }
        }
    }
    Ok(())
}

/// 作用
/// -
/// - 运行 cargo 命令，设置超时时间。进行统一封装
async fn run_cargo_status_timeout(
    crate_id: i32,
    crate_name: &str,
    spawned_stage: &'static str,
    timeout_stage: &'static str,
    timeout_secs: u64,
    mut cmd: TokioCommand,
) -> anyhow::Result<std::process::ExitStatus> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(
        crate_id,
        crate_name = %crate_name,
        pid,
        stage = spawned_stage,
        "crate stage"
    );
    match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(res) => Ok(res?),
        Err(_) => {
            tracing::info!(
                crate_id,
                crate_name = %crate_name,
                pid,
                timeout_secs,
                stage = timeout_stage,
                "crate stage"
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(anyhow::anyhow!("cargo_timeout:{}", timeout_stage))
        }
    }
}

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
    tracing::info!(toolchain = %toolchain, stage = "toolchain_check", "checking rust toolchain");
    let list_output = Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .context("failed to execute rustup toolchain list")?;

    if list_output.status.success() {
        let stdout = String::from_utf8_lossy(&list_output.stdout);
        // rustup toolchain list 输出格式通常为: "1.95.0-----"
        if stdout.lines().any(|line| line.starts_with(toolchain)) {
            tracing::info!(toolchain = %toolchain, stage = "toolchain_ready", "rust toolchain already installed");
            return Ok(());
        }
    }

    tracing::info!(toolchain = %toolchain, stage = "toolchain_install", "installing rust toolchain");
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

    tracing::info!(toolchain = %toolchain, stage = "toolchain_ready", "rust toolchain installed");
    Ok(())
}

// 作用
// -
// - 从 cargo 命令信号量中获取一个许可，确保并发数不超过最大限制。避免一个crate内多个cargo同时跑
async fn acquire_cargo_cmd_permit<'a>(
    cargo_cmd_semaphore: &'a Semaphore,
    crate_id: i32,
    crate_name: &str,
    stage: &'static str,
) -> anyhow::Result<SemaphorePermit<'a>> {
    match cargo_cmd_semaphore.try_acquire() {
        Ok(p) => Ok(p),
        Err(_) => {
            tracing::info!(
                crate_id,
                crate_name = %crate_name,
                stage,
                "waiting for cargo command lock"
            );
            let p = cargo_cmd_semaphore
                .acquire()
                .await
                .map_err(|_| anyhow::anyhow!("failed to acquire cargo command permit"))?;
            tracing::info!(
                crate_id,
                crate_name = %crate_name,
                stage,
                "cargo command lock acquired"
            );
            Ok(p)
        }
    }
}

/// 并发编译框架入口
///
/// 从数据库批量拉取待编译的 crate，按组并发执行单 crate 的编译与依赖升级实验，
/// 并使用隔离的 CARGO_HOME 避免污染全局缓存。
///
/// 并发逻辑怎么跑的
/// - 生产者/消费者：主线程持续从 DB 拉取未编译的 crate，并通过 channel 投递给固定数量的 worker。
/// - worker 常驻：每个 worker 处理完一个 crate 就继续从 channel 取下一个，不再按 group 切分，避免尾部慢 crate 让整组空等。
/// - 周期清理：每个 worker 每处理 WORKER_CARGO_HOME_CLEAN_INTERVAL 个 crate 清理一次自己的 CARGO_HOME，target_dir 则每 crate 清理一次。
pub async fn compile_run(db: &PgDataHandle) -> anyhow::Result<()> {
    let config = config::get_config_once()?;
    let download_dir = config.require("DOWNLOAD_DIR")?;
    let download_dir = PathBuf::from(download_dir);

    let cargo_target_base = PathBuf::from(
        config
            .get("CARGO_TARGET_BASE_DIR")
            .unwrap_or_else(|| "/var/tmp/cargo-target"),
    );

    // CARGO_HOME：挪到与 CARGO_TARGET_DIR 同一盘（例如 /var/tmp 在 sdb2），将 registry/src/git 的大量读写从 /mnt/data 分流
    let cargo_home_base = PathBuf::from(
        config
            .get("CARGO_HOME_BASE_DIR")
            .unwrap_or_else(|| "/var/tmp/cargo-home"),
    );
    if !cargo_home_base.exists() {
        fs::create_dir_all(&cargo_home_base).await?;
    }

    // CARGO_TARGET_DIR：挪到独立盘（例如 /var/tmp 在 sdb2），将最重的编译写入从 /mnt/data 分流
    if !cargo_target_base.exists() {
        fs::create_dir_all(&cargo_target_base).await?;
    }

    let experiment_cargo_home = cargo_home_base.join("cratesio-experiment");
    if !experiment_cargo_home.exists() {
        fs::create_dir_all(&experiment_cargo_home).await?;
    }

    let startup_cleanup_start = Instant::now();
    tracing::info!(
        cargo_home_base = %experiment_cargo_home.display(),
        cargo_target_base = %cargo_target_base.display(),
        stage = "startup_cleanup_start",
        "compile stage"
    );
    let _ = cleanup_dir_contents(&experiment_cargo_home).await;
    let _ = cleanup_dir_contents(&cargo_target_base).await;
    tracing::info!(
        cargo_home_base = %experiment_cargo_home.display(),
        cargo_target_base = %cargo_target_base.display(),
        elapsed_ms = startup_cleanup_start.elapsed().as_millis(),
        stage = "startup_cleanup_done",
        "compile stage"
    );

    tracing::info!(
        cargo_home_base = %experiment_cargo_home.display(),
        cargo_target_base = %cargo_target_base.display(),
        "using isolated IO directories for experiments"
    );

    tracing::info!(
        concurrency = COMPILE_CONCURRENCY,
        page_size = COMPILE_BATCH_SIZE,
        "start compile worker pool (no grouping)"
    );

    type CompileItem = (i32, String, String, PathBuf);

    let (tx, rx) = mpsc::channel::<CompileItem>(COMPILE_CONCURRENCY * 2);
    let rx = Arc::new(Mutex::new(rx));

    let producer_db = db.clone();
    let producer_download_dir = download_dir.clone();
    let producer_tx = tx.clone();
    let producer = tokio::spawn(async move {
        let mut after_id: i32 = 0;
        loop {
            let rows = producer_db
                .get_uncompiled_crates_after_id(COMPILE_BATCH_SIZE, after_id)
                .await
                .context("failed to load uncompiled crates page")?;
            if rows.is_empty() {
                tracing::info!(after_id, stage = "producer_done", "compile stage");
                break;
            }
            tracing::info!(
                after_id,
                fetched = rows.len(),
                stage = "producer_fetched",
                "compile stage"
            );
            for row in rows {
                let crate_dir =
                    get_crate_file_path(&producer_download_dir, &row.name, &row.version_new)
                        .with_extension("");
                after_id = row.id;
                if producer_tx
                    .send((row.id, row.name, row.version_new, crate_dir))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut worker_tasks = Vec::with_capacity(COMPILE_CONCURRENCY);
    for worker_id in 0..COMPILE_CONCURRENCY {
        let db = db.clone();
        let rx = rx.clone();
        let cargo_home = experiment_cargo_home.join(format!("worker_{:02}", worker_id));
        let target_dir = cargo_target_base.join(format!("worker_{:02}", worker_id));

        if !cargo_home.exists() {
            fs::create_dir_all(&cargo_home).await?;
        }
        if !target_dir.exists() {
            fs::create_dir_all(&target_dir).await?;
        }

        let task = tokio::spawn(async move {
            let cargo_cmd_semaphore = Semaphore::new(1);
            let mut handled_since_cleanup: u64 = 0;
            loop {
                let item = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };
                let Some((crate_id, crate_name, version_new, crate_dir)) = item else {
                    break;
                };

                let crate_start = Instant::now();
                tracing::info!(
                    crate_id,
                    crate_name = %crate_name,
                    version_new = %version_new,
                    crate_dir = %crate_dir.display(),
                    target_dir = %target_dir.display(),
                    stage = "crate_start",
                    "crate stage"
                );

                let process_result = process_single_crate_compile(
                    &db,
                    &crate_dir,
                    &cargo_home,
                    &target_dir,
                    &cargo_cmd_semaphore,
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
                    if let Err(err) = cargo_clean(
                        crate_id,
                        &crate_name,
                        &crate_dir,
                        &cargo_home,
                        &target_dir,
                        &cargo_cmd_semaphore,
                    )
                    .await
                    {
                        tracing::warn!(
                            crate_id,
                            crate_name = %crate_name,
                            error = ?err,
                            "failed to cargo clean crate dir"
                        );
                    }
                }

                if let Err(err) = db.mark_crate_compile_handled(crate_id).await {
                    tracing::error!(
                        crate_id,
                        crate_name = %crate_name,
                        error = ?err,
                        "failed to mark crate as compile handled"
                    );
                }

                handled_since_cleanup += 1;
                if handled_since_cleanup % WORKER_CARGO_HOME_CLEAN_INTERVAL == 0 {
                    tracing::info!(
                        worker_id,
                        cargo_home = %cargo_home.display(),
                        handled_since_cleanup,
                        stage = "worker_cargo_home_periodic_cleanup_start",
                        "compile stage"
                    );
                    cleanup_cargo_cache(&cargo_home).await;
                    tracing::info!(
                        worker_id,
                        cargo_home = %cargo_home.display(),
                        handled_since_cleanup,
                        stage = "worker_cargo_home_periodic_cleanup_done",
                        "compile stage"
                    );
                }

                tracing::info!(
                    crate_id,
                    crate_name = %crate_name,
                    version_new = %version_new,
                    elapsed_ms = crate_start.elapsed().as_millis(),
                    stage = "crate_done",
                    "crate stage"
                );
            }

            tracing::info!(
                worker_id,
                cargo_home = %cargo_home.display(),
                stage = "worker_cargo_home_cleanup_start",
                "compile stage"
            );
            cleanup_cargo_cache(&cargo_home).await;
            tracing::info!(
                worker_id,
                cargo_home = %cargo_home.display(),
                stage = "worker_cargo_home_cleanup_done",
                "compile stage"
            );
        });
        worker_tasks.push(task);
    }

    drop(tx);

    if let Err(err) = producer.await {
        tracing::error!(error = ?err, "compile producer task panicked");
    }

    for task in worker_tasks {
        if let Err(err) = task.await {
            tracing::error!(error = ?err, "compile worker task panicked");
        }
    }

    tracing::info!("compile run finished");

    Ok(())
}

///作用
/// -
/// - 实现主要产物清理，避免编译产物累计
async fn cargo_clean(
    crate_id: i32,
    crate_name: &str,
    crate_dir: &Path,
    cargo_home: &Path,
    target_dir: &Path,
    cargo_cmd_semaphore: &Semaphore,
) -> anyhow::Result<()> {
    let _ = cargo_cmd_semaphore;
    let _ = cargo_home;
    tracing::info!(
        crate_id,
        crate_name = %crate_name,
        crate_dir = %crate_dir.display(),
        target_dir = %target_dir.display(),
        stage = "target_cleanup_start",
        "crate stage"
    );

    let clean_start = Instant::now();
    let _ = cleanup_dir_contents(target_dir).await;
    tracing::info!(
        crate_id,
        crate_name = %crate_name,
        crate_dir = %crate_dir.display(),
        target_dir = %target_dir.display(),
        elapsed_ms = clean_start.elapsed().as_millis(),
        stage = "target_cleanup_done",
        "crate stage"
    );
    Ok(())
}

/// 清理给定 CARGO_HOME 中的源码缓存，保留 index 索引以加速后续构建
async fn cleanup_cargo_cache(cargo_home: &Path) {
    tracing::info!(
        cargo_home = %cargo_home.display(),
        stage = "cargo_home_cleanup_start",
        "compile stage"
    );
    let registry_cache = cargo_home.join("registry").join("cache");
    let registry_src = cargo_home.join("registry").join("src");
    let git_checkouts = cargo_home.join("git").join("checkouts");

    let _ = fs::remove_dir_all(&registry_cache).await;
    let _ = fs::remove_dir_all(&registry_src).await;
    let _ = fs::remove_dir_all(&git_checkouts).await;
    tracing::info!(
        cargo_home = %cargo_home.display(),
        stage = "cargo_home_cleanup_done",
        "compile stage"
    );
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
/// - crate_dir: crate 解压后的源码目录路径
/// - cargo_home: 独立使用的 CARGO_HOME 目录，避免污染全局
/// - target_dir: 独立使用的 CARGO_TARGET_DIR
/// - crate_id: 要编译的 crate 的 ID
/// - crate_name: 要编译的 crate 的名称
/// - latest_version: 要编译的 crate 的最新版本
///
async fn process_single_crate_compile(
    db: &PgDataHandle,
    crate_dir: &Path,
    cargo_home: &Path,
    target_dir: &Path,
    cargo_cmd_semaphore: &Semaphore,
    crate_id: i32,
    crate_name: &str,
    latest_version: &str,
) -> anyhow::Result<()> {
    if !crate_dir.exists() || !crate_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Crate directory not found: {}",
            crate_dir.display()
        ));
    }

    tracing::info!(
        crate_id,
        crate_name = %crate_name,
        latest_version = %latest_version,
        crate_dir = %crate_dir.display(),
        stage = "crate_enter",
        "crate stage"
    );

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
    let deps_total = deps_by_kind.normal.len() + deps_by_kind.build.len() + deps_by_kind.dev.len();

    if deps_total > HEAVY_DEPS_SKIP_THRESHOLD {
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            latest_version = %latest_version,
            deps_total,
            threshold = HEAVY_DEPS_SKIP_THRESHOLD,
            stage = "skip_heavy_deps",
            "skip crate due to heavy dependencies"
        );
        db.mark_heavy_deps_skipped(crate_id, deps_total.try_into().unwrap_or(i32::MAX))
            .await?;
        return Ok(());
    }

    if deps_by_kind.normal.is_empty()
        && deps_by_kind.build.is_empty()
        && deps_by_kind.dev.is_empty()
    {
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            stage = "skip_no_deps",
            "no dependencies found in index deps, skip baseline and update experiments"
        );
        return Ok(());
    }

    let baseline_ok = initial_compile_check(
        db,
        crate_id,
        crate_name,
        crate_dir,
        cargo_home,
        target_dir,
        cargo_cmd_semaphore,
        &deps_by_kind,
    )
    .await?;

    // 2. 初始编译验证 (Baseline)
    if !baseline_ok {
        tracing::warn!("初始编译失败，跳过依赖更新实验: {}", crate_name);
        db.mark_initial_compile_failed(crate_id).await?;
        return Ok(());
    }

    //3. 逐版本升级实验  调整为：每个依赖只测当前项目下允许更新到的最新的一个版本
    let summary = run_stepwise_upgrade_experiments(
        db,
        crate_id,
        crate_name,
        &crate_dir,
        cargo_home,
        target_dir,
        cargo_cmd_semaphore,
        &deps_by_kind,
    )
    .await?;

    db.record_compile_result(crate_id, summary.errors_json)
        .await?;

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
    crate_name: &str,
    crate_dir: &Path,
    cargo_home: &Path,
    target_dir: &Path,
    cargo_cmd_semaphore: &Semaphore,
    deps_by_kind: &IndexDepsByKind,
) -> anyhow::Result<bool> {
    let cargo_toml = crate_dir.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Ok(false);
    }

    let cargo_lock = crate_dir.join("Cargo.lock");
    let cargo_lock_baseline = crate_dir.join("Cargo.lock.baseline");
    let cargo_lock_baseline_exists = cargo_lock_baseline.exists();
    let cargo_lock_exists = if cargo_lock.exists() { 1 } else { 2 };
    if !cargo_lock_baseline_exists {
        db.record_cargo_lock_exists(crate_id, cargo_lock_exists)
            .await?;
    }

    let need_build = !deps_by_kind.normal.is_empty() || !deps_by_kind.build.is_empty();
    let need_dev = !deps_by_kind.dev.is_empty();
    if !need_build && !need_dev {
        return Ok(true);
    }

    if cargo_lock_baseline.exists() {
        let _ = fs::remove_file(&cargo_lock_baseline).await;
    }

    if cargo_lock.exists() {
        fs::copy(&cargo_lock, &cargo_lock_baseline).await?;

        let _permit =
            acquire_cargo_cmd_permit(cargo_cmd_semaphore, crate_id, crate_name, "baseline_build")
                .await?;
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            stage = "baseline_build_start",
            "crate stage"
        );
        let build_start = Instant::now();
        let status = match run_cargo_status_timeout(
            crate_id,
            crate_name,
            "baseline_build_spawned",
            "baseline_build_timeout",
            CARGO_CMD_TIMEOUT_SECS,
            {
                let mut cmd = TokioCommand::new("cargo");
                cmd.args(["+1.95.0", "build", "--locked"])
                    .current_dir(crate_dir)
                    .env("CARGO_HOME", cargo_home)
                    .env("CARGO_TARGET_DIR", target_dir)
                    .env("CARGO_INCREMENTAL", "0")
                    .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                cmd
            },
        )
        .await
        {
            Ok(s) => s,
            Err(err) => {
                tracing::info!(
                    crate_id,
                    crate_name = %crate_name,
                    crate_dir = %crate_dir.display(),
                    ok = false,
                    error = ?err,
                    elapsed_ms = build_start.elapsed().as_millis(),
                    stage = "baseline_build_done",
                    "crate stage"
                );
                return Ok(false);
            }
        };
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            crate_dir = %crate_dir.display(),
            ok = status.success(),
            elapsed_ms = build_start.elapsed().as_millis(),
            stage = "baseline_build_done",
            "crate stage"
        );
        if !status.success() {
            return Ok(false);
        }
    } else {
        let _permit = acquire_cargo_cmd_permit(
            cargo_cmd_semaphore,
            crate_id,
            crate_name,
            "baseline_build_init",
        )
        .await?;
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            stage = "baseline_build_start",
            locked = false,
            "crate stage"
        );
        let init_start = Instant::now();
        let init_status = match run_cargo_status_timeout(
            crate_id,
            crate_name,
            "baseline_build_spawned",
            "baseline_build_timeout",
            CARGO_CMD_TIMEOUT_SECS,
            {
                let mut cmd = TokioCommand::new("cargo");
                cmd.args(["+1.95.0", "build"])
                    .current_dir(crate_dir)
                    .env("CARGO_HOME", cargo_home)
                    .env("CARGO_TARGET_DIR", target_dir)
                    .env("CARGO_INCREMENTAL", "0")
                    .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                cmd
            },
        )
        .await
        {
            Ok(s) => s,
            Err(err) => {
                tracing::info!(
                    crate_id,
                    crate_name = %crate_name,
                    crate_dir = %crate_dir.display(),
                    ok = false,
                    error = ?err,
                    cargo_lock_now_exists = cargo_lock.exists(),
                    elapsed_ms = init_start.elapsed().as_millis(),
                    stage = "baseline_build_done",
                    "crate stage"
                );
                return Ok(false);
            }
        };
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            crate_dir = %crate_dir.display(),
            ok = init_status.success(),
            cargo_lock_now_exists = cargo_lock.exists(),
            elapsed_ms = init_start.elapsed().as_millis(),
            stage = "baseline_build_done",
            "crate stage"
        );
        if !init_status.success() || !cargo_lock.exists() {
            return Ok(false);
        }
        fs::copy(&cargo_lock, &cargo_lock_baseline).await?;
    }

    if need_dev {
        let _permit =
            acquire_cargo_cmd_permit(cargo_cmd_semaphore, crate_id, crate_name, "baseline_test")
                .await?;
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            stage = "baseline_test_start",
            "crate stage"
        );
        let test_start = Instant::now();
        let test_status = match run_cargo_status_timeout(
            crate_id,
            crate_name,
            "baseline_test_spawned",
            "baseline_test_timeout",
            CARGO_CMD_TIMEOUT_SECS,
            {
                let mut cmd = TokioCommand::new("cargo");
                cmd.args(["+1.95.0", "test", "--no-run", "--locked"])
                    .current_dir(crate_dir)
                    .env("CARGO_HOME", cargo_home)
                    .env("CARGO_TARGET_DIR", target_dir)
                    .env("CARGO_INCREMENTAL", "0")
                    .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                cmd
            },
        )
        .await
        {
            Ok(s) => s,
            Err(err) => {
                tracing::info!(
                    crate_id,
                    crate_name = %crate_name,
                    crate_dir = %crate_dir.display(),
                    ok = false,
                    error = ?err,
                    elapsed_ms = test_start.elapsed().as_millis(),
                    stage = "baseline_test_done",
                    "crate stage"
                );
                return Ok(false);
            }
        };
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            crate_dir = %crate_dir.display(),
            ok = test_status.success(),
            elapsed_ms = test_start.elapsed().as_millis(),
            stage = "baseline_test_done",
            "crate stage"
        );
        if !test_status.success() {
            return Ok(false);
        }

        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            stage = "baseline_bench_start",
            "crate stage"
        );
        let bench_start = Instant::now();
        let bench_status = match run_cargo_status_timeout(
            crate_id,
            crate_name,
            "baseline_bench_spawned",
            "baseline_bench_timeout",
            CARGO_CMD_TIMEOUT_SECS,
            {
                let mut cmd = TokioCommand::new("cargo");
                cmd.args(["+1.95.0", "bench", "--no-run", "--locked"])
                    .current_dir(crate_dir)
                    .env("CARGO_HOME", cargo_home)
                    .env("CARGO_TARGET_DIR", target_dir)
                    .env("CARGO_INCREMENTAL", "0")
                    .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                cmd
            },
        )
        .await
        {
            Ok(s) => s,
            Err(err) => {
                tracing::info!(
                    crate_id,
                    crate_name = %crate_name,
                    crate_dir = %crate_dir.display(),
                    ok = false,
                    error = ?err,
                    elapsed_ms = bench_start.elapsed().as_millis(),
                    stage = "baseline_bench_done",
                    "crate stage"
                );
                return Ok(false);
            }
        };
        tracing::info!(
            crate_id,
            crate_name = %crate_name,
            crate_dir = %crate_dir.display(),
            ok = bench_status.success(),
            elapsed_ms = bench_start.elapsed().as_millis(),
            stage = "baseline_bench_done",
            "crate stage"
        );
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
    crate_id: i32,
    crate_name: &str,
    crate_dir: &Path,
    cargo_home: &Path,
    target_dir: &Path,
    cargo_cmd_semaphore: &Semaphore,
    deps_by_kind: &IndexDepsByKind,
) -> anyhow::Result<UpgradeExperimentSummary> {
    let deps = flatten_index_deps(deps_by_kind);
    if deps.is_empty() {
        return Ok(UpgradeExperimentSummary::default());
    }

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

        let version = match get_available_versions(db, &dep.name, &version_req).await {
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

        let Some(version) = version else {
            continue;
        };
        restore_baseline_project_state(crate_dir).await?;

        let update_ok = cargo_update_precise(
            crate_id,
            crate_name,
            crate_dir,
            cargo_home,
            cargo_cmd_semaphore,
            &dep.name,
            &version,
        )
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

        let verify_ok = verify_after_update(
            crate_id,
            crate_name,
            crate_dir,
            cargo_home,
            target_dir,
            cargo_cmd_semaphore,
            &dep.dep_type,
            &dep.name,
            &version,
        )
        .await?;

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
    crate_id: i32,
    crate_name: &str,
    crate_dir: &Path,
    cargo_home: &Path,
    cargo_cmd_semaphore: &Semaphore,
    dep_name: &str,
    target_version: &str,
) -> anyhow::Result<bool> {
    let _permit =
        acquire_cargo_cmd_permit(cargo_cmd_semaphore, crate_id, crate_name, "cargo_update").await?;
    let update_start = Instant::now();
    let status = run_cargo_status_timeout(
        crate_id,
        crate_name,
        "dep_update_spawned",
        "dep_update_timeout",
        CARGO_CMD_TIMEOUT_SECS,
        {
            let mut cmd = TokioCommand::new("cargo");
            cmd.args([
                "+1.95.0",
                "update",
                "-p",
                dep_name,
                "--precise",
                target_version,
            ])
            .current_dir(crate_dir)
            .env("CARGO_HOME", cargo_home);
            cmd
        },
    )
    .await?;
    let ok = status.success();
    tracing::info!(
        crate_id,
        crate_name = %crate_name,
        dep_name = %dep_name,
        target_version = %target_version,
        ok,
        elapsed_ms = update_start.elapsed().as_millis(),
        stage = "dep_update_done",
        "crate stage"
    );
    Ok(ok)
}

/// 更新依赖后执行编译验证：normal/build 跑 build，dev 跑 test+bench（均使用 --locked）
async fn verify_after_update(
    crate_id: i32,
    crate_name: &str,
    crate_dir: &Path,
    cargo_home: &Path,
    target_dir: &Path,
    cargo_cmd_semaphore: &Semaphore,
    dep_type: &DepType,
    dep_name: &str,
    target_version: &str,
) -> anyhow::Result<bool> {
    match dep_type {
        DepType::Normal | DepType::Build => {
            let _permit =
                acquire_cargo_cmd_permit(cargo_cmd_semaphore, crate_id, crate_name, "verify_build")
                    .await?;
            let build_start = Instant::now();
            let status = run_cargo_status_timeout(
                crate_id,
                crate_name,
                "verify_build_spawned",
                "verify_build_timeout",
                CARGO_CMD_TIMEOUT_SECS,
                {
                    let mut cmd = TokioCommand::new("cargo");
                    cmd.args(["+1.95.0", "build", "--locked"])
                        .current_dir(crate_dir)
                        .env("CARGO_HOME", cargo_home)
                        .env("CARGO_TARGET_DIR", target_dir)
                        .env("CARGO_INCREMENTAL", "0")
                        .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                    cmd
                },
            )
            .await?;
            let ok = status.success();
            tracing::info!(
                crate_id,
                crate_name = %crate_name,
                crate_dir = %crate_dir.display(),
                dep_name = %dep_name,
                target_version = %target_version,
                ok,
                elapsed_ms = build_start.elapsed().as_millis(),
                stage = "verify_build_done",
                "crate stage"
            );
            Ok(ok)
        }
        DepType::Dev => {
            let _permit =
                acquire_cargo_cmd_permit(cargo_cmd_semaphore, crate_id, crate_name, "verify_dev")
                    .await?;
            let test_start = Instant::now();
            let test_status = run_cargo_status_timeout(
                crate_id,
                crate_name,
                "verify_test_spawned",
                "verify_test_timeout",
                CARGO_CMD_TIMEOUT_SECS,
                {
                    let mut cmd = TokioCommand::new("cargo");
                    cmd.args(["+1.95.0", "test", "--no-run", "--locked"])
                        .current_dir(crate_dir)
                        .env("CARGO_HOME", cargo_home)
                        .env("CARGO_TARGET_DIR", target_dir)
                        .env("CARGO_INCREMENTAL", "0")
                        .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                    cmd
                },
            )
            .await?;
            let test_ok = test_status.success();
            tracing::info!(
                crate_id,
                crate_name = %crate_name,
                crate_dir = %crate_dir.display(),
                dep_name = %dep_name,
                target_version = %target_version,
                ok = test_ok,
                elapsed_ms = test_start.elapsed().as_millis(),
                stage = "verify_test_done",
                "crate stage"
            );
            if !test_ok {
                return Ok(false);
            }

            let bench_start = Instant::now();
            let bench_status = run_cargo_status_timeout(
                crate_id,
                crate_name,
                "verify_bench_spawned",
                "verify_bench_timeout",
                CARGO_CMD_TIMEOUT_SECS,
                {
                    let mut cmd = TokioCommand::new("cargo");
                    cmd.args(["+1.95.0", "bench", "--no-run", "--locked"])
                        .current_dir(crate_dir)
                        .env("CARGO_HOME", cargo_home)
                        .env("CARGO_TARGET_DIR", target_dir)
                        .env("CARGO_INCREMENTAL", "0")
                        .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
                    cmd
                },
            )
            .await?;
            let ok = bench_status.success();
            tracing::info!(
                crate_id,
                crate_name = %crate_name,
                crate_dir = %crate_dir.display(),
                dep_name = %dep_name,
                target_version = %target_version,
                ok,
                elapsed_ms = bench_start.elapsed().as_millis(),
                stage = "verify_bench_done",
                "crate stage"
            );
            Ok(ok)
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
) -> anyhow::Result<Option<String>> {
    let versions = if let Some(versions) = db.get_available_versions_from_db(crate_name).await? {
        versions
    } else {
        get_available_versions_via_api(crate_name).await?
    };

    Ok(filter_versions_by_req(versions, req))
}

/// 过滤版本列表：保留能解析为 semver 且满足给定 VersionReq 的版本。
/// 修改逻辑：按照 semver 解析后的大小进行降序排序，并且只保留符合条件的最新 1 个版本。
fn filter_versions_by_req(versions: Vec<String>, req: &VersionReq) -> Option<String> {
    let parsed_versions: Vec<Version> = versions
        .into_iter()
        .filter_map(|v| Version::parse(&v).ok())
        .filter(|ver| req.matches(ver))
        .collect();

    parsed_versions.into_iter().max().map(|v| v.to_string())
}

/// 通过 crates.io HTTP API 获取指定 crate 的所有非 yanked 版本号。
async fn get_available_versions_via_api(crate_name: &str) -> anyhow::Result<Vec<String>> {
    let url = format!("https://crates.io/api/v1/crates/{}", crate_name);

    let client = reqwest::Client::builder()
        .user_agent("cratesio-GetCode-Bot ")
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
        assert_eq!(filtered, Some("1.5.0".to_string()));

        let versions2 = vec![
            "0.9.9".to_string(),
            "0.10.0".to_string(),
            "0.10.1".to_string(),
            "0.11.0".to_string(),
        ];

        let req2 = VersionReq::parse("^0.10.0").unwrap();
        let filtered2 = filter_versions_by_req(versions2, &req2);
        assert_eq!(filtered2, Some("0.10.1".to_string()));
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
