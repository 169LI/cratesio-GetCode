//! 功能
//! -
//! 下载 crates.io 源码的命令模块。
//!
//! 约定
//! -
//! - 命令参数尽量通过环境变量/配置读取，CLI 子命令本身不承载过多参数
//! - 数据库相关读写通过 `PgDataHandle` 完成（后续在此模块内补充具体查询/更新逻辑）

use crate::pgdatahandle::PgDataHandle;
use anyhow::Context;
use futures_util::StreamExt;
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::time::sleep;

/// 根据 crate_name 的长度和名称，生成目标存储路径。
/// 分层规则：
/// 长度 = 1：1/<crate_name>/<crate_name>-<version>.crate
/// 长度 = 2：2/<crate_name>/<crate_name>-<version>.crate
/// 长度 = 3：3/<first>/<crate_name>/<crate_name>-<version>.crate（<first> 是 name 的第 1 个字符）
/// 长度 ≥ 4：<first2>/<second2>/<crate_name>/<crate_name>-<version>.crate
pub fn get_crate_file_path(base_dir: &Path, crate_name: &str, version: &str) -> PathBuf {
    let file_name = format!("{}-{}.crate", crate_name, version);
    let mut path = base_dir.to_path_buf();
    let name_len = crate_name.len();

    match name_len {
        1 => {
            path.push("1");
            path.push(crate_name);
        }
        2 => {
            path.push("2");
            path.push(crate_name);
        }
        3 => {
            path.push("3");
            let first = &crate_name[0..1];
            path.push(first);
            path.push(crate_name);
        }
        _ => {
            let first2 = &crate_name[0..2];
            let second2 = &crate_name[2..4];
            path.push(first2);
            path.push(second2);
            path.push(crate_name);
        }
    }

    path.push(file_name);
    path
}

/// 由 `.crate` 文件路径推导解压后的目标目录。
///
/// 规则：把扩展名 `.crate` 去掉，得到 `<crate_name>-<version>` 目录。
fn get_crate_extract_dir(crate_file_path: &Path) -> PathBuf {
    crate_file_path.with_extension("")
}

/// 判断是否为“跨文件系统 rename”错误（Linux/Unix 常见：EXDEV=18）。
///
/// 这类错误重试无意义，需要保证临时目录与目标目录在同一文件系统/挂载点。
fn is_cross_device_rename_error(err: &std::io::Error) -> bool {
    match err.raw_os_error() {
        Some(18) => true,
        _ => false,
    }
}

/// 判断当前 rename 失败是否值得重试（偏向处理临时占用/短暂异常）。
///
/// 典型场景：
/// - Windows：杀软/索引器短暂占用导致 `Access denied`（常见 os error 5）
/// - Linux/Unix：短暂的 `EINTR/EBUSY` 等
fn should_retry_rename_error(err: &std::io::Error) -> bool {
    if matches!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    ) {
        return true;
    }

    match err.raw_os_error() {
        Some(5 | 1 | 4 | 13 | 16 | 26) => true,
        _ => false,
    }
}

/// 判断 rename 失败是否可能因为“目标目录已存在/非空”，适合先清理目标再重试。
fn should_remove_target_then_retry(err: &std::io::Error) -> bool {
    match err.raw_os_error() {
        Some(17 | 39) => true,
        _ => false,
    }
}

/// 以指数退避对目录重命名（move）进行重试，提升 Windows/Linux 下的鲁棒性。
///
/// 目的：解压后会先落到 `*.tmp` 目录，再通过 rename 移动到最终目录。此处对短暂占用做重试，避免
/// “解压成功但落盘失败”的状态。
async fn rename_with_retry(from: &Path, to: &Path) -> anyhow::Result<()> {
    let mut delay = Duration::from_millis(120);
    for attempt in 1u32..=8 {
        match fs::rename(from, to).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if is_cross_device_rename_error(&e) {
                    return Err(e).with_context(|| {
                        format!(
                            "cross-device rename {} -> {} (ensure temp and target are on the same filesystem)",
                            from.display(),
                            to.display()
                        )
                    });
                }

                if should_remove_target_then_retry(&e) {
                    let _ = fs::remove_dir_all(to).await;
                }

                if !should_retry_rename_error(&e) || attempt == 8 {
                    return Err(e).with_context(|| {
                        format!(
                            "failed to move extracted dir {} -> {}",
                            from.display(),
                            to.display()
                        )
                    });
                }

                sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
            }
        }
    }

    Err(anyhow::anyhow!(
        "failed to move extracted dir {} -> {}",
        from.display(),
        to.display()
    ))
}

