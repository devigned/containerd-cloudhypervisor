use serde::{Deserialize, Serialize};

/// Runtime configuration loaded from /etc/containerd/cloudhv-runtime.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Path to the cloud-hypervisor binary
    #[serde(default = "default_ch_binary")]
    pub cloud_hypervisor_binary: String,

    /// Path to the virtiofsd binary
    #[serde(default = "default_virtiofsd_binary")]
    pub virtiofsd_binary: String,

    /// Path to the guest kernel (vmlinux or bzImage)
    pub kernel_path: String,

    /// Path to the guest rootfs image (ext4)
    pub rootfs_path: String,

    /// Default number of vCPUs per VM
    #[serde(default = "default_vcpus")]
    pub default_vcpus: u32,

    /// Default memory in MiB per VM
    #[serde(default = "default_memory_mb")]
    pub default_memory_mb: u64,

    /// vsock port for the guest agent
    #[serde(default = "default_vsock_port")]
    pub vsock_port: u32,

    /// Timeout in seconds for agent startup
    #[serde(default = "default_agent_timeout")]
    pub agent_startup_timeout_secs: u64,

    /// Kernel command line arguments
    #[serde(default = "default_kernel_args")]
    pub kernel_args: String,

    /// Enable debug logging
    #[serde(default)]
    pub debug: bool,

    /// Number of pre-warmed VMs in the pool (0 = no pooling)
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Maximum containers per VM (1 = one container per VM)
    #[serde(default = "default_max_containers_per_vm")]
    pub max_containers_per_vm: usize,

    /// Hotplug memory size in MiB (0 = no hotplug).
    /// When set, VMs are created with this additional reservable memory.
    #[serde(default = "default_hotplug_memory_mb")]
    pub hotplug_memory_mb: u64,

    /// Memory hotplug method: "acpi" (default) or "virtio-mem"
    #[serde(default = "default_hotplug_method")]
    pub hotplug_method: String,

    /// Enable TPM 2.0 device (requires swtpm installed on host)
    #[serde(default)]
    pub tpm_enabled: bool,
}

fn default_ch_binary() -> String {
    crate::DEFAULT_CH_BINARY.to_string()
}
fn default_virtiofsd_binary() -> String {
    crate::DEFAULT_VIRTIOFSD_BINARY.to_string()
}
fn default_vcpus() -> u32 {
    crate::DEFAULT_VCPUS
}
fn default_memory_mb() -> u64 {
    crate::DEFAULT_MEMORY_MB
}
fn default_vsock_port() -> u32 {
    crate::AGENT_VSOCK_PORT
}
fn default_agent_timeout() -> u64 {
    crate::AGENT_STARTUP_TIMEOUT_SECS
}
fn default_kernel_args() -> String {
    "console=hvc0 root=/dev/vda rw quiet init=/init".to_string()
}
fn default_pool_size() -> usize {
    crate::DEFAULT_POOL_SIZE
}
fn default_max_containers_per_vm() -> usize {
    crate::DEFAULT_MAX_CONTAINERS_PER_VM
}
fn default_hotplug_memory_mb() -> u64 {
    crate::DEFAULT_HOTPLUG_MEMORY_MB
}
fn default_hotplug_method() -> String {
    "acpi".to_string()
}

/// Cloud Hypervisor VM configuration (JSON sent to CH API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub payload: VmPayload,
    pub cpus: VmCpus,
    pub memory: VmMemory,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<VmDisk>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fs: Vec<VmFs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock: Option<VmVsock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<VmConsoleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console: Option<VmConsoleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpm: Option<VmTpm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmPayload {
    pub kernel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmCpus {
    pub boot_vcpus: u32,
    pub max_vcpus: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmMemory {
    /// Memory size in bytes
    pub size: u64,
    #[serde(default)]
    pub shared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotplug_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotplug_method: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmDisk {
    pub path: String,
    #[serde(default)]
    pub readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmFs {
    pub tag: String,
    pub socket: String,
    #[serde(default = "default_fs_queues")]
    pub num_queues: u32,
    #[serde(default = "default_fs_queue_size")]
    pub queue_size: u32,
}

fn default_fs_queues() -> u32 {
    1
}
fn default_fs_queue_size() -> u32 {
    128
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmVsock {
    pub cid: u64,
    pub socket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConsoleConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl VmConsoleConfig {
    pub fn off() -> Self {
        Self {
            mode: "Off".to_string(),
            file: None,
        }
    }
    pub fn file(path: &str) -> Self {
        Self {
            mode: "File".to_string(),
            file: Some(path.to_string()),
        }
    }
}

/// TPM 2.0 device configuration (requires external swtpm).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmTpm {
    pub socket: String,
}

/// Live migration configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationConfig {
    /// Transport URI: "unix:/path/to/socket" or "tcp:host:port"
    pub uri: String,
    /// Whether this is a local (same-host) migration
    #[serde(default)]
    pub local: bool,
}

/// Snapshot configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    /// Directory to store/load snapshot files
    pub destination_url: String,
}
