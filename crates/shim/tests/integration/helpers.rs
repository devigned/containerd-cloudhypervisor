use std::path::{Path, PathBuf};

/// Test fixture paths, resolved from env vars or project defaults.
pub struct TestFixtures {
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub ch_binary: PathBuf,
    pub virtiofsd_binary: PathBuf,
}

impl TestFixtures {
    /// Resolve test fixture paths. Returns None if essential fixtures are missing.
    pub fn resolve() -> Option<Self> {
        let project_root = project_root();

        let kernel_path = env_or_default(
            "CLOUDHV_TEST_KERNEL",
            project_root.join("guest/kernel/vmlinux"),
        );
        let rootfs_path = env_or_default(
            "CLOUDHV_TEST_ROOTFS",
            project_root.join("guest/rootfs/rootfs.ext4"),
        );
        let ch_binary = env_or_default(
            "CLOUDHV_TEST_CH_BIN",
            PathBuf::from("/usr/local/bin/cloud-hypervisor"),
        );
        let virtiofsd_binary =
            env_or_default("CLOUDHV_TEST_VFSD", PathBuf::from("/usr/libexec/virtiofsd"));

        Some(TestFixtures {
            kernel_path,
            rootfs_path,
            ch_binary,
            virtiofsd_binary,
        })
    }

    /// Check that all required files exist. Returns a list of missing items.
    pub fn check_prerequisites(&self) -> Vec<String> {
        let mut missing = Vec::new();

        if !self.kernel_path.exists() {
            missing.push(format!("kernel: {}", self.kernel_path.display()));
        }
        if !self.rootfs_path.exists() {
            missing.push(format!("rootfs: {}", self.rootfs_path.display()));
        }
        if !self.ch_binary.exists() {
            missing.push(format!("cloud-hypervisor: {}", self.ch_binary.display()));
        }
        if !self.virtiofsd_binary.exists() {
            missing.push(format!("virtiofsd: {}", self.virtiofsd_binary.display()));
        }
        if !Path::new("/dev/kvm").exists() && !Path::new("/dev/mshv").exists() {
            missing.push("hypervisor: /dev/kvm or /dev/mshv".to_string());
        }

        missing
    }

    /// Build a RuntimeConfig suitable for testing.
    pub fn runtime_config(&self) -> cloudhv_common::types::RuntimeConfig {
        cloudhv_common::types::RuntimeConfig {
            cloud_hypervisor_binary: self.ch_binary.to_string_lossy().to_string(),
            virtiofsd_binary: self.virtiofsd_binary.to_string_lossy().to_string(),
            kernel_path: self.kernel_path.to_string_lossy().to_string(),
            rootfs_path: self.rootfs_path.to_string_lossy().to_string(),
            default_vcpus: 1,
            default_memory_mb: 128,
            vsock_port: cloudhv_common::AGENT_VSOCK_PORT,
            agent_startup_timeout_secs: 30,
            kernel_args: "console=hvc0 root=/dev/vda rw quiet init=/init".to_string(),
            debug: true,
            pool_size: 0,
            max_containers_per_vm: 1,
            hotplug_memory_mb: 0,
            hotplug_method: "acpi".to_string(),
            tpm_enabled: false,
        }
    }
}

fn env_or_default(var: &str, default: PathBuf) -> PathBuf {
    std::env::var(var).map(PathBuf::from).unwrap_or(default)
}

fn project_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to crates/shim/ for this crate.
    // The workspace root is two levels up.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap());

    // If we're in a subcrate (crates/shim), go up to workspace root
    if manifest_dir.ends_with("crates/shim") {
        manifest_dir
            .join("../..")
            .canonicalize()
            .unwrap_or(manifest_dir)
    } else {
        manifest_dir
    }
}

/// Skip a test with a message if prerequisites are not met.
#[macro_export]
macro_rules! skip_if_missing {
    ($fixtures:expr) => {
        let missing = $fixtures.check_prerequisites();
        if !missing.is_empty() {
            eprintln!(
                "SKIPPING TEST: missing prerequisites:\n  {}",
                missing.join("\n  ")
            );
            return;
        }
    };
}
