# Performance

All measurements from CI (GitHub Actions, ubuntu-latest, KVM via nested virtualization).

## Cold Boot

Full VM lifecycle from scratch — kernel boots, agent starts, ttrpc connects.

| Phase | Latency |
|-------|---------|
| Shim setup (new + prepare) | **~0.1ms** |
| Cloud Hypervisor startup | **~6ms** |
| VM create + boot (CH API) | **~16ms** |
| Guest boot + agent ready | **~231ms** |
| ttrpc connect | **~0.3ms** |
| **Total cold start** | **~280ms** |
| Shutdown + cleanup | **~19ms** |

End-to-end container lifecycle (boot + disk create + hot-plug + container start):

| Phase | Latency |
|-------|---------|
| VM boot | **~250ms** |
| Disk image create | **~31ms** |
| Hot-plug + CreateContainer | **~64ms** |
| StartContainer | **~2ms** |
| **Total** | **~350ms** |

## Snapshot Restore

Restore from a pre-captured golden snapshot — skips kernel boot and agent initialization.

| Phase | Latency |
|-------|---------|
| Golden snapshot creation (one-time) | **~115ms** |
| VM restore from snapshot | **~32ms** |
| Network hot-add (post-restore) | **~50ms** |
| **Total restore + networking** | **~82ms** |

The golden snapshot captures a fully-booted VM with the agent running. It's created
lazily on first use and reused for all subsequent restores. Pool warming uses snapshot
restore when available, falling back to cold boot transparently.

## Resource Overhead (per VM)

| Component | RSS |
|-----------|-----|
| Cloud Hypervisor (VMM) | ~50 MB |
| Shim process | ~10 MB |
| VM guest memory | 128–512 MB (configurable) |


process instead of a separate daemon:

| Metric | Spawned | Embedded |
|--------|---------|----------|
| RSS per VM | ~5 MB | **0** (shared in shim) |

## Density

At 100 VMs per node with 128 MB guest memory:

| Component | Total |
|-----------|-------|
| Guest memory | ~12.8 GB |
| **Total** | **~19.3 GB** |

With 512 MB guest memory:

| Component | Total |
|-----------|-------|
| Host process overhead | ~6.5 GB |
| Guest memory | ~51.2 GB |
| **Total** | **~57.7 GB** |

## Comparison

| | containerd-cloudhypervisor | Kata Containers |
|---|---|---|
| **Cold start** | ~300ms boot, ~30ms snapshot restore | ~500ms–1s |
| **Shim binary** | 2.4 MB | ~50 MB |
| **Agent binary** | 1.5 MB (static) | ~20 MB |
| **Guest rootfs** | 16 MB (agent + crun only) | ~150 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **TCB** | Minimal | Full Linux userspace |
| **Language** | Rust | Go |
