use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Daemon configuration loaded from a JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Number of warm VMs to keep ready in the pool.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Maximum idle VMs in the pool (hard cap).
    #[serde(default = "default_max_pool_size")]
    pub max_pool_size: usize,

    /// Delay in ms before replenishing pool after an acquire.
    #[serde(default = "default_replenish_delay_ms")]
    pub replenish_delay_ms: u64,

    /// Destroy idle VMs after this many seconds.
    #[serde(default = "default_idle_timeout_secs")]
    pub vm_idle_timeout_secs: u64,

    /// Default vCPUs per pool VM.
    #[serde(default = "default_vcpus")]
    pub default_vcpus: u32,

    /// Default memory in MiB per pool VM.
    #[serde(default = "default_memory_mb")]
    pub default_memory_mb: u64,

    /// Path to cloud-hypervisor binary.
    #[serde(default = "default_ch_binary")]
    pub cloud_hypervisor_binary: String,

    /// Path to guest kernel (vmlinux).
    pub kernel_path: String,

    /// Path to guest rootfs image (erofs).
    pub rootfs_path: String,

    /// Kernel command line arguments.
    #[serde(default = "default_kernel_args")]
    pub kernel_args: String,

    /// Unix socket path for the daemon's ttrpc API.
    #[serde(default = "default_socket_path")]
    pub socket_path: String,

    /// Directory for daemon state (base snapshot, pool VM state).
    #[serde(default = "default_state_dir")]
    pub state_dir: String,

    /// Enable warm workload snapshots via shadow VMs (experimental).
    #[serde(default)]
    pub warm_restore: bool,

    /// Warmup duration in seconds for shadow VMs before snapshotting.
    #[serde(default = "default_warmup_secs")]
    pub warmup_duration_secs: u64,

    /// Maximum number of cached workload snapshots.
    /// Oldest (LRU) snapshots are evicted when this limit is reached.
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: usize,
}

fn default_pool_size() -> usize {
    3
}
fn default_max_pool_size() -> usize {
    10
}
fn default_replenish_delay_ms() -> u64 {
    100
}
fn default_idle_timeout_secs() -> u64 {
    300
}
fn default_vcpus() -> u32 {
    1
}
fn default_memory_mb() -> u64 {
    512
}
fn default_ch_binary() -> String {
    "/usr/local/bin/cloud-hypervisor".to_string()
}
fn default_kernel_args() -> String {
    "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0".to_string()
}
fn default_socket_path() -> String {
    "/run/cloudhv/daemon.sock".to_string()
}
fn default_state_dir() -> String {
    "/run/cloudhv/daemon".to_string()
}
fn default_warmup_secs() -> u64 {
    30
}
fn default_max_snapshots() -> usize {
    20
}

impl DaemonConfig {
    /// Load configuration from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read config: {}", path.display()))?;
        let config: Self = serde_json::from_str(&content).with_context(|| "parse daemon config")?;
        Ok(config)
    }

    /// Path to the base snapshot directory.
    pub fn base_snapshot_dir(&self) -> PathBuf {
        PathBuf::from(&self.state_dir).join("base-snapshot")
    }

    /// Path to the workload snapshot cache directory.
    pub fn snapshot_cache_dir(&self) -> PathBuf {
        PathBuf::from(&self.state_dir).join("snapshot-cache")
    }
}
