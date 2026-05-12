//! 模块说明
//! -
//! 运行时配置加载模块：负责从工作区根目录 `.env` 与进程环境变量读取配置，并提供
//! “Fail Fast”的必填项校验接口。
//!
//! 读取规则
//! -
//! - 默认尝试加载 `crates_io/../.env`（工作区根目录的 `.env`）
//! - 之后从 `std::env::vars()` 收集环境变量快照
//!
//! 使用方式
//! -
//! - `get_config_once(...)`：初始化一次并返回全局只读配置
//! - `config.require("KEY")?`：读取必填配置，缺失则直接报错退出

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct Config {
    pub env: HashMap<String, String>,
}

impl Config {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(|v| v.as_str())
    }

    pub fn require(&self, key: &str) -> anyhow::Result<String> {
        self.get(key)
            .map(|v| v.to_owned())
            .ok_or_else(|| anyhow::anyhow!("missing env: {}", key))
    }
}

#[derive(Clone, Debug)]
pub struct ConfigLoad;

impl ConfigLoad {
    pub fn new() -> Self {
        Self
    }
}

pub fn get_config_once(_load: &ConfigLoad) -> anyhow::Result<&'static Config> {
    if let Some(config) = CONFIG.get() {
        return Ok(config);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap_or(&manifest_dir);
    let env_candidates = [
        (workspace_root.join(".env"), false),
        (
            workspace_root
                .join("datahandle")
                .join("data_import")
                .join(".env"),
            true,
        ),
    ];

    for (env_path, should_override) in env_candidates {
        if !env_path.exists() {
            continue;
        }
        let loaded = if should_override {
            dotenvy::from_path_override(&env_path)
        } else {
            dotenvy::from_path(&env_path)
        };
        loaded.map_err(|e| {
            anyhow::anyhow!(
                "failed to load dotenv from {}: {}",
                env_path.display(),
                e
            )
        })?;
    }
    let env = std::env::vars().collect::<HashMap<_, _>>();

    let _ = CONFIG.set(Config { env });
    Ok(CONFIG.get().expect("config must be initialized"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn env_download_dir_exists() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_env = manifest_dir.parent().unwrap_or(&manifest_dir).join(".env");
        assert!(
            workspace_env.exists(),
            "workspace .env not found: {}",
            workspace_env.display()
        );

        dotenvy::from_path(&workspace_env).unwrap_or_else(|e| {
            panic!(
                "failed to load dotenv from {}: {}",
                workspace_env.display(),
                e
            )
        });

        let download_dir_raw = std::env::var("DOWNLOAD_DIR").unwrap_or_else(|_| {
            panic!(
                "missing env: DOWNLOAD_DIR (loaded from {})",
                workspace_env.display()
            )
        });
        let download_dir = PathBuf::from(download_dir_raw);

        assert!(
            download_dir.is_dir(),
            "DOWNLOAD_DIR must be an existing directory: {}",
            download_dir.display()
        );

        let index_dir_raw = std::env::var("CRATESIO_INDEX_DIR").unwrap_or_else(|_| {
            panic!(
                "missing env: CRATESIO_INDEX_DIR (loaded from {})",
                workspace_env.display()
            )
        });
        let index_dir = PathBuf::from(index_dir_raw);

        assert!(
            index_dir.is_dir(),
            "CRATESIO_INDEX_DIR must be an existing directory: {}",
            index_dir.display()
        );
    }
}
