# Architecture

## System Overview

```text
                    ┌─────────────────────────────────────────────────────────────┐
                    │  cloudhv-sandbox-daemon  (systemd, long-running)            │
                    │                                                             │
                    │  Base snapshot: kernel + agent (clean state)                │
                    │                                                             │
                    │  Pool (OnDemand restore from base, ~25ms each):            │
                    │    VM-1: agent ready, 6 MB idle RSS ✓                      │
                    │    VM-2: agent ready, 6 MB idle RSS ✓                      │
                    │    VM-3: agent ready, 6 MB idle RSS ✓                      │
                    │                                                             │
                    │  Warm workload snapshots (via shadow VMs):                  │
                    │    python-runtime: warm ✓                                   │
                    │    http-echo: warm ✓                                        │
                    │                                                             │
                    │  /run/cloudhv/daemon.sock                                   │
                    └───────────┬─────────────────────────────────────────────────┘
                                │ Unix socket API
                    ┌───────────┴─────────────────────────────────────────────────┐
                    │  containerd-shim-cloudhv-v1  (per-pod, ephemeral)           │
                    │                                                             │
  containerd        │  Pod Network Namespace                                      │
     │              │                                                             │
     │ ttrpc        │  ┌──────┐  TC redirect  ┌──────┐                           │
     │              │  │ veth ├──────────────►│ TAP  │                            │
     │              │  │(eth0)│◄──────────────┤      │                            │
     ▼              │  └───┬──┘               └──┬───┘                           │
  ┌──────────┐      │      │ IP flushed          │ virtio-net                     │
  │  shim    ├──────┤      │ (VM owns pod IP)    │                               │
  │  ~1300   │      │  ┌───┴─────────────────────┴────────────────────────────┐  │
  │  lines   │      │  │  cloud-hypervisor (VMM)                              │  │
  │          │      │  │  ┌─────────────────────────────────────────────┐     │  │
  │ • TAP    │      │  │  │  Guest VM (custom kernel)                   │     │  │
  │ • erofs  │      │  │  │                                             │     │  │
  │ • daemon │      │  │  │  eth0 ← ConfigureNetwork RPC                │     │  │
  │   RPCs   │      │  │  │                                             │     │  │
  └────┬─────┘      │  │  │  ┌───────────┐     ┌──────┐                │     │  │
       │ vsock      │  │  │  │   Agent   │────►│ crun │ (containers)   │     │  │
       │            │  │  │  │  (PID 1)  │     └──────┘                │     │  │
       └────────────┤  │  │  └───────────┘                             │     │  │
                    │  │  └─────────────────────────────────────────────┘     │  │
                    │  └──────────────────────────────────────────────────────┘  │
                    └───────────────────────────────────────────────────────────────┘
```

## Components

### Sandbox Daemon (`cloudhv-sandbox-daemon`)

Long-running systemd daemon on each node. Manages:

- **Base snapshot** — created once at startup from a cold-booted VM (kernel + agent, no containers). Reused across daemon restarts if kernel/rootfs mtime unchanged.
- **VM pool** — maintains `pool_size` (default: 3) pre-booted VMs restored from the base snapshot using CH v51 OnDemand restore (~25ms each, ~6 MB idle RSS via userfaultfd). When the pool is empty, synchronous restore serves as fallback.
- **Shadow VM snapshots** — on first `AcquireSandbox` for an unknown image, the daemon spawns a shadow VM in the background: restore from base → hot-plug rootfs → `RunContainer` → wait `warmup_duration_secs` → pause → snapshot → destroy. Subsequent acquires for the same image restore from the warm snapshot. Shadow VMs have no networking and no impact on live pods.
- **Image key** — content digest from containerd's gRPC image service, immune to tag mutation (e.g., `:latest` re-push).
- **Active VM tracking** — the daemon tracks all acquired VMs for proper lifecycle management. `ReleaseSandbox` destroys the VM; pool is replenished asynchronously.
- **CH process exit watching** — `/proc/{pid}` polling detects unexpected VMM exits.

The daemon serves VMs to shims via a Unix socket API at `/run/cloudhv/daemon.sock`.

### Shim (`containerd-shim-cloudhv-v1`)

Thin containerd shim v2 (~1,300 lines). When `daemon_socket` is configured, the shim delegates VM lifecycle to the daemon and handles only:

- **TAP networking** — creates TAP device in the pod's network namespace, sets up TC redirect rules, flushes the pod IP from the veth.
- **erofs conversion** — converts container rootfs to erofs images, cached at `/run/cloudhv/erofs-cache/<hash>.erofs`. Uses flock serialization with retry for concurrent builds.
- **Daemon RPCs** — `AcquireSandbox` (get a pre-booted VM), `AddContainer` (hot-plug disk + RunContainer), `ReleaseSandbox` (destroy VM).
- **Container lifecycle** — logs, kill, wait, state.

### Guest Agent (`cloudhv-agent`)

PID 1 in the VM. Discovers hot-plugged disks via inotify, adapts OCI specs, delegates to crun. Built as a separate workspace with its own ttrpc 0.9 dependency.

### Communication

- **vsock + ttrpc** — no network stack, no shared filesystem.
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

- **Sandbox** (`container-type=sandbox`): sets up TAP networking in the pod's
  network namespace. With the daemon, no VM is spawned yet — TAP info is stored
  for the subsequent `AcquireSandbox` call. Returns in ~7ms.
- **App container** (`container-type=container`): calls `daemon.AcquireSandbox()`
  with the stored TAP info and image key. The daemon returns a pre-booted VM
  (from pool or warm snapshot). The shim converts the rootfs to erofs (cached),
  hot-plugs it via `AddContainer`, and the guest agent discovers the disk via
  inotify and runs the container with crun.

### Boot Flow (Daemon Mode)

```
start_sandbox (7ms):
  TAP setup → store TAP info → return

  ─── containerd gap (~90ms) ───

start_container:
  erofs lookup (0ms cached / 8ms uncached)
  → daemon.AcquireSandbox (0ms pool hit / 25ms sync restore)
  → hot-plug disk + RunContainer RPC
  → total ~74ms cold path
```

For warm snapshot paths (~168ms): the daemon restores from a workload snapshot,
the shim calls `ConfigureNetwork` to assign the pod IP, and the workload wakes
up already running.

## Container Rootfs Delivery

The shim converts container rootfs to erofs images, cached at
`/run/cloudhv/erofs-cache/<hash>.erofs`. The cache key is an FNV-1a hash
of the image's content digest, resolved via `containerd-client` gRPC.
This makes the key content-addressed and immune to tag mutation.

Concurrent builds are serialized with `flock(LOCK_EX)` on a per-image
lock file. After acquiring the lock, the shim re-checks whether the
cache file exists (double-checked locking). If `mkfs.erofs` fails
(e.g. fork pressure under heavy pod creation load), it retries up to
3 times with progressive backoff (100ms, 200ms, 300ms). In practice,
only the very first pod per image per node runs `mkfs.erofs` (~8ms);
all subsequent pods hit the cache (0ms).

In both tiers, config.json and volume data (ConfigMaps, Secrets) are delivered
inline via the RunContainer RPC — the agent writes them to the bundle
directory. No second disk, no virtiofsd. Hot-plug and RunContainer happen
in the daemon when using daemon mode.

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
