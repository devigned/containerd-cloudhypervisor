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
- **Communication: vsock + ttrpc — no network stack, no shared filesystem.
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
- **App container** (`container-type=container`): clones a cached rootfs ext4 image,
  hot-plugs it into the running VM, and sends config.json + volume data inline via the
  CreateContainer RPC. The guest agent writes metadata to tmpfs and runs the container with crun.

## Container Rootfs via Block Devices (Cached + Inline Metadata)

Following the same approach as [firecracker-containerd](https://github.com/firecracker-microvm/firecracker-containerd),
with a rootfs image cache and inline metadata delivery:

### Cache Miss (first container per image)

1. Host shim mounts the container rootfs from containerd's overlayfs snapshot
2. Computes content hash of the rootfs (FNV-1a over sorted file metadata)
3. Creates ext4 disk image containing the rootfs via `mkfs.ext4 -d`
4. Saves to `/opt/cloudhv/cache/<hash>.img` (persists across pods)
5. Hot-plugs the rootfs disk into the VM via `vm.add-disk`
6. Sends config.json + volume file contents inline in the CreateContainer RPC
7. Guest agent mounts the rootfs disk, writes metadata to tmpfs, runs container

### Cache Hit (subsequent containers of same image)

1. Host shim computes the same content hash → finds cached image
2. `cp` the cached image to a per-container path (~50ms, no mkfs.ext4)
3. Hot-plugs the single rootfs disk
4. Sends config.json + volumes inline via RPC → agent writes to tmpfs

The cache eliminates `mkfs.ext4` from the hot path for all but the very first container
using each image. Metadata (config.json + ConfigMap/Secret files) is delivered inline
via the existing vsock RPC — no second disk, no mkfs.ext4 for metadata. Under burst
load (50 concurrent containers), this reduces disk creation time from 22.5s to 0.65s —
**34× faster with zero failures**.

## Volumes (CSI, ConfigMap, Secret, emptyDir)

All Kubernetes volume types are transported into the VM using block devices:

| Volume Type | Transport | Guest Access | Writes Persist |
|-------------|-----------|-------------|----------------|
| Block PVC (raw) | `vm.add-disk` hot-plug | `/dev/vdX` direct I/O | Yes |
| Filesystem PVC | Inline via RPC (tmpfs) | bind mount | No (read-only) |
| ConfigMap | Inline via RPC (tmpfs) | bind mount | No (read-only) |
| Secret | Inline via RPC (tmpfs) | bind mount | No (read-only) |
| emptyDir | Separate hot-plugged disk | bind mount | Yes (pod lifetime) |

### How It Works

1. The shim reads the OCI spec's mounts array from the container bundle
2. System mounts (`/proc`, `/dev`, `/sys`) are skipped
3. For each volume:
   - **Block devices**: hot-plugged into the VM via `vm.add-disk`, agent
     discovers and mounts the new `/dev/vdX` device
   - **Filesystem volumes**: source data is copied into the metadata disk
     image under `volumes/<hash>/` during metadata disk creation. The
     agent bind-mounts them from the metadata disk at the expected paths.
4. Volume metadata is passed to the agent via the `CreateContainer` RPC
5. The agent injects the volumes as mounts in the adapted OCI spec

Read-only filesystem volumes (ConfigMaps, Secrets) are baked into the metadata
disk alongside config.json — separate from the cached rootfs disk.
Block PVCs and emptyDir volumes use separate hot-plugged disks. No FUSE, no
loopback mounts, no shared filesystem.

> **Limitation:** Writable filesystem PVCs are not currently supported.
> Writes to baked-in volumes do not persist back to the host. This requires
> a shared filesystem transport which is planned for a future release.

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

Container stdout/stderr flows from the guest to `kubectl logs` via vsock:

1. The guest agent captures `crun` stdout/stderr via piped file descriptors
2. The agent buffers output and serves it via the `GetContainerLogs` ttrpc RPC
3. The host shim polls `GetContainerLogs` every 10ms and writes to containerd's stdio FIFOs
4. containerd delivers them as standard container logs (`crictl logs`, `kubectl logs`)

This approach uses no shared filesystem — all log data flows over the existing
vsock connection between the shim and the guest agent.

Infrastructure errors (VM boot failures, API errors, disk hot-plug issues) are logged
via the shim's own logger and appear in the containerd log (`journalctl -u containerd`),
keeping operator diagnostics separate from application output.

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
