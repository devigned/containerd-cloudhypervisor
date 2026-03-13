//! Integration tests for containerd-cloudhypervisor.
//!
//! These tests require a Linux host with KVM, cloud-hypervisor,
//! and a pre-built guest kernel + rootfs. They are meant to be run on the
//! Azure dev VM via `make remote-integration` or `cargo test --test integration`.
//!
//! Set environment variables to override default paths:
//!   CLOUDHV_TEST_KERNEL  - path to vmlinux (default: guest/kernel/vmlinux)
//!   CLOUDHV_TEST_ROOTFS  - path to rootfs.ext4 (default: guest/rootfs/rootfs.ext4)
//!   CLOUDHV_TEST_CH_BIN  - path to cloud-hypervisor (default: /usr/local/bin/cloud-hypervisor)

#[cfg(target_os = "linux")]
mod helpers;

#[cfg(target_os = "linux")]
mod vm_lifecycle;
