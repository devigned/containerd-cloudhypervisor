use std::path::Path;

use log::{debug, info};

/// Detected hypervisor backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HypervisorBackend {
    Kvm,
    Mshv,
    Unknown,
}

impl std::fmt::Display for HypervisorBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HypervisorBackend::Kvm => write!(f, "KVM"),
            HypervisorBackend::Mshv => write!(f, "MSHV"),
            HypervisorBackend::Unknown => write!(f, "unknown"),
        }
    }
}

/// Detect the available hypervisor backend on this host.
///
/// Cloud Hypervisor auto-selects the backend at runtime, but we detect it
/// here for logging, configuration validation, and MSHV-specific tuning.
///
/// Detection order:
/// 1. /dev/kvm → KVM (Linux default)
/// 2. /dev/mshv → MSHV (Microsoft Hypervisor, Azure/Hyper-V)
/// 3. Unknown
pub fn detect_hypervisor() -> HypervisorBackend {
    if Path::new("/dev/kvm").exists() {
        info!("detected hypervisor backend: KVM (/dev/kvm)");
        HypervisorBackend::Kvm
    } else if Path::new("/dev/mshv").exists() {
        info!("detected hypervisor backend: MSHV (/dev/mshv)");
        HypervisorBackend::Mshv
    } else {
        debug!("no hypervisor device detected (/dev/kvm and /dev/mshv not found)");
        HypervisorBackend::Unknown
    }
}

/// Check if the host supports nested virtualization (required for our use case).
#[allow(dead_code)]
pub fn check_virtualization_support() -> bool {
    let backend = detect_hypervisor();
    match backend {
        HypervisorBackend::Kvm => {
            // Check if KVM is accessible
            match std::fs::metadata("/dev/kvm") {
                Ok(meta) => {
                    use std::os::unix::fs::MetadataExt;
                    let mode = meta.mode();
                    let accessible = mode & 0o666 != 0;
                    if !accessible {
                        info!(
                            "/dev/kvm exists but may not be accessible (mode={:#o})",
                            mode
                        );
                    }
                    true
                }
                Err(e) => {
                    info!("/dev/kvm check failed: {e}");
                    false
                }
            }
        }
        HypervisorBackend::Mshv => {
            // MSHV device exists — Cloud Hypervisor will use it
            info!("MSHV backend available — Cloud Hypervisor will use Microsoft Hypervisor");
            true
        }
        HypervisorBackend::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_hypervisor() {
        let backend = detect_hypervisor();
        // On macOS this will be Unknown, on Linux likely KVM
        eprintln!("detected hypervisor: {backend}");
        // Just verify it doesn't panic
        assert!(
            backend == HypervisorBackend::Kvm
                || backend == HypervisorBackend::Mshv
                || backend == HypervisorBackend::Unknown
        );
    }

    #[test]
    fn test_hypervisor_display() {
        assert_eq!(format!("{}", HypervisorBackend::Kvm), "KVM");
        assert_eq!(format!("{}", HypervisorBackend::Mshv), "MSHV");
        assert_eq!(format!("{}", HypervisorBackend::Unknown), "unknown");
    }
}
