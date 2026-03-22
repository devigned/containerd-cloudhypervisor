# Performance

## Daemon Pool Acquire (single pod)

With the sandbox daemon, VMs are pre-booted in a pool via CH v51 OnDemand
restore. Acquiring a VM is near-instant.

| Scenario | Latency |
|----------|--------:|
| Pool hit (pre-booted VM available) | **0ms** |
| Pool empty (synchronous OnDemand restore) | **~25ms** |

## Cold Path (shim inner, daemon mode)

Measured on AKS (3 × D8ds_v5, Kubernetes v1.33.7, containerd 2.0.0):

| Phase | Time |
|-------|-----:|
| erofs conversion (cached) | 0ms |
| erofs conversion (uncached, mkfs.erofs) | ~8ms |
| daemon.AcquireSandbox (pool hit) | ~63ms |
| Hot-plug + RunContainer RPC | ~11ms |
| **Cold path shim inner total** | **~74ms** |

## Warm Snapshot Path (daemon mode)

When a warm workload snapshot exists (created by a shadow VM in the background):

| Phase | Time |
|-------|-----:|
| daemon.AcquireSandbox (warm snapshot restore) | ~24ms |
| Agent connect | ~80ms |
| ConfigureNetwork RPC | ~50ms |
| Container adoption | ~14ms |
| **Warm path total** | **~168ms** |

The workload wakes up **already running** — no kernel boot, no agent init,
no application startup.

## Scale Performance

| Metric | Daemon (current) | v0.11.0 (warm restore) | v0.7.0 |
|--------|------------------:|----------------------:|-------:|
| 150 pods → all Ready | **11s** | 77s | 27s |
| Scale-down 150 → 0 | 12s | 12s | — |
| Infra | 3 × D8ds_v5 | 3 × D8ds_v5 | 3 × D8ds_v5 |

## Resource Overhead

### Per-VM Memory

| Component | Value |
|-----------|------:|
| VM RSS (128 MB allocated, OnDemand userfaultfd) | **~24 MB** |
| Pool VM idle RSS | ~6 MB |
| Shim process RSS | ~6 MB |
| Daemon RSS (total, not per-VM) | ~2.2 MB |

### Node Memory at 150 Pods (3 nodes, 50 VMs per node)

| Metric | Value |
|--------|------:|
| Per-node VM memory | ~3.2 GB |
| Per-node memory utilization | **~10%** of 32 GB |
| Total cluster memory (3 nodes) | ~9.6 GB |

### Cache Sizes (per node)

| Cache | Size |
|-------|-----:|
| erofs cache (per unique image) | ~8.6 MB |
| Base snapshot (daemon state) | ~513 MB |

## Rootfs Delivery Performance

The shim caches rootfs as erofs images at `/run/cloudhv/erofs-cache/<hash>.erofs`,
content-addressed by image digest. flock-serialized for concurrent builds.

| Metric | Without Cache | With Cache |
|--------|--------------|------------|
| 50 concurrent containers (same image) | 22,459ms | **655ms** |
| Burst failures | ~3-15% | **0%** |
| Per-container disk creation | ~460ms | **~3ms** |
| Speedup | — | **153×** |

## Comparison with Kata Containers

Measured on identical AKS infrastructure (3 × D8ds_v5, 96 GiB total RAM):

| | containerd-cloudhypervisor | Kata Containers |
|---|---|---|
| **Cold start (shim inner)** | ~74ms | ~500ms–1s |
| **Warm restore** | ~168ms | N/A |
| **150-pod scale** | 150/150 in **11s** | 130/150 (OOM at 130) |
| **Memory per pod** | **~24 MB** | ~330 MB |
| **Node memory at 150 pods** | **10%** | 43%+ (OOM) |
| **Shim binary** | ~4.6 MB | ~65 MB |
| **Guest rootfs** | 5.4 MB (erofs) | ~257 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Architecture** | Block-device-only (no FUSE) | virtio-fs + block |
| **Language** | Rust | Go |
