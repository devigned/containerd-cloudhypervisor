//! Benchmarks for containerd-cloudhypervisor overhead measurements.
//!
//! Measures:
//! - VM config serialization overhead
//! - Erofs cache key hashing throughput
//!
//! Run with: cargo bench -p containerd-shim-cloudhv
//!
//! For full VM lifecycle benchmarks (requires KVM + root), use
//! the integration test suite with timing output.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use cloudhv_common::types::*;

/// Benchmark VM config JSON serialization (measures shim overhead per create).
fn bench_vm_config_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("vm_config");

    let config = VmConfig {
        payload: VmPayload {
            kernel: "/opt/cloudhv/vmlinux".to_string(),
            cmdline: Some("console=hvc0 root=/dev/vda rw quiet".to_string()),
            initramfs: None,
        },
        cpus: VmCpus {
            boot_vcpus: 1,
            max_vcpus: 4,
        },
        memory: VmMemory {
            size: 128 * 1024 * 1024,
            shared: true,
            hotplug_size: Some(512 * 1024 * 1024),
            hotplug_method: Some("VirtioMem".to_string()),
        },
        disks: vec![VmDisk {
            path: "/opt/cloudhv/rootfs.ext4".to_string(),
            readonly: false,
            id: None,
        }],
        net: vec![],
        fs: vec![],
        vsock: Some(VmVsock {
            cid: 3,
            socket: "/run/cloudhv/vm-1/vsock.sock".to_string(),
        }),
        serial: Some(VmConsoleConfig::off()),
        console: Some(VmConsoleConfig::off()),
        balloon: None,
        tpm: None,
    };

    group.bench_function("serialize_json", |b| {
        b.iter(|| {
            black_box(serde_json::to_string(&config).unwrap());
        });
    });

    let json = serde_json::to_string(&config).unwrap();
    group.bench_function("deserialize_json", |b| {
        b.iter(|| {
            black_box(serde_json::from_str::<VmConfig>(&json).unwrap());
        });
    });

    group.bench_function("serialize_runtime_config", |b| {
        let rt_config = RuntimeConfig {
            cloud_hypervisor_binary: "/usr/local/bin/cloud-hypervisor".into(),
            kernel_path: "/opt/cloudhv/vmlinux".into(),
            rootfs_path: "/opt/cloudhv/rootfs.ext4".into(),
            default_vcpus: 1,
            max_default_vcpus: 0,
            default_memory_mb: 128,
            vsock_port: 10789,
            agent_startup_timeout_secs: 10,
            kernel_args: "console=hvc0 root=/dev/vda rw quiet".into(),
            debug: false,
            max_containers_per_vm: 4,
            hotplug_memory_mb: 512,
            hotplug_method: "virtio-mem".into(),
            tpm_enabled: false,
            daemon_socket: String::new(),
        };
        b.iter(|| {
            black_box(serde_json::to_string(&rt_config).unwrap());
        });
    });

    group.finish();
}

/// Benchmark stable hash (used for erofs cache key).
fn bench_stable_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashing");

    group.bench_function("stable_hash_hex", |b| {
        let input = "/var/lib/containerd/io.containerd.snapshotter.v1.overlayfs/snapshots/42/fs:/var/lib/containerd/io.containerd.snapshotter.v1.overlayfs/snapshots/41/fs";
        b.iter(|| {
            black_box(containerd_shim_cloudhv::instance::stable_hash_hex(input));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_vm_config_serialization, bench_stable_hash,);
criterion_main!(benches);
