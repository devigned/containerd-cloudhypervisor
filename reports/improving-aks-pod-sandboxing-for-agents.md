# Improving AKS Pod Sandboxing for I/O-Bound Agentic Workloads

## Context

This report synthesizes lessons learned building the containerd-cloudhypervisor
shim (v0.7.0 through the current sandbox daemon architecture) and applies them
to improving the existing AKS Pod Sandboxing solution (Kata Containers with
Cloud Hypervisor under MSHV). The target workload is **I/O-bound AI agents** —
pods that spend >95% of wall-clock time waiting on LLM API calls, database
queries, and external tool invocations, with minimal sustained CPU usage.

> **Update (2026-07-15):** The sandbox daemon (recommendation #6) is now
> implemented and tested. See the daemon benchmark results below alongside
> the original v0.7.0 data.

### What We Measured

#### Scale Benchmark (3 × D8ds_v5, 150 pods)

| Metric | CloudHV Daemon | CloudHV v0.7.0 | Kata (AKS built-in) |
|--------|---------------|----------------|---------------------|
| Pods scheduled (of 150) | **150** | **150** | 130 |
| Pods unschedulable | **0** | **0** | 20 |
| Scale-up time | **11s** | 27s | 64s (130 pods) |
| Per-pod RSS | **~36 MB** | ~59 MB | ~330 MB |
| Node memory at peak | **~3.2 GiB (10%)** | ~5.5 GiB | ~13.5 GiB |
| Architecture | Daemon + VM pool | Direct CH spawn | External CH + virtiofsd |
| CrashLoopBackOff | 0 | 0 | 0 |
| Processes per VM | 1 daemon + shim | 2 (CH + shim) | 3 (CH + shim + virtiofsd) |

Pod spec: 100m CPU request, 64Mi memory request, **256Mi memory limit**.
Daemon benchmark: commit `57b7b4d`, Cloud Hypervisor v51 with userfaultfd
`OnDemand` restore.

#### Per-Pod RSS (measured via /proc on AKS nodes)

| Component | CloudHV Daemon | CloudHV v0.7.0 | Kata (AKS) |
|-----------|---------------|----------------|------------|
| VM RSS | **~24 MB** | 54 MB | 265 MB |
| Shim RSS | **~6 MB × 2** | 5 MB | 51 MB |
| Daemon RSS | **~2.2 MB** (per node) | — | — |
| virtiofsd | — | — | 14 MB (7 MB × 2) |
| **Total per pod** | **~36 MB** | **~59 MB** | **~330 MB** |

The daemon's userfaultfd `OnDemand` restore (CH v51) keeps only faulted pages
resident, reducing per-VM RSS from ~54 MB (v0.7.0 cold boot) to ~24 MB.
Kata sizes the VM guest memory to the pod spec's memory limit (256Mi). The
virtiofsd page cache fills all available guest memory, producing RssShmem =
256 MB.

### Core Insight

The density gap is **real and significant** — driven by guest image design
and restore strategy. With the sandbox daemon, our minimal guest image
(16MB rootfs, no systemd, no virtiofsd, no FUSE) consumes **~36 MB** of host
RAM per pod — a **9× density advantage** over Kata's ~330 MB. The daemon's
userfaultfd `OnDemand` restore further reduces RSS by keeping only faulted
pages resident (~24 MB per VM vs ~54 MB with cold boot in v0.7.0).

The biggest density levers are **reducing guest memory consumption** (smaller
guest image, no virtiofsd) and **smarter restore strategies** (userfaultfd
OnDemand, VM pooling).

### How Kata Actually Works on AKS (from node inspection)

We deployed a Kata pod on AKS and inspected the node directly. Findings:

- **Rootfs delivery: virtiofsd, not block passthrough.** AKS configures Kata
  with `disable_block_device_use = true` and `shared_fs = "virtio-fs"`. The
  container rootfs is shared from the host's overlayfs mount into the guest
  via FUSE/virtiofsd. No block device passthrough.

- **ConfigMaps/Secrets:** Also delivered via virtiofsd with inotify-based
  live watching (500ms debounce). More capable than our inline RPC approach
  (supports live ConfigMap updates) but requires virtiofsd processes.

- **Processes per pod (measured):**

  | Process | RSS |
  |---------|-----|
  | Cloud Hypervisor VMM | **265 MB** (includes 256MB guest memory mmap) |
  | Kata shim (containerd-shim-kata-v2) | **51 MB** |
  | virtiofsd (2 instances) | **7 MB × 2 = 14 MB** |
  | **Total per pod** | **~330 MB** |

- **Cgroup placement:** `sandbox_cgroup_only=true` — the CH process, shim,
  and virtiofsd are all in the pod's cgroup. `kubectl top` sees the full
  VM resource usage. **The CPU comparison in our benchmarks is fair.**

- **600Mi RuntimeClass overhead is conservative.** Measured per-pod RSS is
  ~330 MB. The 600Mi declaration is ~1.8× actual usage for this workload.
  This limits Kata to ~43 pods/node when actual memory would support more,
  but provides safety margin for memory-intensive workloads that fill more
  guest memory.

### What This Means

| Metric | CloudHV Daemon | CloudHV v0.7.0 | Kata (AKS) |
|--------|---------------|----------------|------------|
| Pods scheduled (of 150) | **150** | **150** | 130 |
| Per-pod RSS (measured) | **~36 MB** | ~59 MB | ~330 MB |
| Node memory at 150 pods | **~3.2 GiB (10%)** | ~5.5 GiB | ~13.5 GiB |
| Scale-up time | **11s** | 27s | 64s (130 pods) |
| Memory density vs Kata | **9×** | 5.6× | — |

**The daemon architecture resolves the CPU overhead concern.** In v0.7.0,
CloudHV used ~2.5× more CPU (46-49% vs 16-22%) due to per-pod VMM process
spawn. The daemon eliminates this by pre-pooling VMs — the shim no longer
spawns a CH process per pod but instead acquires a pre-booted VM from the
daemon pool (25 ms restore time).

**Per-pod RSS dropped from ~59 MB to ~36 MB** with the daemon's userfaultfd
`OnDemand` restore. VM RSS is ~24 MB (only faulted pages resident) vs ~54 MB
with cold boot. Node memory at 150 pods is just 3.2 GiB (10% utilization),
leaving 90% headroom.

**Kata's 600Mi is ~1.8× actual RSS (~330 MB).** Reducing the declared overhead
to ~350Mi would allow ~70 pods/node on D8ds_v5, up from ~43 today. But this
risks OOM for workloads with higher memory limits.

## Recommendations

### Short Term — Configuration Changes

#### 1. Right-size Kata RuntimeClass overhead

The 600Mi declaration limits scheduling to ~43 pods/node when actual RSS is
~330 MB. For I/O-bound agent workloads with known memory limits, a lower
overhead (300-350Mi) would significantly increase density. This should be
configurable per-RuntimeClass rather than a global default, since memory-
intensive workloads need the higher value.

#### 2. Add CPU overhead to the RuntimeClass

Kata's AKS RuntimeClass declares zero CPU overhead. Kata's own Helm chart
recommends **250m**. Adding even 10-50m gives the scheduler better information
and enables HPA to factor in VMM cost.

#### 3. Pre-pull agent base images on kata node pools

Image pull dominates cold start for real AI agent workloads. Python + LangChain
images are 500MB-2GB. AKS could pre-warm popular agent base images on kata node
pools at creation time.

### Medium Term — Kata Runtime Contributions

#### 4. Contribute nobarrier + noatime mount flags for ephemeral mounts

Kata's agent uses `relatime` but **not** `nobarrier` or `noatime` (confirmed
from `src/agent/src/mount.rs`). For ephemeral container mounts, adding
`MS_NOATIME` + `nobarrier` would reduce I/O overhead. We measured a 2×
improvement (22ms vs 44ms) in our agent with this optimization.

