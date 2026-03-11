//! Benchmarks for containerd-cloudhypervisor overhead measurements.
//!
//! Measures:
//! - Image layer cache operations (in-memory, no I/O)
//! - VM config serialization overhead
//! - CID allocation throughput
//! - Pool acquire/release cycle (in-memory)
//!
//! Run with: cargo bench -p containerd-shim-cloudhv
//!
//! For full VM lifecycle benchmarks (requires KVM + root), use
//! the integration test suite with timing output.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use cloudhv_common::types::*;
use containerd_shim_cloudhv::image_cache::ImageLayerCache;

/// Benchmark image layer cache operations (pure in-memory + filesystem).
fn bench_image_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("image_cache");

    group.bench_function("ensure_layer_new", |b| {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageLayerCache::new(dir.path().to_path_buf());
        let mut i = 0u64;
        b.iter(|| {
            let digest = format!("sha256:{:064x}", i);
            i += 1;
            black_box(cache.ensure_layer(&digest).unwrap());
        });
    });

    group.bench_function("ensure_layer_cached", |b| {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageLayerCache::new(dir.path().to_path_buf());
        cache.ensure_layer("sha256:cached_layer").unwrap();
        b.iter(|| {
            black_box(cache.ensure_layer("sha256:cached_layer").unwrap());
        });
    });

    group.bench_function("release_layer", |b| {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageLayerCache::new(dir.path().to_path_buf());
        // Pre-fill with high refcount
        for _ in 0..1000 {
            cache.ensure_layer("sha256:release_test").unwrap();
        }
        b.iter(|| {
            cache.release_layer(black_box("sha256:release_test"));
        });
    });

    group.bench_function("is_cached_lookup", |b| {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageLayerCache::new(dir.path().to_path_buf());
        for i in 0..100 {
            cache.ensure_layer(&format!("sha256:layer_{i}")).unwrap();
        }
        let mut i = 0;
        b.iter(|| {
            let digest = format!("sha256:layer_{}", i % 100);
            i += 1;
            black_box(cache.is_cached(&digest));
        });
    });

    group.finish();
}

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
        fs: vec![VmFs {
            tag: "containerfs".to_string(),
            socket: "/run/cloudhv/vm-1/virtiofsd.sock".to_string(),
            num_queues: 1,
            queue_size: 128,
        }],
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
            virtiofsd_binary: "/usr/libexec/virtiofsd".into(),
            kernel_path: "/opt/cloudhv/vmlinux".into(),
            rootfs_path: "/opt/cloudhv/rootfs.ext4".into(),
            default_vcpus: 1,
            default_memory_mb: 128,
            vsock_port: 10789,
            agent_startup_timeout_secs: 10,
            kernel_args: "console=hvc0 root=/dev/vda rw quiet".into(),
            debug: false,
            pool_size: 3,
            max_containers_per_vm: 4,
            hotplug_memory_mb: 512,
            hotplug_method: "virtio-mem".into(),
            tpm_enabled: false,
        };
        b.iter(|| {
            black_box(serde_json::to_string(&rt_config).unwrap());
        });
    });

    group.finish();
}

/// Benchmark CID allocation (atomic counter throughput).
fn bench_cid_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("cid_allocation");

    group.bench_function("allocate_vm_manager", |b| {
        let config = RuntimeConfig {
            cloud_hypervisor_binary: "/usr/local/bin/cloud-hypervisor".into(),
            virtiofsd_binary: "/usr/libexec/virtiofsd".into(),
            kernel_path: "/opt/vmlinux".into(),
            rootfs_path: "/opt/rootfs.ext4".into(),
            default_vcpus: 1,
            default_memory_mb: 128,
            vsock_port: 10789,
            agent_startup_timeout_secs: 10,
            kernel_args: "console=hvc0".into(),
            debug: false,
            pool_size: 0,
            max_containers_per_vm: 1,
            hotplug_memory_mb: 0,
            hotplug_method: "acpi".into(),
            tpm_enabled: false,
        };
        let mut i = 0u64;
        b.iter(|| {
            let vm =
                containerd_shim_cloudhv::vm::VmManager::new(format!("bench-{i}"), config.clone())
                    .unwrap();
            i += 1;
            black_box(vm.cid());
        });
    });

    group.finish();
}

/// Benchmark hypervisor detection overhead.
fn bench_hypervisor_detection(c: &mut Criterion) {
    let mut group = c.benchmark_group("hypervisor");

    group.bench_function("detect_backend", |b| {
        b.iter(|| {
            black_box(containerd_shim_cloudhv::hypervisor::detect_hypervisor());
        });
    });

    group.bench_function("check_virt_support", |b| {
        b.iter(|| {
            black_box(containerd_shim_cloudhv::hypervisor::check_virtualization_support());
        });
    });

    group.finish();
}

/// Benchmark pool operations (in-memory, no actual VM creation).
fn bench_pool_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("vm_pool");

    group.bench_function("pool_new", |b| {
        let config = RuntimeConfig {
            cloud_hypervisor_binary: "/usr/local/bin/cloud-hypervisor".into(),
            virtiofsd_binary: "/usr/libexec/virtiofsd".into(),
            kernel_path: "/opt/vmlinux".into(),
            rootfs_path: "/opt/rootfs.ext4".into(),
            default_vcpus: 1,
            default_memory_mb: 128,
            vsock_port: 10789,
            agent_startup_timeout_secs: 10,
            kernel_args: "console=hvc0".into(),
            debug: false,
            pool_size: 10,
            max_containers_per_vm: 4,
            hotplug_memory_mb: 256,
            hotplug_method: "virtio-mem".into(),
            tpm_enabled: false,
        };
        b.iter(|| {
            let pool = containerd_shim_cloudhv::pool::VmPool::new(config.clone());
            black_box(pool.is_enabled());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_image_cache,
    bench_vm_config_serialization,
    bench_cid_allocation,
    bench_hypervisor_detection,
    bench_pool_operations,
);
criterion_main!(benches);
