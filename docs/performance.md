# Performance

All measurements on Azure D8s_v5 (8 vCPU, 32 GB, KVM via nested virtualization).

## Cold Boot

Full VM boot from scratch — kernel boots, agent starts, container created and started.

| Phase | Latency |
|-------|---------|
| VM boot (sandbox) | **257ms** |
| Container create (disk image + hot-plug) | **58ms** |
| Container start (crun run) | **145ms** |
| **Total cold start** | **~460ms** |
| Sandbox stop + cleanup | **92ms** |

## Snapshot Restore

Restore from a pre-captured golden snapshot — skips kernel boot and agent initialization.

| Phase | Latency |
|-------|---------|
| Golden snapshot creation (one-time) | **~170ms** |
| VM restore from snapshot | **~55ms** |
| Network hot-add (post-restore) | **~50ms** |
| **Total restore + networking** | **~105ms** |

The golden snapshot captures a fully-booted VM with the agent running. It's created
lazily on first use and reused for all subsequent restores. Pool warming uses snapshot
restore when available, falling back to cold boot transparently.

## Resource Overhead (per VM)

| Component | RSS |
|-----------|-----|
| Cloud Hypervisor (VMM) | ~50 MB |
| virtiofsd | ~5 MB (0 with embedded mode) |
| Shim process | ~10 MB |
| **Total host overhead** | **~65 MB** (60 MB with embedded virtiofsd) |
| VM guest memory | 128–512 MB (configurable) |

### Embedded virtiofsd

With `--features embedded-virtiofsd`, virtiofsd runs as a thread inside the shim
process instead of a separate daemon:

| Metric | Spawned | Embedded |
|--------|---------|----------|
| virtiofsd startup | ~10ms | **277µs** |
| RSS per VM | ~5 MB | **0** (shared in shim) |
| Processes per VM | 3 (CH + virtiofsd + shim) | **2** (CH + shim) |

## Density

At 100 VMs per node with 128 MB guest memory:

| Component | Total |
|-----------|-------|
| Host process overhead | ~6.5 GB (6.0 GB with embedded virtiofsd) |
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
| **Cold start** | ~460ms boot, ~55ms snapshot restore | ~500ms–1s |
| **Shim binary** | 2.4 MB | ~50 MB |
| **Agent binary** | 1.5 MB (static) | ~20 MB |
| **Guest rootfs** | 16 MB (agent + crun only) | ~150 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **TCB** | Minimal | Full Linux userspace |
| **Language** | Rust | Go |