#### 5. Explore reducing virtiofsd page cache pressure

Kata's virtiofsd fills all available guest memory via page cache. The virtiofsd
processes themselves are small (~7 MB each), but the page cache they generate
is the dominant contributor to guest memory consumption (256 MB RssShmem in our
benchmark). Options:
- virtiofsd cache policy tuning (e.g., `cache=none` or `cache=auto`)
- Block device passthrough as alternative to virtiofsd for rootfs
- Inline volume delivery for ConfigMaps/Secrets (as we do) to eliminate one
  virtiofsd instance

### Longer Term — Architectural Changes

#### 6. Sandbox API daemon ✅ IMPLEMENTED

> **Status: Implemented and benchmarked** (commit `57b7b4d`, 2026-07-15)

The sandbox daemon is now operational. A standalone node-level daemon pre-boots
VM pools from base snapshots using Cloud Hypervisor v51's userfaultfd `OnDemand`
restore:

- **Pool VMs** — pre-booted from base snapshots, assigned to pods on demand
  (25 ms restore time per VM)
- **Shadow VMs** — create warm workload snapshots in the background
- **Thin shim** — ~1,300 lines handling TAP networking, erofs conversion, and
  daemon RPCs (replaced the full VMM-spawning shim)
- **Image key** — uses content digest from containerd's gRPC image service

