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
    "console=hvc0 root=/dev/vda rw quiet".to_string()
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmDisk {
    pub path: String,
    #[serde(default)]
    pub readonly: bool,
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
}

impl VmConsoleConfig {
    pub fn off() -> Self {
        Self {
            mode: "Off".to_string(),
        }
    }
}
