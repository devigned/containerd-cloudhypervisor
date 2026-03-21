# Performance

## Cold Boot with Async Eager Boot (crictl, single pod)

The shim boots the VM asynchronously during sandbox creation, overlapping
with containerd's internal processing. Container rootfs disks are hot-plugged
after boot; the agent discovers them via inotify (<1ms).

Measured on hl-dev (Azure D8s_v5, KVM, Cloud Hypervisor v44.0, 1 vCPU):

### Cached erofs (5 runs)

| Phase | Min | Max | Avg |
|-------|----:|----:|----:|
| start_sandbox (async boot spawned) | 7ms | 8ms | 7ms |
| containerd gap (not our code) | 82ms | 93ms | 88ms |
| Boot await (residual after overlap) | 0ms | 90ms | 55ms |
| Hot-plug + inotify + mount + crun | 14ms | 24ms | 18ms |
| **start_container total** | **26ms** | **110ms** | **80ms** |

Best case: **26ms** (boot finished during containerd gap).

### Uncached erofs (first image)

| Phase | Time |
|-------|-----:|
| start_sandbox | 7ms |
| Boot await (overlapped with erofs) | 0ms |
| erofs conversion (mkfs.erofs) | 43ms |
| Hot-plug + mount + crun | 14ms |
| **start_container total** | **59ms** |

### Optimization breakdown

| Version | start_container (cached) | Key change |
|---------|------------------------:|------------|
| v0.10.0 | ~530ms | Baseline (sequential boot) |
| v0.11.0 | ~340ms | Warm snapshots, tight netlink |
| v0.12.0-dev | **~26-110ms** | Async boot, tight-poll agent, inotify mount |

## Warm Snapshot Restore (AKS)

Warm snapshot restore eliminates kernel boot and workload startup for all
pods after the first. The first pod cold-boots and creates a snapshot; all
subsequent pods restore from it with CoW (copy-on-write) memory.

Measured on AKS (3 × D8ds_v5, Kubernetes v1.33.7, containerd 2.0.0):

| Phase | Cold Boot | Warm Restore |
|-------|----------:|-----------:|
| TAP setup (netlink) | <1ms | <1ms |
| VMM spawn + ready | 6ms | 6ms |
| VM boot / restore | ~35ms | ~293ms |
| Agent connect | ~170ms | ~17ms |
| ConfigureNetwork | — | ~17ms |
| Container start | ~18ms | 0ms (skipped) |
| **Total shim time** | **~240ms** | **~344ms** |

### Scale Performance

| Metric | Value |
|--------|-------|
| 150 pods → all Ready | **77s** (3 × D8ds_v5) |
| Scale-down 150 → 0 | **12s** (clean termination) |
| 10 consecutive sandbox runs | **10/10 pass** |

## Rootfs Delivery Performance

The shim caches rootfs as erofs images at `/run/cloudhv/erofs-cache/<hash>.erofs`,
content-addressed by overlayfs lowerdir paths. flock-serialized for concurrent builds.

| Metric | Without Cache | With Cache |
|--------|--------------|------------|
| 50 concurrent containers (same image) | 22,459ms | **655ms** |
| Burst failures | ~3-15% | **0%** |
| Per-container disk creation | ~460ms | **~3ms** |
| Speedup | — | **153×** |

## Resource Overhead (per VM)

| Component | Cold Boot | Warm Restore (CoW) |
|-----------|----------:|----:|
| Cloud Hypervisor (VmRSS) | ~50 MiB | ~529 MiB (524 MiB shared) |
| Cloud Hypervisor (unique) | ~50 MiB | **~5 MiB** |
| Shim process (sandbox) | ~5.5 MiB | ~5.5 MiB |
| Shim process (container) | ~1.4 MiB | ~1.4 MiB |
| **Total unique per pod** | **~57 MiB** | **~12 MiB** |

No virtiofsd process — block-device-only architecture (2 processes per VM).

### Cache Sizes (per node, per workload image)

| Cache | Size |
|-------|-----:|
| Snapshot cache | ~513 MiB |
| erofs cache | ~8.6 MiB |

## Density

With warm snapshot restore at 150 VMs across 3 nodes (50 per node):

| Component | Per Node | Total (3 nodes) |
|-----------|---------|------|
| Unique VM memory | ~250 MiB | ~750 MiB |
| Snapshot (shared) | ~513 MiB | ~1.5 GiB |
| System overhead | ~1 GiB | ~3 GiB |
| **Total used** | **~6 GiB** | **~14 GiB** |
| **Available (of 32 GiB)** | **~26 GiB** | **~82 GiB** |

## Comparison

| | containerd-cloudhypervisor | Kata Containers |
|---|---|---|
| **Cold start (cached)** | ~26-110ms (async boot) | ~500ms–1s |
| **Warm restore** | ~344ms | N/A |
| **150-pod scale** | 150/150 (77s) | 130/150 (stuck) |
| **Memory per pod** | ~5 MiB (CoW) / ~57 MiB (cold) | ~312 MiB |
| **Shim binary** | ~4.6 MB | ~65 MB |
| **Guest rootfs** | 5.4 MB (erofs) | ~257 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Architecture** | Block-device-only (no FUSE) | virtio-fs + block |
| **Language** | Rust | Go |