**Measured results (150-pod scale):**

| Metric | Before (v0.11.0) | After (Daemon) |
|--------|-----------------|----------------|
| Scale-up time | 77s | **11s** |
| Per-VM RSS | ~529 MB (99% shared) | **~24 MB** |
| Per-pod total RSS | ~5 MB unique | **~36 MB** |
| Daemon RSS | — | **~2.2 MB/node** |
| Pool restore time | — | **25 ms** |
| Shim inner (cold) | ~344 ms | **74 ms** |
| Shim inner (warm) | ~344 ms | **168 ms** |

The daemon eliminates the per-pod VMM process overhead that drove the CPU gap
(46-49% in v0.7.0 vs 16-22% for Kata). Pre-pooled VMs remove cold-boot
bottlenecks entirely.

#### 7. Kata v3 + Dragonball for AKS

Kata v3's built-in Dragonball VMM (single process per pod, upcall-based
hot-plug) would reduce per-pod process count from 3 to 1 and eliminate IPC
overhead. The main gap is MSHV support — Dragonball is KVM-only today.

## Deep Dive: Memory Oversubscription and Node OOM Strategies

### The Fundamental Problem

Kubernetes schedules pods based on **declared requests**, not actual usage.
For VM-isolated runtimes, this creates a tension:

| | CloudHV | Kata |
|---|---------|------|
| Pod memory request | 64Mi | 64Mi |
| RuntimeClass overhead | 50Mi | 600Mi |
| **Scheduler sees** | **114Mi** | **664Mi** |
| Actual RSS at startup | ~59 MB | ~330 MB |
| Actual RSS under load | 59–512 MB (demand-paged) | 330 MB (VM fills limit) |

Kata's VM is sized to the pod's memory limit (256Mi in our benchmark), and
virtiofsd page cache fills all available guest memory at startup. The guest
immediately consumes its full allocation. CloudHV's VM is allocated 512 MB
via mmap, but demand paging (prefault=off) means only touched pages cost host
RAM. At startup, only ~49 MB of guest pages are touched.

This creates a **structural oversubscription opportunity** that only works
with demand-paged VMMs like CloudHV's approach.

### How CloudHV Enables Memory Oversubscription

CloudHV's memory model has three properties that enable safe oversubscription:

**1. Demand-paged guest memory (prefault=off)**

Cloud Hypervisor's `mmap` for guest memory uses `MAP_PRIVATE | MAP_ANONYMOUS`
without `MAP_POPULATE`. The kernel doesn't allocate physical pages until the
guest touches them. A 512 MB VM allocation costs ~49 MB of host RSS for our
minimal guest. This is not configurable — it's the default CH behavior and
it's always on.

This means the **scheduler can safely overcommit** based on the knowledge that
idle agents won't consume their full memory allocation. For I/O-bound agents
that spend >95% of time waiting on API calls, actual memory usage stays near
the ~59 MB floor.

**2. Virtio-mem dynamic resize**

CloudHV supports `virtio-mem` hot-plug, which allows the shim to grow or
shrink guest memory at runtime. Our memory monitor (`crates/shim/src/memory.rs`)
already implements this:

- **Growth**: When guest MemAvailable < 20%, grow in 128 MiB steps
- **Reclaim**: When guest MemAvailable > 50% for 60s, shrink in 128 MiB steps
- **PSI-aware**: Reacts immediately to memory pressure signals from the guest
- **Bounds**: Never below boot memory (floor), never above boot + hotplug (ceiling)

With this, a pod can boot with minimal memory (e.g., 128 MB) and grow to its
limit (e.g., 2 GiB) only when the workload actually needs it — and shrink
back when it doesn't.

**3. Balloon device with free page reporting**

When virtio-mem is not active, CloudHV can use a balloon device with
`free_page_reporting: true`. The host kernel reclaims pages that the guest
marks as free, reducing host RSS even within a fixed guest memory allocation.

### Why Kata Cannot Oversubscribe

Kata's virtiofsd architecture prevents the same oversubscription because:

1. **Virtiofsd fills all guest memory.** The FUSE page cache aggressively
   caches filesystem data, consuming all available guest memory within seconds
   of boot. A 256 MB VM has 256 MB RssShmem — there is no "idle floor."

2. **VM sized to pod limit.** Kata sizes the VM to `limits.memory`, not
   `requests.memory`. A pod with `requests: 64Mi, limits: 256Mi` gets a
   256 MB VM that is fully consumed by page cache at startup.

3. **No dynamic resize.** Kata does not use virtio-mem to grow/shrink VMs.
   The memory allocation is fixed at boot.

