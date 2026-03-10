pub mod error;
pub mod types;

/// Default vsock port for the guest agent ttrpc server.
pub const AGENT_VSOCK_PORT: u32 = 10789;

/// Guest vsock CID starts at this value (host=2, first guest=3).
pub const GUEST_CID_START: u64 = 3;

/// Default timeout (seconds) waiting for the guest agent to become ready.
pub const AGENT_STARTUP_TIMEOUT_SECS: u64 = 10;

/// Default number of boot vCPUs.
pub const DEFAULT_VCPUS: u32 = 1;

/// Default guest memory in MiB.
pub const DEFAULT_MEMORY_MB: u64 = 128;

/// virtio-fs tag used to share container bundles with the guest.
pub const VIRTIOFS_TAG: &str = "containerfs";

/// Mount point inside the guest for the virtio-fs share.
pub const VIRTIOFS_GUEST_MOUNT: &str = "/containers";

/// Runtime state directory on the host.
pub const RUNTIME_STATE_DIR: &str = "/run/cloudhv";

/// Default path to the Cloud Hypervisor binary.
pub const DEFAULT_CH_BINARY: &str = "/usr/local/bin/cloud-hypervisor";

/// Default path to virtiofsd binary.
pub const DEFAULT_VIRTIOFSD_BINARY: &str = "/usr/libexec/virtiofsd";

/// Default VM pool size (number of pre-warmed VMs).
pub const DEFAULT_POOL_SIZE: usize = 2;

/// Default maximum containers per VM.
pub const DEFAULT_MAX_CONTAINERS_PER_VM: usize = 5;

/// Default hotplug memory size in MiB (0 = no hotplug).
pub const DEFAULT_HOTPLUG_MEMORY_MB: u64 = 0;
