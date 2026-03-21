# Performance

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
| Agent connect | ~361ms | ~17ms |
| ConfigureNetwork | — | ~17ms |
| Container start | ~5ms | 0ms (skipped) |
| **Total shim time** | **~420ms** | **~344ms** |

### Scale Performance

| Metric | Value |
|--------|-------|
| 150 pods → all Ready | **77s** (3 × D8ds_v5) |
| Scale-down 150 → 0 | **12s** (clean termination) |
| 10 consecutive sandbox runs | **10/10 pass** |

## Cold Boot (crictl, single pod)

Full VM lifecycle from scratch — kernel boots, agent starts, container runs.

Measured on hl-dev (Azure D8s_v5, KVM, Cloud Hypervisor v44.0):

| Phase | Cache Hit | Cache Miss (first image) |
|-------|-----------|--------------------------|
| Sandbox (VM boot) | **~97ms** | **~97ms** |
| Container create (erofs cache) | **~3ms** | **~64ms** |
| Container start (agent RPC) | **~160ms** | **~160ms** |
| Exit detection | **~100ms** | **~100ms** |
| **Total e2e** | **~365ms** | **~420ms** |

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
| **Warm restore** | ~344ms | N/A |
| **Cold boot e2e** | ~420ms | ~1,134ms |
| **150-pod scale** | 150/150 (77s) | 130/150 (stuck) |
| **Memory per pod** | ~5 MiB (CoW) / ~57 MiB (cold) | ~312 MiB |
| **Shim binary** | ~4.6 MB | ~65 MB |
| **Guest rootfs** | 5.4 MB (erofs) | ~257 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Architecture** | Block-device-only (no FUSE) | virtio-fs + block |
| **Language** | Rust | Go |
