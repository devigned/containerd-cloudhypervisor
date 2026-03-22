# Sandbox Daemon Design

## Status: Proposal

## Problem

Today, every pod start pays for VM boot + agent connect (~175ms with async
eager boot, 15s+ under 50-VM contention). The shim is ephemeral — spawned
per pod by containerd, destroyed when the pod dies. It cannot pre-warm VMs
because it doesn't exist before `RunPodSandbox`.

Async eager boot (v0.12.0) overlaps ~90ms of boot with containerd's
internal processing, but can never eliminate the cost entirely. Under
burst scale (150 pods), all VMs compete for host CPU during boot, inflating
per-pod latency to seconds.

## Proposal: Sandbox Daemon

A long-running systemd daemon on each node that:

1. **Pre-boots a pool of clean VMs** from a base snapshot (kernel + agent,
   no containers, no workload)
2. **Maintains warm workload snapshots** per container image
3. **Serves ready-to-use VMs to shims** via a Unix socket API

```
                    ┌─────────────────────────────────┐
                    │  cloudhv-sandbox-daemon          │
                    │  (systemd, long-running)         │
                    │                                  │
                    │  Base snapshot:                  │
                    │    kernel + agent (clean state)  │
                    │                                  │
                    │  Pool (restored from base):      │
                    │    VM-1: agent ready, clean ✓    │
                    │    VM-2: agent ready, clean ✓    │
                    │    VM-3: agent ready, clean ✓    │
                    │                                  │
                    │  Workload snapshots:             │
                    │    python-runtime: warm ✓        │
                    │    http-echo: warm ✓             │
                    │                                  │
                    │  /run/cloudhv/daemon.sock        │
                    └───────────┬─────────────────────┘
                                │
                    ┌───────────┴─────────────────────┐
                    │  containerd-shim-cloudhv-v1      │
                    │  (per-pod, ephemeral)             │
                    │                                  │
                    │  start_sandbox:                  │
                    │    TAP setup (0ms)               │
                    │    store TAP info                │
                    │    return (7ms)                  │
                    │                                  │
                    │  start_container:                │
                    │    daemon.AcquireSandbox()       │
                    │    receives pre-booted VM (<1ms) │
                    │    hot-plug rootfs (20ms)        │
                    │    RunContainer RPC (4ms)        │
                    │                                  │
                    │  delete:                         │
                    │    daemon.ReleaseSandbox()       │
                    │    VM destroyed, pool refilled   │
                    └─────────────────────────────────┘
```

## Architecture

### Daemon Responsibilities

The daemon owns:

- **CH process lifecycle** — spawn, boot, shutdown, destroy
- **Base snapshot** — created once at daemon startup from a clean VM boot
- **VM pool** — maintained by restoring from base snapshot (CoW memory)
- **Workload snapshots** — per-image warm snapshots (experimental)
- **Agent connections** — pre-connected, health-checked
- **Pool replenishment** — async restore after each acquire

The daemon does NOT own:

- **Networking** — TAP/tc setup stays in the shim (requires pod netns)
- **Container lifecycle** — RunContainer, logs, kill, wait stay in the shim
- **Rootfs conversion** — erofs cache stays in the shim
- **OCI spec handling** — the shim adapts specs and sends RPCs to the agent

### Shim Changes

The shim becomes a thin client for VM lifecycle:

```
start_sandbox:
  1. Set up TAP device in pod's network namespace
  2. Store TAP name, MAC, IP, gateway for later
  3. Return immediately (no VM interaction)

start_container (first container):
  1. Compute erofs image (cache hit or mkfs.erofs)
  2. Compute snapshot key from image identity
  3. Call daemon.AcquireSandbox(tap, mac, ip, gw, snapshot_key)
  4. Daemon returns: VM handle (api_socket, vsock_socket, cid, ch_pid, restored)
  5. Move CH process to pod cgroup
  6. If not restored: hot-plug rootfs disk, RunContainer RPC
  7. If restored: ConfigureNetwork RPC, skip RunContainer (workload alive)

delete:
  1. Kill container via agent RPC
  2. Call daemon.ReleaseSandbox(vm_id)
  3. Daemon destroys the VM
  4. Clean up TAP device
```

### Why AcquireSandbox in start_container, not start_sandbox

`start_sandbox` doesn't know the container image. The decision between
"warm workload restore" vs "clean pool VM" depends on the image identity,
which is only available in `start_container`. By deferring the acquire to
`start_container`, the daemon can make the optimal choice:

- **Workload snapshot exists** → restore from it (workload already running)
- **No snapshot** → hand out a clean pool VM (agent ready, hot-plug rootfs)

### Pool Management

```
Base VM Snapshot (created once at daemon startup):
  - Boot VM with guest rootfs only (/dev/vda)
  - Wait for agent to connect and respond to health check
  - Pause → snapshot → destroy
  - This is the "golden" clean state

Pool (maintained by daemon):
  - Target: pool_size VMs always ready (default: 3)
  - Each pool VM: restored from base snapshot (CoW, ~300ms)
  - VMs boot without networking (no TAP, no IP)
  - Agent already running and connected (from snapshot)

On AcquireSandbox:
  1. Pop a VM from the pool
  2. Configure its network (ConfigureNetwork RPC with pod IP)
  3. Return VM handle to shim
  4. Async: restore another VM from base snapshot to replenish pool

On ReleaseSandbox:
  1. Destroy the VM (shutdown + kill CH process)
  2. No recycling — clean VMs are cheap from snapshot

Pool sizing:
  - pool_size: target idle VMs (default: 3)
  - max_pool_size: hard cap on idle VMs (default: 10)
  - If pool is empty: synchronous restore from base snapshot (blocking)
  - replenish_delay_ms: wait before spawning replacement (default: 100)
  - vm_idle_timeout_secs: destroy idle VMs after timeout (default: 300)
```

### Warm Workload Snapshots (Experimental)

When `warm_restore` is enabled:

1. After a cold-boot container's workload is fully running (~20s warmup),
   the shim tells the daemon: `CreateWorkloadSnapshot(vm_id, image_key)`
2. Daemon pauses the VM, snapshots it (with workload running), resumes
3. On future `AcquireSandbox` with matching `image_key`:
   - Daemon restores from the workload snapshot instead of the base
   - VM wakes up with workload already serving
   - Shim skips RunContainer

Workload snapshots are stored separately from the base snapshot and are
per-image. They include the full workload state (Python runtime loaded,
server listening, etc.).

### Resource Accounting

#### Idle Pool VMs

Pool VMs live in the daemon's cgroup until acquired:

```yaml
# Daemon DaemonSet spec
resources:
  requests:
    memory: "2Gi"      # pool_size × default_memory_mb + daemon overhead
  limits:
    memory: "4Gi"      # headroom for replenishment
```

Kubelet sees pool VMs as daemon system overhead, not pod usage.

#### Acquired VMs

When a shim acquires a VM:

1. Shim receives the CH process PID from the daemon
2. Shim moves the CH PID to the pod's cgroup:
   `place_in_pod_cgroup(ch_pid, pod_cgroup_path)`
3. Kubelet now accounts the VM's memory/CPU to the pod
4. Daemon's cgroup usage drops by that VM's footprint

#### CoW Memory Sharing

All pool VMs share physical pages via CoW from the base snapshot.
The kernel's memory accounting correctly attributes:

- Shared pages: counted once (in the first VM's cgroup that faulted them)
- Private pages: counted per-VM (unique state after fork)

This is the same behavior as today's warm restore — no new accounting
challenges.

### Daemon API

```protobuf
service SandboxDaemon {
  // Acquire a pre-booted VM. The daemon selects the optimal source:
  // warm workload snapshot (if available) or clean pool VM.
  rpc AcquireSandbox(AcquireRequest) returns (AcquireResponse);

  // Release a VM back to the daemon for destruction.
  rpc ReleaseSandbox(ReleaseRequest) returns (ReleaseResponse);

  // Request a warm workload snapshot of a running VM.
  rpc CreateWorkloadSnapshot(SnapshotRequest) returns (SnapshotResponse);

  // Pool and snapshot status for monitoring.
  rpc Status(Empty) returns (StatusResponse);
}

message AcquireRequest {
  string tap_name = 1;
  string tap_mac = 2;
  string ip_cidr = 3;
  string gateway = 4;
  string netns = 5;
  string image_key = 6;     // erofs cache key for workload snapshot lookup
  uint32 vcpus = 7;         // 0 = use default
  uint64 memory_mb = 8;     // 0 = use default
}

message AcquireResponse {
  string vm_id = 1;
  string api_socket = 2;
  string vsock_socket = 3;
  uint64 cid = 4;
  uint32 ch_pid = 5;
  bool from_snapshot = 6;    // Was this a warm workload restore?
}

message ReleaseRequest {
  string vm_id = 1;
}

message ReleaseResponse {}

message SnapshotRequest {
  string vm_id = 1;
  string image_key = 2;
}

message SnapshotResponse {
  bool created = 1;
}

message StatusResponse {
  uint32 pool_ready = 1;
  uint32 pool_target = 2;
  uint32 active_vms = 3;
  repeated string snapshot_keys = 4;
}
```

### Daemon Configuration

```json
{
  "pool_size": 3,
  "max_pool_size": 10,
  "replenish_delay_ms": 100,
  "vm_idle_timeout_secs": 300,
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "warm_restore": false,
  "socket_path": "/run/cloudhv/daemon.sock"
}
```

### Installer Changes

The installer DaemonSet additionally:

1. Installs `cloudhv-sandbox-daemon` binary to `/usr/local/bin/`
2. Creates a systemd unit:
   ```ini
   [Unit]
   Description=CloudHV Sandbox Daemon
   After=containerd.service
   Requires=containerd.service

   [Service]
   ExecStart=/usr/local/bin/cloudhv-sandbox-daemon \
     --config /opt/cloudhv/daemon.json
   Restart=always
   MemoryMax=4G

   [Install]
   WantedBy=multi-user.target
   ```
3. The shim config adds: `"daemon_socket": "/run/cloudhv/daemon.sock"`
4. When `daemon_socket` is set, the shim uses the daemon API instead of
   spawning CH directly

### Expected Performance

| Scenario | v0.12.0 (no daemon) | With Daemon |
|----------|---------------------|-------------|
| Single pod (cold, cached erofs) | 120-200ms | **~110ms** |
| Single pod (warm restore) | 390-400ms | **~10ms** |
| 150-pod burst | 77s | **~20-30s** |
| Agent connect under contention | 15s+ | **0ms** |
| Pod-to-pod memory overhead | 57 MiB (cold) | **~5 MiB (CoW)** |

The biggest win is at scale: pre-booted VMs eliminate boot contention
entirely. Every pod gets an instantly-ready VM regardless of node load.

### Migration Path

The daemon is opt-in via the `daemon_socket` config field:

- **No daemon_socket**: shim behaves exactly as today (v0.12.0)
- **daemon_socket set**: shim delegates VM lifecycle to the daemon

This allows incremental rollout and easy fallback.

#### Phase 1: Daemon with base VM pool (no workload snapshots)

- New crate: `crates/daemon/`
- VM pool management (restore from base snapshot)
- AcquireSandbox / ReleaseSandbox RPCs
- Shim connects to daemon for VM acquisition
- Installer deploys daemon binary + systemd unit

#### Phase 2: Workload snapshots via daemon

- Move snapshot creation/restore logic from shim to daemon
- CreateWorkloadSnapshot RPC
- Image-key-based snapshot selection in AcquireSandbox
- Snapshot eviction policy (LRU, max cache size)

#### Phase 3: Observability and optimization

- Pool auto-sizing based on node load
- Prometheus metrics (pool size, acquire latency, restore latency)
- Predictive pre-warming based on workload patterns
- Snapshot pre-creation for known images (e.g. from DaemonSet config)

### Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| Daemon crash loses pool state | Systemd restarts daemon; pool rebuilds from base snapshot in seconds |
| Pool drain under burst | Fallback to synchronous restore (same latency as today) |
| Memory waste from idle pool | `vm_idle_timeout_secs` destroys unused VMs; DaemonSet `resources.limits` caps total |
| Stale base snapshot after upgrade | Daemon recreates base snapshot on startup; installer clears snapshot cache |
| Daemon socket unavailable | Shim falls back to direct CH spawn (v0.12.0 behavior) |

### Open Questions

1. **Pool sizing heuristics**: Static `pool_size` or dynamic based on
   recent pod creation rate?
2. **Multi-tenant isolation**: Should different RuntimeClasses get separate
   pools (e.g. different memory sizes)?
3. **Snapshot garbage collection**: LRU eviction? Max total cache size?
   Per-image TTL?
4. **Hot-standby for networking**: Can we pre-create TAP devices and
   pre-assign IPs to pool VMs for even faster acquire?
5. **Integration with Kubernetes scheduler**: Can we expose pool capacity
   as an extended resource so the scheduler avoids overcommitting nodes?
