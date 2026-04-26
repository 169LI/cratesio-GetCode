//! 功能
//! -
//! 编译下载的 crate，并对单个依赖进行不同版本的更新和编译验证。

use anyhow::Context;
use cargo_metadata::MetadataCommand;
use std::path::Path;
use std::process::Command;
use tokio::fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepType {
    Normal,
    Dev,
    Build,
}

#[derive(Debug)]
pub struct DependencyInfo {
    pub name: String,
    pub dep_type: DepType,
    pub req: String,
}

/// 对给定的 crate 目录进行依赖更新和编译验证
pub async fn verify_crate_dependency_updates(crate_dir: &Path) -> anyhow::Result<()> {
    tracing::info!("开始验证 Crate: {}", crate_dir.display());

    let cargo_toml = crate_dir.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Err(anyhow::anyhow!(
            "Cargo.toml 不存在: {}",
            cargo_toml.display()
        ));
    }

    // 0. 确保初始 Cargo.lock 存在（生成 lockfile）
    let status = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(crate_dir)
        .status()
        .context("cargo generate-lockfile 执行失败")?;

    if !status.success() {
        tracing::warn!("cargo generate-lockfile 失败，可能是包配置问题或网络问题");
    }

    let cargo_lock = crate_dir.join("Cargo.lock");
    let cargo_lock_bak = crate_dir.join("Cargo.lock.bak");

    // 1. 复制原始的 Cargo.lock 作为备份
    if cargo_lock.exists() {
        fs::copy(&cargo_lock, &cargo_lock_bak)
            .await
            .context("备份 Cargo.lock 失败")?;
        tracing::debug!("已备份 Cargo.lock");
    } else {
        tracing::warn!("未能生成初始 Cargo.lock: {}", cargo_lock.display());
    }

    // 2. 解析依赖项（区分普通、dev、build）
    let dependencies = parse_dependencies(crate_dir)?;
    tracing::info!("解析到 {} 个直接依赖", dependencies.len());

    // 3. 遍历依赖，获取可更新版本并验证
    for dep in dependencies {
        tracing::info!(
            "处理依赖: {} (类型: {:?}, 原版本要求: {})",
            dep.name,
            dep.dep_type,
            dep.req
        );

        let versions = match get_available_versions(&dep.name).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("获取依赖 {} 可用版本失败: {:?}", dep.name, e);
                continue;
            }
        };

        tracing::debug!(
            "依赖 {} 共有 {} 个未被 yank 的版本",
            dep.name,
            versions.len()
        );

        for version in versions {
            tracing::info!("尝试更新依赖 {} 到版本 {}", dep.name, version);

            // 还原 Cargo.lock 以确保每次只更新一个依赖的一个版本
            if cargo_lock_bak.exists() {
                fs::copy(&cargo_lock_bak, &cargo_lock).await?;
            }

            // 执行 cargo update -p <dep> --precise <version>
            let update_status = Command::new("cargo")
                .args(["update", "-p", &dep.name, "--precise", &version])
                .current_dir(crate_dir)
                .output()
                .context("执行 cargo update 失败")?;

            if !update_status.status.success() {
                let stderr = String::from_utf8_lossy(&update_status.stderr);
                // 处理依赖解析器拒绝更新的情况
                if stderr.contains("failed to select a version")
                    || stderr.contains("does not match the version req")
                    || stderr.contains("could not find specification for")
                {
                    tracing::warn!(
                        "依赖 {} 版本 {} 被解析器拒绝 (依赖冲突或不满足要求)",
                        dep.name,
                        version
                    );
                } else {
                    tracing::error!("依赖 {} 版本 {} 更新失败: {}", dep.name, version, stderr);
                }
                continue; // 跳过此版本
            }

            // 更新成功，执行对应的验证命令
            let verify_success = verify_compilation(crate_dir, &dep.dep_type)?;
            if verify_success {
                tracing::info!("✅ 验证成功: {} 更新到 {}", dep.name, version);
            } else {
                tracing::warn!("❌ 验证失败(破坏性更新): {} 更新到 {}", dep.name, version);
            }
        }
    }

    tracing::info!("Crate 验证完成: {}", crate_dir.display());
    Ok(())
}

/// 根据依赖类型执行相应的验证命令
fn verify_compilation(crate_dir: &Path, dep_type: &DepType) -> anyhow::Result<bool> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(crate_dir);

    match dep_type {
        // 普通依赖和 Build 依赖，尝试编译即可
        DepType::Normal | DepType::Build => {
            cmd.arg("build");
        }
        // Dev 依赖，通常用于测试，因此运行测试的编译过程
        DepType::Dev => {
            cmd.args(["test", "--no-run"]);
        }
    }

    let output = cmd.output().context("执行编译验证命令失败")?;
    Ok(output.status.success())
}

/// 解析 Cargo.toml 中的直接依赖项并分类
fn parse_dependencies(crate_dir: &Path) -> anyhow::Result<Vec<DependencyInfo>> {
    let metadata = MetadataCommand::new()
        .current_dir(crate_dir)
        .no_deps() // 只解析工作区内的信息
        .exec()
        .context("cargo metadata 获取失败")?;

    let mut deps = Vec::new();

    if let Some(root) = metadata.root_package() {
        for dep in &root.dependencies {
            let dep_type = match dep.kind {
                cargo_metadata::DependencyKind::Normal => DepType::Normal,
                cargo_metadata::DependencyKind::Development => DepType::Dev,
                cargo_metadata::DependencyKind::Build => DepType::Build,
                _ => continue,
            };

            deps.push(DependencyInfo {
                name: dep.name.clone(),
                dep_type,
                req: dep.req.to_string(),
            });
        }
    } else {
        tracing::warn!(
            "未找到 root_package，可能是 workspace 项目: {}",
            crate_dir.display()
        );
    }

    Ok(deps)
}

/// 通过 crates.io API 获取可用版本列表
async fn get_available_versions(crate_name: &str) -> anyhow::Result<Vec<String>> {
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
