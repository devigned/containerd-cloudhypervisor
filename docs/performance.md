# Performance

All measurements from hl-dev (Azure D8s_v5, KVM, Cloud Hypervisor v44.0,
containerd 2.2.2).

## Cold Boot

Full VM lifecycle from scratch — kernel boots, agent starts, container runs.

| Phase | Cache Hit | Cache Miss (first image) |
|-------|-----------|--------------------------|
| Sandbox (VM boot) | **~97ms** | **~97ms** |
| Container create (devmapper passthrough or cache cp + inline RPC) | **~8ms** | **~64ms** |
| Container start (agent RPC) | **~160ms** | **~160ms** |
| Exit detection | **~100ms** | **~100ms** |
| **Total e2e** | **~365ms** | **~420ms** |

Cache miss occurs only on the first container per unique container image on
each node. All subsequent containers using the same image get a cache hit.

> **Note:** With devmapper snapshotter, container create is near-zero since
> the thin snapshot is passed directly — no image creation or copying needed.
> The cache hit/miss distinction applies only to the overlayfs fallback path.

## Rootfs Delivery Performance

The shim caches rootfs ext4 images at `/opt/cloudhv/cache/<hash>.img`,
eliminating `mkfs.ext4` from the container startup hot path.

| Metric | Without Cache | With Cache |
|--------|--------------|------------|
| 50 concurrent containers (same image) | 22,459ms | **655ms** |
| Burst failures | ~3-15% | **0%** |
| Per-container disk creation | ~460ms | **~50ms** |
| Speedup | — | **34×** |

## Resource Overhead (per VM)

| Component | RSS |
|-----------|-----|
| Cloud Hypervisor (VMM) | ~50 MB |
| Shim process | ~7 MB |
| VM guest memory | 128–512 MB (configurable) |
| **Total host overhead** | **~57 MB** |

No virtiofsd process — block-device-only architecture (2 processes per VM).

## Density

At 100 VMs per node with 128 MB guest memory:

| Component | Total |
|-----------|-------|
| Host process overhead | ~5.7 GB |
| Guest memory | ~12.8 GB |
| **Total** | **~18.5 GB** |

## Comparison

Benchmarked on identical hardware (Azure D8s_v5, 3 nodes):

| | containerd-cloudhypervisor | Kata Containers 3.27 |
|---|---|---|
| **Sandbox boot** | ~97ms | ~876ms |
| **Total e2e** | ~420ms | ~1,134ms |
| **VMM memory (RSS)** | ~50 MB | ~144 MB |
| **Shim memory (RSS)** | ~7 MB | ~45 MB |
| **Shim binary** | ~4 MB | ~65 MB |
| **Guest rootfs** | 16 MB | ~257 MB |
| **Total on disk** | 44 MB | 651 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Architecture** | Block-device-only (no FUSE) | virtio-fs + block |
| **Language** | Rust | Go |
