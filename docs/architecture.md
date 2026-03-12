# Architecture

## System Overview

```text
                     ┌─────────────────────────────────────────────────────────┐
                     │  Pod Network Namespace                                  │
containerd           │                                                         │
   │                 │  ┌──────┐  TC redirect  ┌──────┐                        │
   │ ttrpc           │  │ veth ├──────────────►│ TAP  │                        │
   │                 │  │(eth0)│◄──────────────┤      │                        │
   ▼                 │  └───┬──┘               └──┬───┘                        │
┌──────────────┐     │      │                     │                            │
│  shim-v1     ├─────┤      │ IP flushed          │ virtio-net                 │
│              │     │      │ (VM owns pod IP)    │                            │
│  • disk img  │     │  ┌───┴─────────────────────┴────────────────────────┐   │
│  • hot-plug  │     │  │  cloud-hypervisor (VMM)                          │   │
│  • logs      │     │  │  ┌─────────────────────────────────────────────┐ │   │
│              │     │  │  │  Guest VM (custom kernel)                   │ │   │
└──────┬───────┘     │  │  │                                             │ │   │
       │ vsock       │  │  │  eth0 ← kernel ip= (IP_PNP at boot)         │ │   │
       │             │  │  │                                             │ │   │
       │             │  │  │  ┌───────────┐     ┌──────┐                 │ │   │
       └─────────────┤  │  │  │   Agent   │────►│ crun │ (containers)    │ │   │
                     │  │  │  │  (PID 1)  │     └──────┘                 │ │   │
                     │  │  │  └───────────┘                              │ │   │
                     │  │  └─────────────────────────────────────────────┘ │   │
                     │  └──────────────────────────────────────────────────┘   │
                     └─────────────────────────────────────────────────────────┘
```

## Components

- **Host shim** (`containerd-shim-cloudhv-v1`): containerd shim v2, manages VM lifecycle,
  creates disk images, hot-plugs block devices, sets up networking, forwards logs.
- **Guest agent** (`cloudhv-agent`): PID 1 in the VM, discovers hot-plugged disks, adapts
  OCI specs, delegates to crun. Built as a separate workspace with its own ttrpc 0.9 dependency.
- **Communication**: vsock + ttrpc — no network stack for the control plane.
- **Container runtime**: crun (1.8 MB static) — lighter than runc (10 MB).
- **Kernel**: Custom kernel (~27 MB) with virtio, vsock, BPF, ACPI hot-plug,
  IP_PNP, and virtio-net. Supports both x86_64 (PVH boot, `console=hvc0`) and
  ARM64/aarch64 (direct kernel boot, PL011 serial `console=ttyAMA0`).