The result: Kata's host RSS per pod equals the pod's memory limit + overhead,
regardless of actual workload memory usage. Oversubscription based on
"workloads usually don't use their limits" doesn't apply because the VM
infrastructure itself fills the allocation.

### Oversubscription Strategies for CloudHV

#### Strategy 1: Low-boot + virtio-mem growth (recommended for agents)

Boot VMs with minimal memory and grow on demand:

```
config.json:
  default_memory_mb: 128      # Boot floor — 128 MiB
  hotplug_memory_mb: 896      # Growth ceiling — total 1024 MiB
  hotplug_method: "virtio-mem"

RuntimeClass overhead: 100Mi  # 128 MiB boot + shim overhead
Pod spec:
  requests.memory: 64Mi       # Workload request
  limits.memory: 1Gi          # Workload limit
```

**At startup:** VM boots with 128 MiB, guest touches ~49 MB, host RSS ≈ 55 MB.
**Under load:** Memory monitor detects pressure, grows to 256 → 384 → ... → 1024 MiB.
**After spike:** Monitor detects idle (MemAvailable > 50% for 60s), shrinks back.

**Scheduler capacity:** With 100Mi overhead, the scheduler sees 164Mi per pod.
On a 32 GiB node: ~195 pods schedulable (vs ~48 with 664Mi Kata overhead).
Actual host memory at 195 pods idle: 195 × 55 MB ≈ 10.7 GiB (33% of 32 GiB).

The overcommit ratio is ~3× (195 pods × 1 GiB limit = 195 GiB on 32 GiB node),
but actual usage is ~11 GiB because most agents are idle.

#### Strategy 2: Request-based boot with limit ceiling

Size the VM to the pod's request (not limit), using virtio-mem for growth:

```
VM boot memory = pod.requests.memory (64 MiB)
VM max memory  = pod.limits.memory (1 GiB)
Growth via virtio-mem as needed
```

This aligns the VM's initial allocation with what the scheduler already
accounts for. The pod can burst to its limit via virtio-mem, but the host
only pays for actual usage. This would require the shim to read the pod's
resource spec and configure the VM accordingly.

#### Strategy 3: Node-level memory budget with admission control

Implement a node-level memory budget controller that:
1. Tracks aggregate actual RSS (not declared requests) across all VMs
2. Admits new pods only if the node has headroom for their boot RSS (~55 MB)
3. Monitors aggregate RSS and triggers virtio-mem reclaim when approaching
   node capacity
4. Evicts lowest-priority pods if aggregate RSS exceeds a safety threshold

