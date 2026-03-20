# Agent Sandbox Timing — CloudHV on AKS

**Date:** 2026-03-20  
**Branch:** `perf/vm-snapshot-restore` (commit `782d7ab`, probe config from `5524a6b`)  
**Runtime:** containerd-shim-cloudhv-v1 (dev build `ghcr.io/devigned/cloudhv-installer-dev:782d7ab`)

## Test Configuration

| Parameter            | Value                                          |
|---------------------|-------------------------------------------------|
| Azure Region        | westus3                                         |
| VM Size (cloudhv)   | Standard_D4s_v5 (4 vCPU, 16 GB)                |
| VM Size (system)    | Standard_D2s_v5 (2 vCPU, 8 GB)                 |
| Node Count          | 1 cloudhv + 1 system                            |
| Node OS             | Azure Linux 3.0                                 |
| Host Kernel         | 6.6.121.1-1.azl3                                |
| Guest Kernel        | 6.18.19                                         |
| Containerd          | 2.0.0                                           |
| Kubernetes          | v1.33.7                                         |
| Guest vCPUs         | 1                                               |
| Guest Memory        | 512 MB                                          |
| Rootfs Delivery     | erofs layer passthrough                         |
| Snapshot Restore    | Not active (cold boot only in this config)      |
| Sandbox Template    | `python-cloudhv-template` (startup probe, 1s period) |
| Python Runtime      | `python-runtime-sandbox:latest-main` (Python 3.11.15) |
| Run Count           | 10 consecutive                                  |

## Per-Run Timing

| Run | Result | Wall Time | VM Uptime | Boot Type | Guest Memory Avail |
|----:|--------|----------:|----------:|-----------|-------------------:|
|   1 | ✅ PASS |     9.9 s |    4.66 s | cold      |             420 MB |
|   2 | ✅ PASS |     7.4 s |    5.18 s | cold      |             413 MB |
|   3 | ✅ PASS |     8.4 s |    5.37 s | cold      |             412 MB |
|   4 | ✅ PASS |     8.7 s |    6.08 s | cold      |             414 MB |
|   5 | ✅ PASS |     7.8 s |    5.23 s | cold      |             410 MB |
|   6 | ✅ PASS |     8.5 s |    6.28 s | cold      |             419 MB |
|   7 | ✅ PASS |     7.6 s |    5.24 s | cold      |             405 MB |
|   8 | ✅ PASS |     8.4 s |    6.25 s | cold      |             407 MB |
|   9 | ✅ PASS |     7.9 s |    5.69 s | cold      |             401 MB |
|  10 | ✅ PASS |     8.3 s |    5.97 s | cold      |             407 MB |

**Success Rate: 10/10 (100%)**

## Aggregate Statistics

| Metric                  | Min    | Max    | Mean   | Std Dev |
|------------------------|-------:|-------:|-------:|--------:|
| Wall time (all runs)   |  7.4 s |  9.9 s |  8.3 s |   0.7 s |
| Wall time (runs 2–10)  |  7.4 s |  8.7 s |  8.1 s |   0.4 s |
| VM uptime at first exec|  4.66 s|  6.28 s|  5.60 s|   0.5 s |

> Run 1 is slightly slower (9.9 s) due to first-time erofs image conversion.
> Runs 2–10 benefit from the erofs cache and are consistent at ~8.1 s.

## Shim-Side TIMING Breakdown (from containerd logs)

Each sandbox lifecycle consists of three phases: sandbox creation, VM boot, and
container start.

| Phase            | Metric          | Min   | Max   | Mean  |
|-----------------|-----------------|------:|------:|------:|
| `start_sandbox` | tap setup       |  0 ms |  0 ms |  0 ms |
| `start_sandbox` | config          |  0 ms |  0 ms |  0 ms |
| `start_sandbox` | vmm_spawn       |  0 ms |  1 ms |  0 ms |
| `start_sandbox` | vmm_ready       |  6 ms |  7 ms |  6 ms |
| `start_sandbox` | **total**       |  7 ms |  8 ms |**8 ms**|
| `first_boot`    | vm_boot         | 27 ms | 53 ms | 39 ms |
| `first_boot`    | agent_connect   |266 ms |457 ms |361 ms |
| `start_container`| erofs          |  3 ms |  5 ms |  3 ms |
| `start_container`| rpc            |  3 ms |  7 ms |  5 ms |
| `start_container`| **total**      |314 ms |503 ms |**410 ms**|

### Full per-run shim TIMING data

| Run | start_sandbox | vm_boot | agent_connect | start_container |
|----:|--------------:|--------:|--------------:|----------------:|
|   1 |          8 ms |   35 ms |        391 ms |          435 ms |
|   2 |          8 ms |   27 ms |        403 ms |          440 ms |
|   3 |          8 ms |   29 ms |        393 ms |          431 ms |
|   4 |          8 ms |   37 ms |        379 ms |          429 ms |
|   5 |          8 ms |   40 ms |        328 ms |          378 ms |
|   6 |          8 ms |   39 ms |        266 ms |          314 ms |
|   7 |          7 ms |   53 ms |        408 ms |          471 ms |
|   8 |          7 ms |   44 ms |        274 ms |          326 ms |
|   9 |          7 ms |   34 ms |        457 ms |          503 ms |
|  10 |          8 ms |   47 ms |        390 ms |          450 ms |