/// 解压 `.crate`（tar.gz）到目标目录，并清理临时目录与原始压缩包。
///
/// 流程：
/// - 若目标目录已存在：视为已解压过，仅删除 `.crate` 后返回（便于断点恢复）
/// - 否则：解压到 `*.tmp` 目录 → rename 到最终目录 → 删除 `*.tmp` 与 `.crate`
async fn extract_and_cleanup(crate_file_path: &Path) -> anyhow::Result<PathBuf> {
    let crate_file_path = crate_file_path.to_path_buf();
    let extract_dir = get_crate_extract_dir(&crate_file_path);

    if extract_dir.exists() {
        fs::remove_file(&crate_file_path)
            .await
            .with_context(|| format!("failed to delete archive {}", crate_file_path.display()))?;
        return Ok(extract_dir);
    }

    let temp_root = extract_dir.with_extension("tmp");
    if temp_root.exists() {
        fs::remove_dir_all(&temp_root)
            .await
            .with_context(|| format!("failed to remove temp dir {}", temp_root.display()))?;
    }
    fs::create_dir_all(&temp_root)
        .await
        .with_context(|| format!("failed to create temp dir {}", temp_root.display()))?;

    let extract_dir_name = extract_dir
        .file_name()
        .map(|s| s.to_owned())
        .context("invalid extract dir")?;
    let temp_extract_dir = temp_root.join(extract_dir_name);

    tokio::task::spawn_blocking({
        let crate_file_path = crate_file_path.clone();
        let temp_root = temp_root.clone();
        move || -> anyhow::Result<()> {
            let file = std::fs::File::open(&crate_file_path)
                .with_context(|| format!("failed to open archive {}", crate_file_path.display()))?;
            let gz = flate2::read::GzDecoder::new(file);
            let mut archive = tar::Archive::new(gz);
            archive
                .unpack(&temp_root)
                .with_context(|| format!("failed to unpack into {}", temp_root.display()))?;
            Ok(())
        }
    })
    .await
    .context("extract task join failed")??;

    if !temp_extract_dir.exists() {
        return Err(anyhow::anyhow!(
            "extract result dir not found: {}",
            temp_extract_dir.display()
        ));
    }

    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)
            .await
            .with_context(|| format!("failed to remove old dir {}", extract_dir.display()))?;
    }
    rename_with_retry(&temp_extract_dir, &extract_dir).await?;
    fs::remove_dir_all(&temp_root)
        .await
        .with_context(|| format!("failed to remove temp root {}", temp_root.display()))?;

    fs::remove_file(&crate_file_path)
        .await
        .with_context(|| format!("failed to delete archive {}", crate_file_path.display()))?;

    Ok(extract_dir)
}

/// 下载 `.crate` 文件到本地（使用 `.part` 临时文件保证原子落盘）。
///
/// 流程：
/// - 写入 `<target>.part`，成功后 rename 到 `<target>`
/// - 每次尝试前会清理残留的 `.part` 和目标文件，避免重试时 rename 失败
async fn download_archive(client: &Client, url: &str, file_path: &Path) -> anyhow::Result<()> {
    let temp_path = file_path.with_extension("crate.part");
    let _ = fs::remove_file(&temp_path).await;
    let _ = fs::remove_file(file_path).await;

    let res = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request failed: {}", url))?;

    if !res.status().is_success() {
        return Err(anyhow::Error::new(HttpStatusError {
            status: res.status(),
        }));
    }

    let mut file = File::create(&temp_path)
        .await
        .with_context(|| format!("failed to create file {}", temp_path.display()))?;

    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.with_context(|| format!("failed to read stream for {}", url))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("failed to write file {}", temp_path.display()))?;
    }

    let _ = file.flush().await;
    drop(file);

    fs::rename(&temp_path, file_path).await.with_context(|| {
        format!(
            "failed to move downloaded file {} -> {}",
            temp_path.display(),
            file_path.display()
        )
    })?;

    Ok(())
}

#[derive(Debug)]
struct HttpStatusError {
    status: reqwest::StatusCode,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http status {}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

fn is_permanent_http_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    )
}

