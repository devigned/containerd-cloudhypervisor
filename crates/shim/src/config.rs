use std::path::Path;

use anyhow::{Context, Result};
use cloudhv_common::types::RuntimeConfig;
use log::debug;

/// Default configuration file path.
const DEFAULT_CONFIG_PATH: &str = "/opt/cloudhv/config.json";

/// Load runtime configuration from the default path or a specified path.
pub fn load_config(path: Option<&str>) -> Result<RuntimeConfig> {
    let config_path = path.unwrap_or(DEFAULT_CONFIG_PATH);
    let p = Path::new(config_path);

    if !p.exists() {
        anyhow::bail!(
            "runtime config not found at {config_path}; \
             create it or specify an alternative path"
        );
    }

    let data = std::fs::read_to_string(p)
        .with_context(|| format!("failed to read config from {config_path}"))?;

    let config: RuntimeConfig = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse config from {config_path}"))?;

    debug!("loaded runtime config: {:?}", config);
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_valid_config() {
        let config_json = r#"{
            "kernel_path": "/opt/cloudhv/vmlinux",
            "rootfs_path": "/opt/cloudhv/rootfs.ext4"
        }"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(config_json.as_bytes()).unwrap();

        let config = load_config(Some(f.path().to_str().unwrap())).unwrap();
        assert_eq!(config.kernel_path, "/opt/cloudhv/vmlinux");
        assert_eq!(config.rootfs_path, "/opt/cloudhv/rootfs.ext4");
        assert_eq!(config.default_vcpus, cloudhv_common::DEFAULT_VCPUS);
        assert_eq!(config.default_memory_mb, cloudhv_common::DEFAULT_MEMORY_MB);
    }

    #[test]
    fn test_load_missing_config() {
        let result = load_config(Some("/nonexistent/path.json"));
        assert!(result.is_err());
    }
}