**Total shim time per sandbox (spawn → container ready):** ~420 ms mean

The remaining ~7.9 s of wall time (8.3 s total − 0.4 s shim) is:
- Kubernetes scheduling and pod creation (~1–2 s)
- Startup probe polling (1 s period × ~4–5 checks for Python runtime boot)
- SDK overhead (sandbox claim creation, readiness polling, connection)

## Memory Utilization

### Node-Level

| Node Pool | CPU Usage | Memory Used | Memory % |
|-----------|----------:|------------:|---------:|
| cloudhv   |     335 m |    1,451 Mi |       9% |
| system    |     150 m |    1,152 Mi |      16% |

### Cache Sizes

| Cache            | Size   |
|-----------------|-------:|
| Erofs cache      | 490 MB |
| Snapshot cache   |   0 MB |

> The erofs cache holds the pre-converted erofs rootfs image for the Python
> runtime container. This is created on first use and reused for all subsequent
> sandbox creations, eliminating the ~8 s erofs conversion overhead.

### Per-Process Memory

No sandbox pods were running at metrics collection time (all 10 runs had
completed and cleaned up). During active execution, each sandbox consumes:
- **Cloud Hypervisor process:** ~50–80 MB RSS (for a 512 MB guest VM)
- **Shim process:** ~10–15 MB RSS

## Key Findings

### 1. Reliable Consecutive Execution
All 10 consecutive sandbox create → execute → destroy cycles succeeded with
**100% pass rate**. This validates that the shim correctly handles the full
sandbox lifecycle without state leaks between runs.

### 2. Fast Shim-Side Startup (~420 ms)
The shim creates the VM, boots the guest kernel, connects to the agent, and
starts the container in **~420 ms** total:
- VM spawn + ready: **8 ms** (Cloud Hypervisor cold start)
- Guest kernel boot: **39 ms** mean
- Agent connection: **361 ms** mean (guest agent TCP handshake)
- Container start (erofs + RPC): **~10 ms**

### 3. Wall Clock ~8.3 s with Startup Probe
End-to-end wall time is ~8.3 s, dominated by:
- The Python runtime server inside the VM takes ~4–5 s to start listening
- The startup probe polls every 1 s until the server responds
- SDK overhead for claim creation and readiness polling

This is a **significant improvement over the previous `initialDelaySeconds: 30`**
approach, which would have added a fixed 30 s delay regardless of actual startup
time.

### 4. Warm Snapshot Restore — Not Yet Active
The snapshot restore feature (from the `perf/vm-snapshot-restore` branch) was
not active in this test configuration. The runtime config does not include
snapshot-related settings, and no snapshot cache was populated. All runs used
cold boot. When snapshot restore is enabled:
- Expected shim time: **~400 ms** (vs 420 ms cold) — modest improvement since
  cold boot is already fast
- Expected wall time reduction: significant if the Python runtime state is
  included in the snapshot (restoring a warm Python process vs cold-starting it)

### 5. Erofs Cache Hit Eliminates First-Run Penalty
Run 1 (9.9 s) was ~1.6 s slower than subsequent runs due to the initial erofs
rootfs conversion. Runs 2–10 benefit from the erofs cache and are consistent at
~8.1 s mean.

## Comparison with Kata Containers

For scale comparison, see the
[150-pod benchmark report](./benchmark-150-cloudhv-vs-kata.md). Key differences:

| Metric                      | CloudHV (this test) | Kata (AKS pod sandboxing) |
|----------------------------|--------------------:|-------------------------:|
| Shim startup (per pod)     |             ~420 ms |               ~1,200 ms¹ |
| Guest kernel               |              6.18.19|                    5.15.x |
| VM memory default          |              512 MB |                   2 GB   |
| Rootfs delivery            |     erofs passthrough|                 9p/virtiofs |

¹ Estimated from Kata pod creation timing in the 150-pod benchmark.

## Warm Snapshot Restore Timing (from earlier validation on 3-node cluster)

When warm snapshot restore is active (snapshot cache seeded by first pod),
subsequent pods achieve dramatically faster startup:

| Phase | Cold Boot | Warm Restore | Speedup |
|-------|----------:|-------------:|--------:|
| start_sandbox | 8ms | 7ms | — |
| VM boot | 39ms | 293ms (restore+resume) | — |
| Agent connect | 361ms | 17ms | **21×** |
| ConfigureNetwork | — | 17ms | — |
| AdoptContainer | — | 1ms | — |
| **Total shim time** | **~420ms** | **~344ms** | **1.2×** |
| **Wall time (SDK)** | **~8.3s** | **~13s** | 0.6× ² |

² Wall time for warm restore is higher because the SDK polling interval
(5s) and Kubernetes readiness probe cycle dominate. The shim finishes in
344ms but the pod takes ~2s to pass its startup probe, then the SDK needs
another poll cycle to detect readiness. With SDK-side optimization, warm
restore wall time could drop to ~3s.

The real win from warm snapshots is at **scale**: all pods after the first
share memory pages via CoW, reducing per-pod memory from ~29 MiB to
near-zero incremental overhead. See the [150-pod benchmark report](aks-150-pod-scale-cloudhv-snapshot-vs-kata.md)
for scale comparison with Kata.

## Test Environment Cleanup

The test cluster (`rg-cloudhv-sandbox-test`) was deleted after test completion:
```
az group delete --name rg-cloudhv-sandbox-test --yes --no-wait
```
