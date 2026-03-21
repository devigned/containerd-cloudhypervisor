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

- **Sandbox** (`container-type=sandbox`): spawns the Cloud Hypervisor process and
  **eagerly boots the VM** with only the guest rootfs (`/dev/vda`). The VM boots
  asynchronously — `start_sandbox` returns immediately (~7ms) while boot + agent
  connect proceed in the background (~175ms). Networking (TAP + TC redirect) is
  set up before boot.
- **App container** (`container-type=container`): awaits the async boot (usually
  complete by now — 0ms wait), converts the rootfs to erofs (cached), hot-plugs
  it into the running VM, and sends config.json + volume data inline via the
  RunContainer RPC. The guest agent discovers hot-plugged disks via inotify (<1ms)
  and runs the container with crun.

### Async Eager Boot

The VM boots in the background during sandbox creation, overlapping with
containerd's internal work (~90ms gap between RunPodSandbox and StartContainer).
By the time `start_container` runs, the VM is usually booted and the agent is
connected, making the container start nearly instant:

```
start_sandbox (7ms):
  TAP setup → config → spawn VMM → SPAWN boot+agent (async) → return

  ─── containerd gap (~90ms) ─── boot running in background ───

start_container (~26-110ms):
  erofs lookup (0ms cached) → await boot (0-90ms) → hot-plug disk → RPC
```

The agent connect uses a two-phase strategy: tight-poll with `yield_now()`
for the first 500ms (catches the agent the instant it's ready), then
exponential backoff for slow/contended boots.

## Warm Snapshot Restore

The shim supports **warm workload snapshots** for near-instant pod startup.

### How it works

1. **Cold boot** (first pod per workload image per node): VM boots normally,
   agent starts, workload initializes (e.g., Python server starts listening).
   After 20s warmup, the VM is paused, snapshotted, and resumed. The snapshot
   is cached at `/run/cloudhv/snapshot-cache/<key>/`.

2. **Warm restore** (all subsequent pods): The snapshot config is patched with
   the new pod's TAP name, MAC, vsock CID, and serial console path. The VM is
   restored with CoW memory (userfaultfd), resumed, and the guest IP is
   reconfigured via the agent's `ConfigureNetwork` RPC. The workload wakes up
   **already running** — no kernel boot, no agent init, no workload startup.

### Why 0.0.0.0 makes this work

The workload binds `0.0.0.0:<port>`, which accepts on ALL interfaces. The
snapshot's TAP is patched to the new pod's TAP in the config before restore.
The guest sees the same eth0 backed by a different TAP. Since the workload
listens on `0.0.0.0`, traffic flows immediately — no rebind, no restart.

### Container adoption

The `AdoptContainer` RPC re-registers the snapshot's running container under
the new container ID so that kill/wait/state RPCs work correctly. An exit
watcher is spawned for clean termination on scale-down.

## Container Rootfs Delivery

The shim converts container rootfs to erofs images, cached at
`/run/cloudhv/erofs-cache/<hash>.erofs`. The cache is content-addressed
by the overlayfs lowerdir paths (FNV-1a 128-bit hash) and flock-serialized
for concurrent builds. One build per unique image; all subsequent pods
hardlink the cached image.

### Block Device Passthrough (devmapper snapshotter)

When containerd uses the devmapper snapshotter, the container rootfs is already
a thin-provisioned block device. The shim detects this via `/proc/self/mountinfo`
and passes the device path directly to Cloud Hypervisor's `vm.add-disk` API.

### erofs Cache (overlayfs snapshotter)

When the snapshotter produces a directory mount, the shim runs `mkfs.erofs`
to convert it into a compact read-only image. The erofs image is cached and
hardlinked for all subsequent pods using the same container image.

In both tiers, config.json and volume data (ConfigMaps, Secrets) are delivered
inline via the CreateContainer RPC — the agent writes them to the bundle
directory. No second disk, no virtiofsd.

## Volumes (CSI, ConfigMap, Secret, emptyDir)

All Kubernetes volume types are transported into the VM using block devices:

| Volume Type | Transport | Guest Access | Writes Persist |
|-------------|-----------|-------------|----------------|
| Block PVC (raw) | `vm.add-disk` hot-plug | `/dev/vdX` direct I/O | Yes |
| Filesystem PVC | Inline via RPC | bind mount | No (read-only) |
| ConfigMap | Inline via RPC | bind mount | No (read-only) |
| Secret | Inline via RPC | bind mount | No (read-only) |
| emptyDir | Separate hot-plugged disk | bind mount | Yes (pod lifetime) |

### How It Works

1. The shim reads the OCI spec's mounts array from the container bundle
2. System mounts (`/proc`, `/dev`, `/sys`) are skipped
3. For each volume:
   - **Block devices**: hot-plugged into the VM via `vm.add-disk`, agent
     discovers and mounts the new `/dev/vdX` device
   - **Filesystem volumes**: file contents are read by the shim and sent
     inline in the `CreateContainer` RPC. The agent writes them to the
     bundle directory and bind-mounts them at the expected paths.
4. Volume metadata is passed to the agent via the `CreateContainer` RPC
5. The agent injects the volumes as mounts in the adapted OCI spec

Read-only filesystem volumes (ConfigMaps, Secrets) are delivered inline via
the CreateContainer RPC — no separate disk, no virtiofsd.
Block PVCs and emptyDir volumes use separate hot-plugged disks. No FUSE, no
loopback mounts, no shared filesystem.

> **Limitation:** Writable filesystem PVCs are not currently supported.
> Writes to inline-delivered volumes do not persist back to the host. This
> requires a shared filesystem transport which is planned for a future release.

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
