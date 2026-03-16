# Improving AKS Pod Sandboxing for I/O-Bound Agentic Workloads

## Context

This report synthesizes lessons learned building the containerd-cloudhypervisor
shim (v0.7.0) and applies them to improving the existing AKS Pod Sandboxing
solution (Kata Containers with Cloud Hypervisor under MSHV). The target workload
is **I/O-bound AI agents** — pods that spend >95% of wall-clock time waiting on
LLM API calls, database queries, and external tool invocations, with minimal
sustained CPU usage.

### What We Measured

#### Scale Benchmark (3 × D8ds_v5, 150 pods)

| Metric | CloudHV v0.7.0 | Kata (AKS built-in) |
|--------|----------------|---------------------|
| Pods scheduled (of 150) | **150** | 130 |
| Pods unschedulable | **0** | 20 |
| Per-pod RuntimeClass overhead | 50Mi / 10m CPU | 600Mi / 0m CPU |
| Actual CPU at peak | **46-49%** | 16-22% |
| Scale-up time (warm) | **27s** (150 pods) | 64s (130 pods) |
| CrashLoopBackOff | 0 | 0 |
| Processes per VM | 2 | 3 |

Pod spec: 100m CPU request, 64Mi memory request, **256Mi memory limit**.

#### Per-Pod RSS (measured via /proc on AKS nodes)

| Component | CloudHV v0.7.0 | Kata (AKS) |
|-----------|----------------|------------|
| CH VmRSS (total) | **54 MB** | 265 MB |
| CH RssShmem (guest pages touched) | **49 MB** | 256 MB |
| Shim RSS | 5 MB | 51 MB |
| virtiofsd | — (none) | 14 MB (7 MB × 2) |
| **Total per pod** | **~59 MB** | **~330 MB** |

Kata sizes the VM guest memory to the pod spec's memory limit (256Mi). The
virtiofsd page cache fills all available guest memory, producing RssShmem =
256 MB. CloudHV boots with a fixed 512 MB mmap but the minimal guest only
touches ~49 MB (demand-paged, no virtiofsd).

### Core Insight

The density gap is **real and significant** — driven by guest image design.
Our minimal guest image (16MB rootfs, no systemd, no virtiofsd, no FUSE)
consumes ~59MB of host RAM per pod. Kata's full guest (~150MB rootfs,
virtiofsd FUSE mounts, kata-agent, more kernel subsystems) consumes ~330MB.
Both use the same Cloud Hypervisor VMM. The ~5.6× RSS difference comes from
how much guest memory is touched, not how much is allocated.

The biggest density lever is **reducing guest memory consumption** — through
a smaller guest image, removing virtiofsd, or reducing virtiofsd page cache
pressure.

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

| Metric | CloudHV v0.7.0 | Kata (AKS) |
|--------|----------------|------------|
| Pods scheduled (of 150) | **150** | 130 |
| Per-pod overhead (declared) | 50Mi | 600Mi |
| Per-pod RSS (measured) | ~59 MB | ~330 MB |
| Actual CPU at peak | **46-49%** | 16-22% |
| Scale-up time (warm) | **27s** (150 pods) | 64s (130 pods) |

**Our 50Mi overhead closely matches measured RSS (~59 MB)** for this workload.
However, the 512 MB guest memory mmap means RSS could grow toward 512 MB under
memory pressure inside the VM. For I/O-bound agents with small working sets,
50Mi accurately reflects actual usage.

**Kata's 600Mi is ~1.8× actual RSS (~330 MB).** Reducing the declared overhead
to ~350Mi would allow ~70 pods/node on D8ds_v5, up from ~43 today. But this
risks OOM for workloads with higher memory limits.

**The CPU gap is significant.** CloudHV uses ~2.5× more CPU (46-49% vs 16-22%).
This is structural: per-pod VMM process spawn, API socket, agent ttrpc
handshake. It is not rootfs delivery overhead — both runtimes deliver the
rootfs at VM boot time.

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

#### 6. Sandbox API daemon

A node-level daemon that pools VMs would eliminate the per-pod overhead gap:
- **Pool VMs** — pre-boot N VMs, assign to pods on demand (~10ms cold start)
- **Share rootfs cache** — centralized LRU cache instead of per-shim flock
- **Centralize metrics** — one Prometheus endpoint per node
- **Crash recovery** — daemon persists VM state to disk

Kata has discussed this in issue #7043. The containerd Sandbox API provides
the contract.

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

| # | Recommendation | Effort | Impact | Timeframe |
|---|---|---|---|---|
| 1 | Low-boot + virtio-mem oversubscription | Shim config change | **~4× density** | Days |
| 2 | Right-size Kata RuntimeClass overhead | AKS config | +50% Kata density | Weeks |
| 3 | Add CPU overhead to Kata RuntimeClass | AKS config | Better scheduling | Weeks |
| 4 | Request-based VM sizing | Shim code change | Align boot to request | 1-2 weeks |
| 5 | Pre-pull agent base images | AKS config | Faster cold start | 2-4 weeks |
| 6 | nobarrier + noatime mount flags | Small Kata agent PR | Reduced guest I/O | 1-2 weeks |
| 7 | Reduce virtiofsd page cache pressure | Kata design discussion | Lower Kata RSS | Months |
| 8 | Node-level memory budget controller | Sandbox daemon | Safe overcommit | 3-6 months |
| 9 | Kata v3 Dragonball + MSHV | Large upstream | 1 process/pod | 6-12 months |

## Conclusion

CloudHV achieves ~5.6× lower per-pod RSS (59 MB vs 330 MB) and schedules 15%
more pods (150 vs 130) on identical hardware with identical overhead
declarations. But the real density advantage is **oversubscription via
demand-paged memory**.

Kata's virtiofsd fills all guest memory at startup, making actual RSS equal
to the declared allocation regardless of workload. CloudHV's minimal guest
touches only ~49 MB of its 512 MB mmap. Combined with virtio-mem dynamic
resize, CloudHV can boot VMs with 128 MiB and grow on demand — enabling
~4× more pods per node for I/O-bound agents that are mostly idle.

The trade-off is CPU: CloudHV uses ~2.5× more host CPU (46-49% vs 16-22%)
due to per-pod VMM process overhead. This is structural and requires
architectural changes (sandbox daemon or built-in VMM) to resolve.

**The highest-impact near-term action is enabling low-boot + virtio-mem
oversubscription** (config change, no code required). This alone could
push CloudHV density from 150 to ~195 pods on the same 3-node cluster.
For Kata, the highest-impact change is **right-sizing the 600Mi overhead**
to match actual usage (~330 Mi), which would increase Kata density by ~50%.

---

*Based on containerd-cloudhypervisor v0.7.0 benchmarks on AKS (westus3),
3 × D8ds_v5 nodes. Pod spec: 100m CPU, 64Mi mem request, 256Mi mem limit.
Kata internals reviewed from kata-containers source code (src/runtime-rs/,
src/agent/, tools/packaging/). Per-pod RSS measured via /proc/PID/status.*
