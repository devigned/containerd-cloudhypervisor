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

### Cloud Hypervisor Version Requirement

The daemon requires **Cloud Hypervisor >= v51.0** for the `OnDemand`
memory restore mode ([PR #7800](https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7800)).
This enables userfaultfd-based lazy memory population during snapshot
restore, which reduces restore time from **277ms to 18ms** (15× faster)
and idle VM memory from **517 MB to 6 MB** per pool VM.

Without `OnDemand` mode (CH v44 and earlier), restore copies the entire
512 MB guest memory file into RAM synchronously. With `OnDemand`, memory
pages are loaded on demand via userfaultfd — only pages actually accessed
by the guest are read from the snapshot file.

| Metric | CH v44 (eager) | CH v51+ (OnDemand) |
|--------|---------------:|-------------------:|
| vm.restore | 277ms | **18ms** |
| Idle pool VM RSS | 517 MB | **6 MB** |
| Minor page faults | 131,512 | 546 |
| Pool of 10 VMs memory | ~5.2 GB | **~60 MB** |

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

### Warm Workload Snapshots via Shadow VMs

Today (v0.11.0), warm snapshots are taken by pausing a **live production
VM** — the pod that's serving actual traffic. This causes:

- ~200-500ms of request downtime while paused
- Risk of TCP connection resets (peers time out)
- Kubelet may restart the container if liveness probe fires during pause

The daemon eliminates this by using **shadow VMs** — dedicated throwaway
instances that exist solely to produce warm snapshots, outside of
Kubernetes entirely.

#### Shadow VM Flow

```
First AcquireSandbox(image_key="python-runtime"):
  1. No workload snapshot exists for this image
  2. Daemon returns a clean pool VM → shim starts container normally
  3. Pod serves traffic immediately — never paused, never disrupted

Meanwhile (async, background):
  4. Daemon spawns a shadow VM:
     a. Restore from base snapshot (clean VM, agent ready)
     b. Hot-plug the container rootfs (same erofs image)
     c. RunContainer RPC (workload starts inside shadow)
     d. Wait warmup_duration (configurable, default 30s)
     e. Pause shadow VM → snapshot → destroy shadow
     f. Cache workload snapshot under image_key
  5. Shadow VM lifecycle: ~35s total, fully async

Next AcquireSandbox(image_key="python-runtime"):
  6. Workload snapshot exists → restore from it
  7. VM wakes up with Python already running, server listening
  8. ConfigureNetwork RPC (new IP) → traffic flows immediately
```

#### Shadow VM Properties

| Property | Value |
|----------|-------|
| Managed by containerd? | No — daemon-internal only |
| In a pod cgroup? | No — lives in daemon's cgroup |
| Has networking? | No — no TAP, no IP, no CNI |
| Serves traffic? | No — purely for snapshot creation |
| Lifetime | ~35s (boot + warmup + snapshot + destroy) |
| Impact on live pods | Zero — completely isolated |

#### Benefits Over Live-Pause Snapshots

- **Zero production disruption** — live pods are never paused
- **Longer warmup** — can wait 60s+ for full JIT/model loading since no
  user traffic is affected
- **Safer** — snapshot failure doesn't impact any running workload
- **Simpler error handling** — if shadow VM crashes, just retry later
- **Configurable warmup** — per-image warmup duration in daemon config

#### Shadow VM Networking

Shadow VMs boot without networking because:

1. The workload binds `0.0.0.0:<port>` — it listens on all interfaces
2. No interface exists during warmup, but the bind still succeeds on `0.0.0.0`
3. On restore, the shim configures the real network via ConfigureNetwork RPC
4. The kernel sees the new interface, workload accepts on it automatically

If a workload requires a network interface during startup (e.g. connects
to an external service on init), the daemon can create a dummy interface
with a non-routable IP for the shadow VM's warmup period.

#### Snapshot Cache Management

```json
{
  "snapshot_cache": {
    "max_total_size_gb": 10,
    "max_snapshots": 20,
    "eviction_policy": "lru",
    "warmup_duration_secs": 30,
    "per_image_overrides": {
      "python-runtime-sandbox": {
        "warmup_duration_secs": 60
      }
    }
  }
}
```

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

message StatusResponse {
  uint32 pool_ready = 1;
  uint32 pool_target = 2;
  uint32 active_vms = 3;
  uint32 shadow_vms_running = 4;   // Shadow VMs currently warming up
  repeated string snapshot_keys = 5;
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
2. Installs **Cloud Hypervisor >= v51.0** (built from source or from a
   future release). The current installer downloads CH v44 from GitHub
   releases. For the daemon, the installer must either:
   - Download a pre-built CH v51+ binary from the project's own GHCR
     (built in CI from CH main), or
   - Build CH from source during the installer image build (adds ~2 min
     to image build but guarantees version alignment)

   The recommended approach: **build CH from source in the installer
   Dockerfile** and pin to a specific CH commit SHA:
   ```dockerfile
   ARG CH_COMMIT=fc79d08  # CH main with OnDemand restore
   RUN git clone --depth 1 https://github.com/cloud-hypervisor/cloud-hypervisor.git /tmp/ch && \
       cd /tmp/ch && git fetch origin ${CH_COMMIT} && git checkout ${CH_COMMIT} && \
       cargo build --release && \
       cp target/release/cloud-hypervisor /opt/cloud-hypervisor && \
       rm -rf /tmp/ch
   ```

   Once CH cuts a release containing PR #7800, switch to downloading the
   release binary as before.

3. Creates a systemd unit:
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
4. Writes `/opt/cloudhv/daemon.json` with pool configuration
5. The shim config adds: `"daemon_socket": "/run/cloudhv/daemon.sock"`
6. When `daemon_socket` is set, the shim uses the daemon API instead of
   spawning CH directly

### Expected Performance

With CH v51+ OnDemand restore:

| Scenario | v0.12.0 (no daemon) | With Daemon |
|----------|---------------------|-------------|
| Single pod (pool hit) | 120-200ms | **<5ms acquire + ~20ms hot-plug** |
| Single pod (pool empty) | 120-200ms | **~50ms** (was 296ms with eager) |
| Single pod (warm restore) | 390-400ms | **~10ms** |
| 150-pod burst | 77s | **~10-20s** |
| Agent connect under contention | 15s+ | **0ms** |
| Pool VM memory overhead | N/A | **~6 MB per idle VM** |
| Pool of 10 idle VMs | N/A | **~60 MB total** |

The biggest win is at scale: pre-booted VMs eliminate boot contention
entirely. Every pod gets an instantly-ready VM regardless of node load.

### Implementation Plan

The daemon is built and validated **independently** of the shim. The shim
is not modified until the daemon's performance and reliability are proven
via its own test harness.

#### Stage 1: Daemon Core (no shim changes)

Build the daemon as a standalone binary with its ttrpc API. Test and
benchmark it using a dedicated test client that simulates what the shim
would do.

**1a. Scaffold**
- New crate: `crates/daemon/` with `main.rs`, `pool.rs`, `api.rs`, `config.rs`
- Proto: `proto/daemon.proto` with `AcquireSandbox`, `ReleaseSandbox`, `Status`
- Daemon binary: `cloudhv-sandbox-daemon`
- Config loading from JSON file
- Systemd unit template

**1b. Base snapshot**
- On startup: cold-boot a VM, wait for agent health check, pause → snapshot → destroy
- Cache base snapshot at `/run/cloudhv/daemon/base-snapshot/`
- On subsequent startups: reuse if kernel + rootfs mtime unchanged

**1c. VM pool**
- Restore `pool_size` VMs from base snapshot (CoW)
- Each VM: CH process + vsock socket + agent connected
- Pool stored as `Vec<PoolVm>` behind a `Mutex`
- Replenish async after each acquire
- Idle timeout destroys excess VMs

**1d. AcquireSandbox / ReleaseSandbox**
- AcquireSandbox: pop from pool, call ConfigureNetwork on the VM (with
  caller-provided IP/TAP info), return VM handle
- If pool empty: synchronous restore from base snapshot
- ReleaseSandbox: shutdown + destroy CH process

**1e. Test client**
- Standalone binary or integration test in `crates/daemon/tests/`
- Connects to daemon socket, calls AcquireSandbox with mock TAP/IP
- Hot-plugs a real rootfs disk, runs a container via agent RPC
- Verifies workload runs, then calls ReleaseSandbox
- Measures: acquire latency, container-ready latency, release latency
- Runs lifecycle N times, reports p50/p95/p99

**1f. Benchmarks**
- Single-pod latency: acquire → hot-plug → RunContainer → workload ready
- Pool drain: N concurrent acquires, measure tail latency
- Pool replenish: time from release to pool_ready restored
- Memory: per-VM RSS, total pool RSS, CoW sharing ratio
- Compare with direct CH spawn (v0.12.0 shim behavior)
- Run on hl-dev (D8s_v5, KVM) for consistent comparison

**Milestone**: daemon passes all Stage 1 tests, acquire latency <5ms from
warm pool, pool replenish <400ms, 100 consecutive lifecycle passes.

#### Stage 2: Shadow VM Snapshots (no shim changes)

Add warm workload snapshots via shadow VMs to the daemon. Test via the
same test client.

**2a. Shadow VM lifecycle**
- On first AcquireSandbox for an unknown `image_key`:
  - Return pool VM immediately
  - Spawn shadow VM in background (restore from base → hot-plug rootfs →
    RunContainer → wait warmup → pause → snapshot → destroy)
- Cache workload snapshot under `image_key`

**2b. Snapshot restore in AcquireSandbox**
- If workload snapshot exists for `image_key`: restore from it instead of pool
- Set `from_snapshot = true` in response
- Caller skips RunContainer (workload already alive)

**2c. Snapshot management**
- LRU eviction when cache exceeds `max_snapshots`
- Invalidation when rootfs mtime changes
- Per-image warmup duration config

**2d. Test client extensions**
- Acquire with `image_key` → verify cold path (pool VM)
- Wait for shadow snapshot → acquire again → verify `from_snapshot = true`
- Workload responds to HTTP immediately after warm restore
- Shadow failure → verify no impact, retry works
- Eviction: fill cache, verify oldest evicted

**2e. Benchmarks**
- Warm acquire latency: target <10ms
- Shadow VM total time: boot + warmup + snapshot + destroy
- Cold vs warm workload readiness (measure HTTP response time)
- Memory sharing: 50 warm-restored VMs, total RSS vs naive

**Milestone**: warm workload snapshot restore <10ms, shadow VMs produce
correct snapshots for Python/Node/Go workloads, 100 consecutive warm
lifecycle passes.

#### Stage 3: Shim Integration

Only after Stage 1 and 2 milestones are met. Minimal shim changes:

**3a. Daemon client in shim**
- Add `daemon_socket` field to `RuntimeConfig`
- New module: `crates/shim/src/daemon_client.rs`
- ttrpc client that connects to daemon socket

**3b. start_sandbox changes**
- If `daemon_socket` is set: skip VMM spawn, store TAP info only
- If not set: existing behavior (eager boot)

**3c. start_container changes**
- If daemon mode: call `AcquireSandbox(tap, mac, ip, gw, image_key)`
- Move CH PID to pod cgroup
- If `from_snapshot`: ConfigureNetwork, skip RunContainer
- If not: hot-plug rootfs, RunContainer
- If not daemon mode: existing behavior

**3d. delete changes**
- If daemon mode: call `ReleaseSandbox(vm_id)`, clean TAP
- If not: existing behavior

**3e. Fallback**
- If daemon socket unavailable: log warning, fall back to direct spawn
- Shim works with or without daemon — daemon is purely additive

**3f. Integration tests**
- Full CRI flow via crictl with daemon running
- Compare timing with daemon vs without
- Scale test on AKS with daemon DaemonSet

**Milestone**: shim + daemon end-to-end passing all existing tests (crictl
lifecycle, AKS agent sandbox, 150-pod scale), with improved latency.

#### Stage 4: AKS Benchmark and Production Hardening

**4a. Installer changes**
- Install daemon binary + systemd unit + daemon config
- DaemonSet resource requests cover pool memory
- Cache cleanup on installer upgrade

**4b. AKS benchmark**
- 150-pod scale: CloudHV + daemon vs Kata (identical infra)
- Agent sandbox timing: 10 consecutive runs
- Contention: 50 concurrent pod starts
- Node memory at peak
- Generate report for `reports/`

**4c. Production hardening**
- Prometheus metrics endpoint on daemon
- Pool auto-sizing based on recent acquire rate
- Graceful shutdown (drain pool on SIGTERM)
- Log correlation between daemon and shim (shared request ID)

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

---

## Addendum: Test Plan

The daemon's ttrpc API provides a clean boundary for testing. A test client
can drive the full lifecycle without containerd, Kubernetes, or CRI — just
connect to the Unix socket and call RPCs.

### Unit Tests

Testable in isolation with mocked CH processes:

| Test | Validates |
|------|-----------|
| Pool initializes to `pool_size` VMs | Base snapshot creation + restore loop |
| AcquireSandbox returns a VM with valid api_socket, vsock_socket | Response fields populated correctly |
| AcquireSandbox decrements pool count | Pool bookkeeping |
| Pool replenishes after acquire (async) | Replenishment triggers and completes |
| AcquireSandbox blocks when pool is empty | Synchronous fallback restore |
| ReleaseSandbox destroys VM (CH process exits) | Cleanup path |
| ReleaseSandbox with invalid vm_id returns error | Error handling |
| Status RPC reports correct pool_ready / active_vms | Monitoring accuracy |
| Daemon startup with no existing base snapshot | Creates base snapshot from cold boot |
| Daemon startup with existing base snapshot | Reuses cached base snapshot |
| VM idle timeout destroys unused pool VMs | Resource reclaim |
| Pool respects max_pool_size cap | No unbounded growth |
| Config reload updates pool_size | Dynamic reconfiguration |

### Integration Tests (Daemon + CH + Agent)

These run against real Cloud Hypervisor with KVM. Require a host with
`/dev/kvm` access (hl-dev or CI runner with nested virt).

#### Lifecycle Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **Acquire and use** | AcquireSandbox → hot-plug disk → RunContainer → verify workload runs → ReleaseSandbox | Full lifecycle |
| **Consecutive acquire/release** | Repeat acquire→release 10 times | No state leaks, pool replenishment |
| **Concurrent acquire** | 10 parallel AcquireSandbox calls | Pool drain + synchronous fallback |
| **Acquire with custom resources** | AcquireSandbox(vcpus=2, memory=1024) | Custom VM sizing |
| **Release during acquire** | AcquireSandbox (blocks, pool empty) → cancel | Graceful cancellation |
| **Daemon restart** | AcquireSandbox → daemon restarts → ReleaseSandbox | Orphan VM cleanup |
| **Daemon crash recovery** | Kill daemon process → restart → Status | Pool rebuilds from base snapshot |

#### Shadow VM Snapshot Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **Shadow snapshot creation** | AcquireSandbox(image_key=X, no snapshot) → wait → verify snapshot exists | Shadow VM lifecycle |
| **Snapshot restore** | Create snapshot → AcquireSandbox(same image_key) → verify from_snapshot=true | Workload snapshot hit |
| **Shadow VM failure** | Shadow VM crashes during warmup → verify no snapshot cached, no impact on live VMs | Fault isolation |
| **Shadow VM networking** | Verify shadow VM has no TAP, workload still binds 0.0.0.0 | Networkless warmup |
| **Snapshot eviction** | Fill cache to max_snapshots → verify LRU eviction | Cache management |
| **Concurrent shadow VMs** | Multiple images trigger shadows simultaneously | No resource conflicts |
| **Per-image warmup duration** | Configure 60s for image A, 10s for image B → verify timing | Warmup configuration |

#### Resource Accounting Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **Cgroup migration** | AcquireSandbox → verify CH PID in daemon cgroup → shim moves to pod cgroup → verify | Accounting handoff |
| **Pool memory accounting** | Start daemon → verify pool VMs count against daemon cgroup | System overhead tracking |
| **CoW memory sharing** | Acquire 10 VMs → measure total RSS vs 10 × per-VM RSS | CoW deduplication |
| **Daemon resource limits** | Set MemoryMax=2G → fill pool → verify OOM kills daemon not host | Resource containment |

#### Network Configuration Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **ConfigureNetwork on acquired VM** | AcquireSandbox → daemon calls ConfigureNetwork → verify guest has IP | Post-acquire networking |
| **IP conflict detection** | Acquire two VMs with same IP → verify error or correct isolation | Safety |
| **Network cleanup on release** | AcquireSandbox → configure → ReleaseSandbox → verify TAP cleaned | No network leaks |

### Performance Tests

Run on identical infrastructure to enable comparison with previous
benchmarks and Kata Containers.

#### Single-Pod Latency (hl-dev, D8s_v5, KVM)

Measure wall-clock time from AcquireSandbox call to workload serving HTTP:

| Scenario | Metric | Target | Comparison |
|----------|--------|--------|------------|
| Pool hit (base VM) | AcquireSandbox latency | <5ms | v0.12.0: 7ms sandbox + 26-110ms container |
| Pool hit + hot-plug + RPC | Container ready | <25ms | v0.12.0: 26-110ms |
| Pool empty (sync restore) | AcquireSandbox latency | <350ms | v0.12.0: 120-200ms total |
| Workload snapshot hit | Container ready (warm) | <10ms | v0.12.0 warm: 300ms |
| Pool replenish time | Time to refill one slot | <400ms | Background, non-blocking |

Run each scenario 20 times to reduce noise. Report p50, p95, p99.

#### Scale Benchmark (AKS, 3 × D8ds_v5)

Compare with v0.12.0 and Kata on identical infrastructure:

| Test | CloudHV v0.12.0 | CloudHV + Daemon | Kata |
|------|-----------------|------------------|------|
| 150 pods → all Ready | 77s | Target: <30s | 130/150 (stuck) |
| Scale-down 150 → 0 | 12s | Target: <10s | ~30s |
| Per-pod memory (cold) | 57 MiB | ~5 MiB (CoW from base) | 312 MiB |
| Per-pod memory (warm) | 5 MiB | ~5 MiB (CoW from workload) | N/A |
| Node memory at 150 pods | 4.2 GiB | Target: <3 GiB | 40.5 GiB |

Protocol:
1. Deploy daemon with `pool_size: 10` on each node
2. Wait for pools to fill (Status RPC shows pool_ready = pool_size)
3. Scale deployment from 0 → 150
4. Record time to all pods Ready (readiness probe with startup probe, 1s period)
5. Capture node memory via `free -m` (VMSS run-command)
6. Scale 150 → 0, record time to all pods terminated
7. Repeat 3 times, report median

#### Agent Sandbox Benchmark (AKS, Python workload)

Compare Python sandbox startup with and without daemon:

| Test | Without Daemon | With Daemon (cold) | With Daemon (warm) |
|------|---------------|-------------------|-------------------|
| SDK create → sandbox ready | ~13s | Target: <5s | Target: <3s |
| Consecutive runs (10x) | 10/10 pass | 10/10 pass | 10/10 pass |
| VM uptime at first exec | 20s (cold boot) | ~0.5s (pool) | ~50s (snapshot clock) |

#### Contention Benchmark

The primary value of the daemon — pre-booted pools eliminate boot contention:

| Concurrent pods | v0.12.0 agent_connect | With Daemon |
|----------------|----------------------|-------------|
| 1 | 170ms | <5ms |
| 10 | ~500ms | <5ms |
| 50 | ~5s | <5ms (pool) / ~350ms (drain) |
| 150 | ~15s | <5ms (pool) / ~350ms (drain) |

Protocol:
1. Pre-fill pool to 50 VMs
2. Launch N pods simultaneously (kubectl scale)
3. Measure per-pod agent_connect time from TIMING logs
4. Compare with Kata's per-pod boot time at same concurrency

### Edge Cases

| Edge Case | Expected Behavior |
|-----------|-------------------|
| Daemon not running when shim starts | Shim falls back to direct CH spawn (v0.12.0 behavior) |
| Daemon socket exists but daemon is dead | Shim detects connection failure, falls back |
| Pool drains completely during burst | Synchronous restore (same latency as v0.12.0 warm restore) |
| Base snapshot corrupted | Daemon recreates from cold boot on startup |
| Workload snapshot restore fails | Daemon falls back to pool VM (cold boot path) |
| CH binary upgraded while daemon running | Daemon restart required (systemd handles) |
| Guest rootfs upgraded while pool VMs exist | Daemon invalidates pool and base snapshot on rootfs mtime change |
| Shadow VM OOM during warmup | Shadow destroyed, snapshot not cached, retry on next acquire |
| Node memory pressure | Daemon reduces pool_size via idle timeout; kubelet can evict daemon pod |
| Daemon and shim version mismatch | ttrpc API versioned; daemon rejects incompatible clients |
| Multiple images trigger shadow VMs simultaneously | Daemon caps concurrent shadows (e.g. max 2) to avoid resource exhaustion |
| AcquireSandbox with vcpus/memory that don't match pool VMs | Daemon bypasses pool, creates custom VM synchronously |