This is the most sophisticated approach and requires either:
- A DaemonSet that watches `/proc` and communicates with the scheduler
- A sandbox daemon (recommendation #6) that manages the memory budget
  directly

### Handling Node OOM

Even with oversubscription, OOM is possible if many pods simultaneously
consume their full memory limits. Defenses (in order of preference):

**1. Guest OOM first (default behavior)**

The Linux OOM killer inside the guest fires before host OOM because the guest
sees its virtio-mem allocation as the total available memory. If a container
exceeds its cgroup memory limit inside the VM, the guest kills it. The host
RSS drops immediately as the killed process's pages are freed. This is the
natural containment boundary — the VM is a memory isolation boundary.

**2. Virtio-mem reclaim under host pressure**

The memory monitor can be extended to monitor **host** memory pressure (e.g.,
`/proc/pressure/memory` on the host) and aggressively reclaim guest memory
across all VMs when the host is under pressure. This distributes the shrinkage
proportionally:
- VMs with high MemAvailable are reclaimed first
- VMs under active load are reclaimed last
- This requires the sandbox daemon architecture (shared state across VMs)

**3. Balloon free page reporting**

With `free_page_reporting: true`, the balloon device automatically reports
guest free pages to the host. The host kernel reclaims these pages without
any explicit resize operation. This provides passive oversubscription — the
host reclaims pages the guest isn't using, without the guest being aware.

**4. Pod eviction via kubelet**

Kubelet monitors node memory pressure and evicts pods based on priority and
resource usage. For CloudHV pods, kubelet sees the cgroup memory usage (which
includes the CH process's RSS). Pods whose VMs are consuming more memory than
declared are evicted first. This is the last-resort defense and works the same
as for non-VM pods.

### What This Means for Density

| Scenario | CloudHV (128 MiB boot) | Kata (256 MiB boot) |
|----------|----------------------|---------------------|
| Overhead declared | 100Mi | 600Mi |
| Scheduler sees per pod | 164Mi | 664Mi |
| Max schedulable (32 GiB node) | ~195 | ~48 |
| Actual RSS at idle | ~55 MB | ~330 MB |
| Aggregate idle RSS (max pods) | ~10.7 GiB | ~15.8 GiB |
| Node memory headroom at idle | ~21 GiB (66%) | ~16 GiB (50%) |
| Overcommit ratio (limit/RSS) | ~3× | ~1× |

CloudHV with low-boot + virtio-mem can safely schedule **~4× more pods** than
Kata on the same hardware, with **more headroom** for burst memory usage. The
key enabler is demand-paged guest memory — idle agents genuinely don't consume
host RAM, and the memory monitor ensures growth when they need it.

This advantage is specific to **I/O-bound workloads** where agents are mostly
idle. For compute-heavy workloads that consistently use their memory limits,
the overcommit ratio shrinks toward 1× and the advantage disappears.

## Priority Matrix

| # | Recommendation | Effort | Impact | Status |
|---|---|---|---|---|
| 1 | Low-boot + virtio-mem oversubscription | Shim config change | **~4× density** | Available |
| 2 | Right-size Kata RuntimeClass overhead | AKS config | +50% Kata density | Open |
| 3 | Add CPU overhead to Kata RuntimeClass | AKS config | Better scheduling | Open |
| 4 | Request-based VM sizing | Shim code change | Align boot to request | Available |
| 5 | Pre-pull agent base images | AKS config | Faster cold start | Open |
| 6 | nobarrier + noatime mount flags | Small Kata agent PR | Reduced guest I/O | Open |
| 7 | Reduce virtiofsd page cache pressure | Kata design discussion | Lower Kata RSS | Open |
| 8 | **Sandbox daemon** | **Implemented** | **7× faster, 9× density** | **✅ Done** |
| 9 | Kata v3 Dragonball + MSHV | Large upstream | 1 process/pod | Open |

## Current Status

The sandbox daemon (recommendation #8) is **implemented and benchmarked** as
of commit `57b7b4d`. Key results on the same 3 × D8ds_v5 infrastructure:

| Metric | v0.7.0 | v0.11.0 | Daemon | Kata |
|--------|--------|---------|--------|------|
| 150-pod scale-up | 27s | 77s | **11s** | 64s (130 only) |
| Per-pod RSS | 59 MB | ~5 MB unique | **36 MB** | 330 MB |
| Node memory (150 pods) | ~5.5 GiB | ~13.8 GiB | **~3.2 GiB** | ~13.5 GiB |
| CPU overhead concern | 2.5× Kata | 2.5× Kata | **Resolved** | Baseline |

The daemon architecture eliminated the CPU overhead gap by replacing per-pod
VMM process spawn with pre-pooled VMs acquired via daemon RPC. The shim is
now ~1,300 lines handling only TAP networking, erofs conversion, and daemon
communication.

## Conclusion

CloudHV with the sandbox daemon achieves **9× lower per-pod RSS** (36 MB vs
330 MB) and **150/150 pods in 11 seconds** — a 7× improvement over v0.11.0
and 2.5× over v0.7.0 on identical hardware.

The daemon architecture resolved the CPU overhead concern that was the primary
trade-off in v0.7.0 (46-49% vs Kata's 16-22%). Pre-pooled VMs eliminate
per-pod VMM process spawn entirely. userfaultfd `OnDemand` restore (CH v51)
keeps per-VM RSS at ~24 MB, and node memory utilization at 150 pods is just
10% (3.2 GiB of 32 GiB), leaving 90% headroom.

The real density advantage remains **oversubscription via demand-paged memory**.
Kata's virtiofsd fills all guest memory at startup, making actual RSS equal
to the declared allocation regardless of workload. CloudHV's minimal guest
with userfaultfd restore touches only ~24 MB. Combined with virtio-mem dynamic
resize, CloudHV can boot VMs with 128 MiB and grow on demand — enabling
~4× more pods per node for I/O-bound agents that are mostly idle.

For Kata, the highest-impact change remains **right-sizing the 600Mi overhead**
to match actual usage (~330 Mi), which would increase Kata density by ~50%.

---

*Based on containerd-cloudhypervisor v0.7.0 through daemon (commit `57b7b4d`)
benchmarks on AKS (westus3), 3 × D8ds_v5 nodes. Pod spec: 100m CPU, 64Mi mem
request, 256Mi mem limit. Daemon benchmark: Cloud Hypervisor v51 with
userfaultfd OnDemand restore. Kata internals reviewed from kata-containers
source code (src/runtime-rs/, src/agent/, tools/packaging/). Per-pod RSS
measured via /proc/PID/status.*