/// 下载 crate 源码的主函数
///
/// 行为概要：
/// - 从数据库批量拉取待处理记录（`download=false && download_failed=false`）
/// - 并发下载（Semaphore 控制并发上限），每条记录最多重试 3 次
/// - 每次成功：下载 `.crate` → 解压到目录 → 删除 `.crate` → 更新数据库 `download=true`
/// - 连续失败 3 次：更新数据库 `download_failed=true`，避免下一轮重复卡住同一批数据
pub async fn download_run(db: &PgDataHandle, download_dir: &Path) -> anyhow::Result<()> {
    const MAX_ATTEMPTS: u32 = 3; // 3 次重试后算失败

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .context("failed to build http client")?;

    // 限制最大并发数，例如 40
    let semaphore = Arc::new(Semaphore::new(40));

    loop {
        // 每次取 1000 条未下载的数据
        let unfetched = db.get_unfetched_crates(1000).await?;
        if unfetched.is_empty() {
            tracing::info!("All crates downloaded.");
            break;
        }

        tracing::info!("Fetched {} crates to download.", unfetched.len());

        let mut tasks = Vec::new();

        for crate_model in unfetched {
            let db = db.clone();
            let client = client.clone();
            let semaphore = semaphore.clone();
            let download_dir = download_dir.to_path_buf();

            let task = tokio::spawn(async move {
                let crate_name = &crate_model.name;
                let version = &crate_model.version_new;
                let id = crate_model.id;

                let url = format!(
                    "https://static.crates.io/crates/{}/{}-{}.crate",
                    crate_name, crate_name, version
                );

                let file_path = get_crate_file_path(&download_dir, crate_name, version);

                if let Some(parent) = file_path.parent() {
                    if let Err(e) = fs::create_dir_all(parent).await {
                        tracing::error!(error = ?e, "Failed to create directory {:?}", parent);
                        return;
                    }
                }

                for attempt in 1..=MAX_ATTEMPTS {
                    let permit = match semaphore.acquire().await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::error!(error = ?e, "semaphore closed");
                            return;
                        }
                    };

                    let attempt_result = async {
                        download_archive(&client, &url, &file_path).await?;
                        extract_and_cleanup(&file_path).await?;
                        db.mark_crate_downloaded(id).await.with_context(|| {
                            format!("failed to mark crate {} as downloaded", id)
                        })?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;

                    drop(permit);

                    match attempt_result {
                        Ok(()) => {
                            return;
                        }
                        Err(e) => {
                            if let Some(status_err) = e.downcast_ref::<HttpStatusError>() {
                                if is_permanent_http_status(status_err.status) {
                                    tracing::warn!(
                                        attempt,
                                        max_attempts = MAX_ATTEMPTS,
                                        error = ?e,
                                        "crate {} v{} failed (permanent)",
                                        crate_name,
                                        version
                                    );

                                    if let Err(db_err) = db.mark_crate_download_failed(id).await {
                                        tracing::error!(
                                            error = ?db_err,
                                            "failed to mark crate {} as download_failed",
                                            id
                                        );
                                    }

                                    return;
                                }
                            }

                            tracing::warn!(
                                attempt,
                                max_attempts = MAX_ATTEMPTS,
                                error = ?e,
                                "crate {} v{} failed",
                                crate_name,
                                version
                            );

                            if attempt < MAX_ATTEMPTS {
                                let backoff = Duration::from_secs(1u64 << (attempt - 1));
                                sleep(backoff).await;
                                continue;
                            }

                            if let Err(db_err) = db.mark_crate_download_failed(id).await {
                                tracing::error!(
                                    error = ?db_err,
                                    "failed to mark crate {} as download_failed",
                                    id
                                );
                            }

                            return;
                        }
                    }
                }
            });

            tasks.push(task);
        }

        for task in tasks {
            if let Err(e) = task.await {
                tracing::error!("download task panicked: {}", e);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_get_crate_file_path() {
        let base = Path::new("/download/dir");

        // 长度 = 1
        assert_eq!(
            get_crate_file_path(base, "a", "1.0.0"),
            base.join("1").join("a").join("a-1.0.0.crate")
        );

        // 长度 = 2
        assert_eq!(
            get_crate_file_path(base, "ab", "0.1.1"),
            base.join("2").join("ab").join("ab-0.1.1.crate")
        );

        // 长度 = 3
        assert_eq!(
            get_crate_file_path(base, "abc", "2.0.0"),
            base.join("3").join("a").join("abc").join("abc-2.0.0.crate")
        );

        // 长度 = 4
        assert_eq!(
            get_crate_file_path(base, "abcd", "1.2.3"),
            base.join("ab")
                .join("cd")
                .join("abcd")
                .join("abcd-1.2.3.crate")
        );

        // 长度 > 4
        assert_eq!(
            get_crate_file_path(base, "serde", "1.0.197"),
            base.join("se")
                .join("rd")
                .join("serde")
                .join("serde-1.0.197.crate")
        );
    }
}
