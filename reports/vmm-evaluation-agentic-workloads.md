# VMM Evaluation for Agentic Workloads

## Goal

Run AI agent workloads in VM-isolated sandboxes with:
- **Minimum memory overhead** — maximize pod density per node
- **Minimum startup latency** — agents must respond to events in <1s
- **Multi-container pods** — sidecar patterns (agent + tools + proxy)
- **Kubernetes-native** — kubectl top, HPA, RuntimeClass, CRI

This report evaluates six approaches: our current Cloud Hypervisor shim,
Kata v3 with Dragonball (built-in VMM), Kata v2 (external CH), libkrun,
krun (crun-krun), and OpenVMM.

## Candidates

### 1. Cloud Hypervisor (current — containerd-cloudhypervisor, sandbox daemon)

Rust-based VMM from Intel/Microsoft. Our current architecture uses a
**standalone sandbox daemon** that pre-boots VM pools from base snapshots
using CH v51's userfaultfd `OnDemand` restore. The shim is a thin daemon
client (~1,300 lines) handling only TAP networking, erofs conversion, and
daemon RPCs.

| Attribute | Value |
|-----------|-------|
| Language | Rust (rust-vmm crates) |
| Process model | 1 daemon/node + 1 thin shim/pod |
| VM RSS | **~24 MB** (userfaultfd OnDemand) |
| Per-pod total RSS | **~36 MB** (VM + 2 shims) |
| Daemon RSS | ~2.2 MB per node |
| Per-pod overhead (RuntimeClass) | 10m CPU, 50Mi memory |
| Pool restore time | **25 ms** per VM |
| Shim inner (cold) | **74 ms** (erofs 8ms + daemon acquire 63ms) |
| Shim inner (warm) | **168 ms** (acquire 24ms + agent + net config) |
| Scale-up (150 pods) | **11 seconds** (commit `57b7b4d`) |
| Disk hot-plug | ✅ vm.add-disk HTTP API |
| Memory hot-plug | ✅ virtio-mem + balloon |
| Network | ✅ TAP + virtio-net |
| vsock | ✅ |
| KVM | ✅ |
| MSHV | ✅ |
| Multi-container | ✅ via disk hot-plug |
| Confidential VMs | ✅ SEV, TDX |
| CH version | v51 (userfaultfd OnDemand restore, PR #7800) |

**Strengths:** Production-proven on Azure, MSHV support, full hot-plug, mature
HTTP management API. The daemon architecture eliminates per-pod VMM process
spawn — VMs are pre-pooled and acquired via RPC. Combined with our minimal
guest image (16MB rootfs, no virtiofsd, no systemd), achieves **~36 MB actual
host RSS per pod** — **9× lower than Kata's ~330 MB** on identical hardware.
userfaultfd `OnDemand` restore keeps only faulted pages resident (~24 MB per
VM). Shadow VMs create warm workload snapshots in the background for fast
workload-specific restore.

**Previous weakness resolved:** The CPU overhead concern from v0.7.0 (46-49%
CPU at 150-pod scale, ~2.5× higher than Kata) was driven by per-pod VMM
process spawn. The daemon architecture eliminates this by pre-booting VM pools
— the shim acquires a pre-booted VM in 25 ms instead of spawning and booting
a new CH process per pod.

**Remaining trade-offs:** The daemon is an additional node-level component to
deploy and monitor. Image key computation requires containerd gRPC access.
Shadow VM snapshot creation adds background CPU load (amortized, not per-pod).

### 2. Kata v3 / runtime-rs with Dragonball (built-in VMM)

This is the most architecturally significant candidate. Kata v3 introduced
`runtime-rs` — a Rust rewrite of the Kata runtime — with **Dragonball** as
an optional **built-in VMM**. Instead of forking a separate VMM process, the
runtime links Dragonball as a Rust library. The VMM runs in-process with the
shim.

| Attribute | Value |
|-----------|-------|
| Language | Rust (runtime-rs + Dragonball) |
| Process model | **1 process per pod** (shim = VMM) |
| VMM RSS overhead | ~5 MB (shared with shim) |
| Boot time | Sub-200ms (no process spawn) |
| Disk hot-plug | ✅ via **upcall** (vsock-based, no ACPI) |
| Memory hot-plug | ✅ via upcall |
| CPU hot-plug | ✅ via upcall |
| Network | ✅ VETH, TAP, TC filter, MacVlan, IPVlan |
| vsock | ✅ |
| KVM | ✅ |
| MSHV | ❌ (not currently supported) |
| Multi-container | ✅ (via upcall device hot-plug) |
| Confidential VMs | ⚠️ Limited (no ACPI = challenges for TDX) |
| Production status | ✅ Used by Alibaba Cloud, Ant Group |

**Key innovation: upcall.** Dragonball replaces ACPI-based device hot-plug with
a direct vsock communication channel between the VMM and a guest kernel driver.
The VMM (client) sends hot-plug requests via vsock to the upcall driver (server)
in the guest kernel. This is:
- **Faster** than ACPI — no firmware emulation, no udevd dependency
- **Deterministic** — direct request/response, no async ACPI notification
- **Lighter** — no ACPI table emulation, no PCI bus emulation

The upcall approach eliminates the ACPI/PCI overhead that makes traditional VMM
hot-plug expensive, and it works for virtio-mmio devices — the same virtio-blk
disks we hot-plug for container rootfs.

**This is essentially what libkrun would be if it had device hot-plug and was
integrated into a Kubernetes-native runtime.** Kata v3 + Dragonball already
delivers the "VMM as library" model with full multi-container pod support.

**Weaknesses:**
- No MSHV support (Dragonball is KVM-only) — blocks Azure/AKS
- Requires custom guest kernel patches for the upcall driver
- Not yet used in AKS pod sandboxing (AKS uses Kata v2 with external CH)
- Still carries Kata's heavier guest rootfs (~150MB vs our 16MB)
- Async runtime overhead from tokio (though reduced from Go version)

### 3. Kata v2 (AKS Pod Sandboxing — external Cloud Hypervisor)

The current AKS production path. Go-based kata-runtime spawns Cloud Hypervisor
as an external process. This is what we benchmarked against.

| Attribute | Value |
|-----------|-------|
| Process model | 3 per pod (shim + CH + virtiofsd) |
| Per-pod overhead (AKS) | 600Mi declared (actual ~330 MB RSS) |
| Per-pod RSS breakdown | CH: 265 MB, shim: 51 MB, virtiofsd: 7 MB × 2 |
| Rootfs delivery | virtiofsd (FUSE), not block passthrough |
| Boot time | 500ms-1s |
| Multi-container | ✅ |
| MSHV | ✅ |
| Production | ✅ (AKS native) |

**Key finding:** AKS Kata uses `disable_block_device_use = true` and
`shared_fs = "virtio-fs"`. The container rootfs is shared via virtiofsd/FUSE,
not block device passthrough. Virtiofsd page cache pressure causes the guest
to touch all available memory — with a pod spec `limits.memory: 256Mi`, Kata
sizes the VM to 256 MB and RssShmem = 256 MB. The 600Mi RuntimeClass overhead
is conservative at ~1.8× actual RSS for this workload.

**Strengths:** AKS-native, battle-tested, Microsoft-supported.

**Weaknesses:** Heaviest of all options — 600Mi overhead, 3 processes, slowest
boot. Our benchmarks showed it caps at 130/150 pods where we ran 150/150 on
identical hardware.

### 4. libkrun

Rust VMM as a shared library from Red Hat. The caller links against it — no
separate process.

| Attribute | Value |
|-----------|-------|
| Process model | In-process (library) — 1 per pod |
| VMM RSS overhead | ~5 MB |
| Disk hot-plug | ❌ Pre-boot only |
| MSHV | ❌ |
| Multi-container | ❌ (no runtime device add) |
| Confidential VMs | ✅ SEV, SEV-SNP, TDX, Nitro |

**Compared to Kata v3 + Dragonball:** libkrun is the same concept (VMM as
library, single process) but without device hot-plug, without Kubernetes
integration, and without the production track record. Dragonball solves the
hot-plug gap with upcall. libkrun would need equivalent work to be competitive.

**Strengths:** Clean C API, confidential computing variants, Red Hat backing.

**Weaknesses:** No hot-plug = no multi-container pods. No MSHV. Kata v3
already delivers what libkrun promises, with more features.

### 5. krun (crun-krun)

OCI runtime using libkrun. Replaces runc, not a containerd shim.

| Attribute | Value |
|-----------|-------|
| Process model | 1 VM per container (not per pod) |
| Multi-container pods | ❌ Each container = separate VM |
| Kubernetes CRI | No custom shim, limited integration |

**Not suitable for our use case.** One VM per container breaks the Kubernetes
pod model — containers can't share localhost networking. No sandbox concept.

### 6. OpenVMM (Microsoft)

Rust VMM from Microsoft, primarily the OpenHCL paravisor for Azure.

| Attribute | Value |
|-----------|-------|
| KVM | ✅ |
| MSHV | ✅ |
| Disk hot-plug | ⚠️ Infrastructure present, not stable |
| Production ready | ❌ Explicitly not recommended |

**Interesting for MSHV-native development** but too early for production.
Worth monitoring — if it matures, it could be the ideal VMM for Azure
deployments. Same Rust + MSHV foundation as the Azure infrastructure itself.

## Comparison Matrix

| | Our CH (Daemon) | Kata v3 Dragonball | Kata v2 (AKS) | libkrun | krun | OpenVMM |
|---|---|---|---|---|---|---|
| **Processes/pod** | 1 daemon + shim | **1** | 3 | **1** | 1 | 2 |
| **VM RSS** | **~24 MB** | ~5 MB* | 25-50 MB | ~5 MB | ~5 MB | ? |
| **Per-pod host RSS** | **~36 MB** | ~50-100MB* | **~330 MB** | ~50MB* | N/A | ? |
| **Pod overhead** | **50Mi** | ~50-100Mi* | 600Mi | ~30Mi* | N/A | ? |
| **Shim inner time** | **74–168ms** | <200ms | 500-1000ms | <50ms* | N/A | ? |
| **Pool restore** | **25ms** | N/A | N/A | N/A | N/A | N/A |
| **Disk hot-plug** | ✅ | ✅ (upcall) | ✅ | ❌ | ❌ | ⚠️ |
| **Multi-container** | ✅ | ✅ | ✅ | ❌ | ❌ | ⚠️ |
| **MSHV (Azure)** | ✅ | ❌ | ✅ | ❌ | ❌ | ✅ |
| **KVM** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Production** | ✅ | ✅ (Alibaba) | ✅ (AKS) | ✅ (crun) | ✅ | ❌ |
| **VM pooling** | **✅** | ❌ | ❌ | ❌ | ❌ | ❌ |
| **Confidential** | ✅ | ⚠️ | ✅ | ✅ | ✅ | ✅ |
| **150-pod test** | **150/150 (11s)** | Untested | 130/150 | Untested | N/A | Untested |

*Projected, not measured

## Analysis: Kata v3 Dragonball and the Daemon Architecture

Our original VMM evaluation identified the "VMM as library" approach (Kata v3
Dragonball) as the most promising path. Since then, we've implemented an
alternative approach — the **sandbox daemon** — that achieves similar benefits
through a different mechanism:

| Property | Kata v3 Dragonball | Our Daemon |
|----------|-------------------|------------|
| Process per pod | 1 (shim = VMM) | 1 daemon/node + thin shim/pod |
| VMM spawn overhead | None (in-process) | None (pre-pooled) |
| Hot-plug | upcall (vsock) | Standard CH HTTP API |
| VM restore | — | 25 ms (userfaultfd OnDemand) |
| MSHV | ❌ | ✅ |
| Guest kernel | Custom (upcall patches) | Standard |

The daemon approach achieved the primary goal — eliminating per-pod VMM process
overhead — without requiring a built-in VMM or custom kernel patches. Pre-pooled
VMs with userfaultfd `OnDemand` restore deliver 25 ms acquisition time and
~24 MB per-VM RSS.

### What Dragonball Gets Right

1. **Single process per pod** — the shim *is* the VMM. Same benefit as libkrun,
   but with production deployment history.

2. **Device hot-plug via upcall** — vsock-based, no ACPI, no PCI bus emulation.
   This is a cleaner solution than CH's ACPI hot-plug or libkrun's "no
   hot-plug at all." It's faster and more deterministic.

3. **Rust ecosystem** — built on rust-vmm crates (same foundation as CH and
   libkrun). The code is composable.

4. **Extensible framework** — Kata v3's architecture separates service, runtime
   handler, and resource manager. New hypervisors are pluggable. New resource
   types (rootfs cache, inline volumes) can be added without rewriting core.

5. **Async I/O** — tokio-based async runtime reduces thread count from
   `4 + 12*M` (sync) to `2 + N` (async) for M containers and N tokio workers.

### What's Missing for Our Use Case

1. **MSHV support** — Dragonball is KVM-only. This is the single blocker for
   Azure/AKS adoption. Adding MSHV to Dragonball (or contributing an MSHV
   backend to runtime-rs) would unlock the entire AKS ecosystem.

2. **Rootfs image caching** — Kata v3 creates fresh rootfs images per container,
   same as Kata v2. Our caching optimization (359× faster burst) could be
   contributed to runtime-rs's resource manager.

3. **Inline metadata delivery** — Kata still stages volumes via virtiofsd or
   bakes into disk images. Our inline RPC approach (zero disk I/O for metadata)
   fits cleanly into Kata's existing ttRPC agent protocol.

4. **Guest kernel patches** — upcall requires custom kernel patches (ported to
   5.10). This is a deployment complexity our CH-based approach avoids (standard
   ACPI hot-plug, works with any modern kernel).

5. **AKS integration** — Not yet used in AKS pod sandboxing. Microsoft would
   need to validate and certify runtime-rs + Dragonball for their managed
   offering.

## Recommended Path Forward

### Option A: Contribute to Kata v3 (Recommended)

Rather than maintaining a separate shim, contribute our optimizations to Kata v3
runtime-rs and work toward MSHV support for Dragonball:

1. **Contribute rootfs caching** to runtime-rs resource manager — our proven
   flock + atomic rename + cache-hit cp pattern
2. **Contribute inline metadata** to Kata's agent protocol — config.json +
   volume files sent in CreateContainer RPC
3. **Contribute nobarrier ext4** to Kata's agent mount logic
4. **Work on MSHV support** for Dragonball — you noted MSHV is architecturally
   close to KVM, making the port feasible
5. **Advocate for a dense RuntimeClass** on AKS once Kata v3 support lands

This path leverages the Kata community (OpenInfra Foundation, Alibaba, Ant
Group, Intel, Red Hat) and gives our optimizations the widest possible impact.
Our shim becomes R&D that uplifts the ecosystem.

**Timeline:** 3-6 months for contributions, 6-12 months for AKS adoption.

### Option B: VMM Abstraction + Dual Backend (Hedge)

If contributing to Kata v3 is too slow or politically complex, abstract our
shim's VMM layer behind a trait and support both backends:

```rust
trait Vmm: Send + Sync {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmHandle>;
    async fn boot(&self, handle: &VmHandle) -> Result<u32>;
    async fn add_disk(&self, handle: &VmHandle, path: &str, id: &str) -> Result<()>;
    async fn resize_memory(&self, handle: &VmHandle, mb: u64) -> Result<()>;
    async fn shutdown(&self, handle: &VmHandle) -> Result<()>;
}
```

- `CloudHypervisorVmm` — current implementation (HTTP API, separate process)
- `DragonballVmm` — in-process, upcall-based hot-plug (from Kata v3 crates)

This keeps our shim's orchestration layer (caching, inline RPC, cgroup placement)
while gaining Dragonball's single-process density on KVM deployments. CH remains
the backend for MSHV/Azure.

### Option C: Continue Standalone Shim (Current Path)

Keep iterating on containerd-cloudhypervisor with Cloud Hypervisor as the VMM.
This is the lowest-risk path but caps our density at 2 processes per pod and
doesn't benefit the broader ecosystem.

Appropriate if the goal is a focused, single-purpose tool rather than ecosystem
contribution.

### What Not to Do

- **Don't adopt libkrun** — Kata v3 + Dragonball already delivers what libkrun
  promises (VMM as library, single process), with device hot-plug and Kubernetes
  integration that libkrun lacks.

- **Don't build on OpenVMM** — too early, unstable APIs, paravisor-focused.
  Monitor it, don't depend on it.

- **Don't use krun** — incompatible with the Kubernetes pod model.

## Conclusion

**The sandbox daemon architecture delivers the density and performance we
originally projected would require a built-in VMM.** With 150/150 pods in
11 seconds, ~36 MB per-pod RSS (9× better than Kata), and the CPU overhead
concern resolved, the daemon approach validates an alternative to the "VMM
as library" path.

**Kata v3 with Dragonball remains the most promising path for the ecosystem.**
It solves the "VMM as library" problem with production-tested upcall hot-plug,
single-process-per-pod density, and an extensible Rust framework. The only
gap for Azure is MSHV support.

Our shim's innovations (rootfs caching, inline RPC, nobarrier ext4, cgroup
placement, and now daemon VM pooling with userfaultfd restore) are directly
portable to Kata v3's architecture. Contributing them upstream while working
on MSHV for Dragonball would create the ideal solution: Kata's production
orchestration + our density optimizations + Azure compatibility.