> **⚠️ ARM64 support is experimental.** All binaries compile and the guest kernel
> config is in place, but integration tests cannot run in CI because GitHub's
> ARM64 runners (`ubuntu-24.04-arm`) do not expose `/dev/kvm`
> ([actions/partner-runner-images#147](https://github.com/actions/partner-runner-images/issues/147)).
> ARM64 integration testing must be done manually on a KVM-capable ARM64 host
> until GitHub enables nested virtualization on ARM runners.

## Sandbox and Container Split

The shim uses the `io.kubernetes.cri.container-type` annotation to distinguish between
sandbox creation and application containers:

- **Sandbox** (`container-type=sandbox`): boots the Cloud Hypervisor VM. The VM **is** the
  sandbox — no pause container needed. Networking (TAP + TC redirect) is set up at this stage.
- **App container** (`container-type=container`): creates an ext4 disk image from the container
  rootfs, hot-plugs it into the running VM, and the guest agent runs it with crun.

## Container Rootfs via Block Devices

Following the same approach as [firecracker-containerd](https://github.com/firecracker-microvm/firecracker-containerd):

1. Host shim mounts the container rootfs from containerd's overlayfs snapshot
2. Creates an ext4 disk image containing the OCI bundle + rootfs
3. Hot-plugs the disk into the running VM via Cloud Hypervisor's `vm.add-disk` API
4. Guest agent discovers the new `/dev/vdX` block device and mounts it
5. `crun` runs the container with mount + PID namespaces on the real ext4 filesystem

This avoids FUSE in the container data path and enables proper `pivot_root`.

## Volumes (CSI, ConfigMap, Secret)

Kubernetes volumes are transported into the VM using a dual-path approach
optimized for each volume type:

| Volume Type | Host Form | Transport | Guest Access | Writes Persist |
|-------------|-----------|-----------|-------------|----------------|
| Block PVC (raw) | `/dev/sdX` | `vm.add-disk` hot-plug | `/dev/vdX` direct I/O | Yes |
| Filesystem PVC | directory | virtio-fs bind mount | bind mount | Yes |
| ConfigMap | directory | virtio-fs bind mount | bind mount | No (read-only) |
| Secret | directory | virtio-fs bind mount | bind mount | No (read-only) |
| emptyDir | directory | virtio-fs bind mount | bind mount | Yes (pod lifetime) |

### How It Works

1. The shim reads the OCI spec's mounts array from the container bundle
2. For each volume mount (skipping system mounts like `/proc`, `/dev`, `/sys`):
   - **Block devices**: detected via `is_block_device()`, hot-plugged into the VM
     via `vm.add-disk`, agent discovers and mounts the new `/dev/vdX` device
   - **Filesystem paths**: bind-mounted into the virtio-fs shared directory,
     giving the guest a live view of the host data through virtio-fs
3. Volume metadata (destination, source, type, readonly) is passed to the agent
   via the `CreateContainer` RPC
4. The agent injects the volumes as mounts in the adapted OCI spec
5. `crun` mounts them at the expected paths inside the container

Block device passthrough avoids FUSE overhead for I/O-intensive workloads.
Filesystem sharing via virtio-fs preserves write persistence — changes inside
the container propagate back to the host PV.

## Networking

VM networking follows the [tc-redirect-tap](https://github.com/firecracker-microvm/firecracker-containerd)
pattern used by firecracker-containerd, adapted for Cloud Hypervisor:

1. **TAP creation**: the shim creates a TAP device inside the pod's network namespace
2. **TC redirect**: bidirectional `tc filter` rules redirect all traffic between the
   CNI veth and the TAP device at layer 2
3. **IP flush**: the pod IP is removed from the veth so packets traverse TC into the VM
4. **Kernel IP_PNP**: the pod IP, gateway, and netmask are passed as a kernel boot
   parameter (`ip=<addr>::<gw>:<mask>::eth0:off`), so the guest kernel configures the
   interface at boot — no agent-side networking code needed
5. **CH in netns**: Cloud Hypervisor is launched inside the pod network namespace
   (via `nsenter`) so it can access the TAP device

The result is that the VM's `eth0` has the pod IP and responds to traffic on the pod
network, fully transparent to CNI and Kubernetes services.

## Container Logs

Container stdout/stderr flows from the guest to `kubectl logs` without any agent-side
log infrastructure:

1. `crun` inside the VM writes stdout/stderr to files on the virtio-fs shared directory
2. The host shim tails these files and forwards lines to containerd's stdio FIFOs
3. containerd delivers them as standard container logs (`crictl logs`, `kubectl logs`)

Infrastructure errors (VM boot failures, API errors, disk hot-plug issues) are logged
via the shim's own logger and appear in the containerd log (`journalctl -u containerd`),
keeping operator diagnostics separate from application output.

## Snapshot/Restore

The `SnapshotManager` captures a **golden snapshot** of a fully-booted VM (kernel up,
agent running) and restores copies in ~30ms instead of cold-booting (~300ms):

1. **Golden snapshot creation** (one-time, ~115ms): boot a minimal VM (disk + vsock,
   no virtiofs), verify agent health, pause, snapshot to disk, shut down
2. **Restore** (~32ms): start new CH process, restore from snapshot with per-instance
   config.json (rewritten vsock paths, symlinked memory file), resume
3. **Post-restore networking** (~50ms): hot-add virtio-net via `vm.add-net` API

The golden snapshot excludes virtiofs because the vhost-user protocol state cannot
reconnect to a fresh virtiofsd after restore. Container operations that need the
shared directory (disk image hot-plug, I/O file forwarding) use full VM boot.
The VM pool uses snapshot restore when a golden snapshot is available, falling back
to cold boot transparently.

## Embedded virtiofsd

With the `embedded-virtiofsd` feature, virtiofsd runs as a thread inside the shim
process instead of a separate daemon. This eliminates ~5 MB RSS per VM and reduces
virtiofsd startup from ~10ms to ~277µs. The vhost-user socket is still created —
Cloud Hypervisor connects to it the same way — but no child process is spawned.

```bash
cargo build --release -p containerd-shim-cloudhv --features embedded-virtiofsd
```

Requires `libseccomp-dev` and `libcap-ng-dev` on Linux.

## Dynamic Memory Management

VMs can grow and shrink memory on demand using virtio-mem hotplug, bridging the
gap between Kubernetes resource requests (boot memory) and limits (max memory).

### Configuration

Memory growth activates automatically when a pod's resource limit exceeds its request:

```yaml
resources:
  requests:
    memory: "128Mi"   # → VM boot memory
  limits:
    memory: "1Gi"     # → max memory (hotplug ceiling)
```

Or via annotation:

```yaml
annotations:
  io.cloudhv.config.hypervisor.default_memory: "128"
  io.cloudhv.config.hypervisor.memory_limit: "1024"
```

When limit > request, the shim automatically:
- Sets `hotplug_memory_mb = limit - request` (896 MiB headroom)
- Selects virtio-mem for bidirectional resize
- Adds a balloon device with free page reporting

### Growth

Two mechanisms trigger memory growth, from fastest to slowest:

1. **PSI pressure watcher** (< 1s response): the agent monitors
   `/proc/pressure/memory` using a kernel PSI trigger. When 100ms of memory
   stall accumulates in any 1s window, the agent writes a signal file to the
   shared directory. The shim detects it and calls `vm.resize(+128MiB)` immediately.

2. **Periodic polling** (5s cycle): the shim polls the agent's `GetMemInfo` RPC.
   When `MemAvailable` drops below 20% of `MemTotal`, it grows memory in 128 MiB steps.

### Reclaim

When `MemAvailable` exceeds 50% of `MemTotal` for 60 consecutive seconds, the shim
calls `vm.resize(-128MiB)` to return memory to the host. The floor is the original
request — memory never shrinks below boot size.

The balloon device with `free_page_reporting=on` lets the guest proactively report
freed pages to the host for immediate reclaim, complementing the virtio-mem resize.