For the immediate future, our daemon architecture is the best option for Azure
deployments — it's the only solution achieving **150/150 pods in 11 seconds**
with ~36 MB per-pod RSS and 90% memory headroom on AKS. But the long-term
play is to bring these capabilities to Kata v3.

---

*Analysis based on containerd-cloudhypervisor benchmarks (v0.7.0 through daemon
commit `57b7b4d`) on AKS (westus3, D8ds_v5 nodes, pod spec: 100m CPU / 64Mi
req / 256Mi limit), Cloud Hypervisor v51 with userfaultfd OnDemand restore,
Kata v3 architecture docs, Dragonball source code, and public documentation
for each VMM candidate.*

### References

- [Kata v3 Architecture](https://github.com/kata-containers/kata-containers/tree/main/docs/design/architecture_3.0)
- [Dragonball VMM](https://github.com/kata-containers/kata-containers/blob/main/src/dragonball/README.md)
- [Dragonball Upcall](https://github.com/kata-containers/kata-containers/blob/main/src/dragonball/docs/upcall.md)
- [Kata v3 Feature Comparison](https://github.com/kata-containers/kata-containers/issues/8702)
- [Kata 3.0.0 Release](https://katacontainers.io/blog/getting-rust-y-introducing-kata-containers-3-0-0/)
- [libkrun](https://github.com/containers/libkrun)
- [OpenVMM](https://github.com/microsoft/openvmm)
- [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor)
- [containerd-cloudhypervisor](https://github.com/devigned/containerd-cloudhypervisor)
