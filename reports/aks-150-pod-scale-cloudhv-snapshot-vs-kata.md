# AKS 150-Pod Scale Benchmark: CloudHV Sandbox Daemon with VM Pooling

**Date**: 2026-07-15
**Commit**: `57b7b4d`
**Region**: West US 3

## Architecture

The sandbox daemon replaces the per-pod VMM spawn model with a node-level
daemon that pre-boots VM pools from base snapshots:

- **Standalone sandbox daemon** pre-boots VM pools from base snapshots using
  Cloud Hypervisor v51's userfaultfd `OnDemand` restore
- **Shadow VMs** create warm workload snapshots in the background
- **Thin shim** (~1,180 lines) handles only TAP networking, erofs conversion,
  and daemon RPCs
- **Image key** uses content digest from containerd's gRPC image service

## Test Configuration

| Parameter | Value |
|-----------|-------|
| AKS Cluster | `cloudhv-bench` in `rg-bench-final` |
| Worker Nodes | 3 × Standard_D8ds_v5 (8 vCPU, 32 GiB RAM) |
| Max Pods/Node | 75 |
| OS | AzureLinux 3.0 |
| Kubernetes | v1.33.x |
| containerd | 2.0.x |
| Runtime | `cloudhv` (Cloud Hypervisor v51, built from source) |
| CH Restore | userfaultfd `OnDemand` (PR #7800) |
| VM Memory | 128 MB allocated |
| Workload | `hashicorp/http-echo:latest` |
| Pod Resources | 100m CPU request, 64Mi/256Mi memory |

## Scale-Up Results

### 150/150 Pods Ready in 11 Seconds

```
  0s:   0/150
  2s:  30/150
  4s:  65/150
  6s: 100/150
  8s: 130/150
 11s: 150/150  ← ALL READY
```

**150/150 Running, 0 Pending, 0 Failed.**

## Node Memory at 150 Pods

| Metric | Per Node | Notes |
|--------|----------|-------|
| Total RAM | 32 GiB | D8ds_v5 |
| Used at peak | ~3.2 GiB | 10% utilization |
| Available | ~28.8 GiB | 90% headroom |
| VM RSS | ~24 MB per VM | userfaultfd OnDemand — only faulted pages resident |
| Daemon RSS | ~2.2 MB | Single daemon per node |

### Per-Pod Memory (RSS)

| Component | RSS | Notes |
|-----------|----:|-------|
| VM (Cloud Hypervisor) | ~24 MB | OnDemand userfaultfd — minimal faulted pages |
| Shim (sandbox) | ~6 MB | Per-sandbox process |
| Shim (container) | ~6 MB | Per-container process |
| **Total per pod** | **~36 MB** | **9× lower than Kata (~330 MB)** |

Previous snapshot-based approach (v0.11.0): ~529 MB VmRSS (99% shared via CoW).
The daemon's userfaultfd `OnDemand` restore keeps only faulted pages resident,
reducing per-VM RSS from ~529 MB to ~24 MB.

## Shim-Side Timing

### Cold Path (no warm snapshot available)

| Phase | Time |
|-------|------|
| erofs conversion | 8 ms |
| Daemon acquire (pool restore) | 63 ms |
| **Total shim inner** | **74 ms** |

### Warm Snapshot Path

| Phase | Time |
|-------|------|
| Daemon acquire | 24 ms |
| Agent connect + net config | included |
| **Total shim inner** | **168 ms** |

### Pool Restore

| Metric | Value |
|--------|-------|
| VM restore from snapshot (userfaultfd OnDemand) | **25 ms** |

## CloudHV Version Comparison

All benchmarks on identical infrastructure: 3 × D8ds_v5 (32 GiB, 8 vCPU),
150-pod target, `hashicorp/http-echo:latest`.

| Metric | v0.7.0 | v0.11.0 | Daemon (v51) |
|--------|--------|---------|--------------|
| Pods ready (of 150) | 150 | 150 | **150** |
| Time to 150 pods | 27s | 77s | **11s** |
| Architecture | Direct CH spawn | Warm snapshot restore | **Daemon + VM pool** |
| Per-VM RSS | ~54 MB | ~529 MB (99% shared) | **~24 MB** |
| Per-pod total RSS | ~59 MB | ~5 MB unique | **~36 MB** |
| Shim inner time | ~350 ms | ~344 ms | **74–168 ms** |
| CH version | v38 | v44 | **v51** |
| Restore method | — (cold boot) | CoW snapshot | **userfaultfd OnDemand** |

**v0.7.0** (27s): Direct CH process spawn per pod, cold boot, lazy boot
optimization. Fast at scale but each VM is an independent process.

**v0.11.0** (77s): Warm snapshot restore with CoW memory sharing. First pod
cold-boots and creates a snapshot; subsequent pods restore from snapshot. High
CoW sharing (524 MB shared per VM) but cold-boot seed pod creates a bottleneck
and snapshot cache warming adds latency at burst scale.

**Daemon** (11s): Sandbox daemon pre-boots VM pools from base snapshots.
userfaultfd `OnDemand` restore keeps only faulted pages resident (~24 MB vs
529 MB). No cold-boot bottleneck — pool VMs are pre-warmed. Shadow VMs create
workload snapshots in the background.

## Kata Comparison

From previous benchmarks on identical D8ds_v5 × 3 infrastructure:

| Metric | CloudHV Daemon | Kata (AKS) |
|--------|---------------|------------|
| Pods ready (of 150) | **150** | 130 (OOM) |
| Time to ready | **11s** | ~64s (then stuck) |
| Per-pod RSS | **~36 MB** | ~330 MB |
| Memory density | **9× better** | — |
| Node memory at peak | ~3.2 GiB (10%) | ~13.5 GiB (42%) |
| Failures | **0** | 20 unschedulable |

Kata caps at 130 pods due to the 600Mi RuntimeClass overhead and ~330 MB
actual RSS per pod. The CloudHV daemon achieves **9× better memory density**
with ~36 MB per pod and 90% node memory headroom at 150 pods.

## Conclusion

The CloudHV sandbox daemon with VM pooling achieves:
- **150/150 pods in 11 seconds** on 3 × D8ds_v5 nodes (0 failures)
- **~24 MB RSS per VM** via userfaultfd OnDemand restore
- **~3.2 GiB per node** at 150 pods (10% utilization, 90% headroom)
- **25 ms pool restore time** per VM
- **74–168 ms shim inner time** (cold/warm paths)
- **9× better memory density** vs Kata Containers
- **7× faster scale-up** than v0.11.0 (77s), **2.5× faster** than v0.7.0 (27s)
- **Daemon RSS: ~2.2 MB** per node — negligible overhead

The daemon architecture resolves the per-pod VMM process overhead that drove
high CPU usage in previous versions. Pre-pooled VMs eliminate cold-boot
bottlenecks, and userfaultfd `OnDemand` restore minimizes resident memory.

---

*Benchmark commit `57b7b4d` on AKS (westus3), 3 × D8ds_v5 nodes. Pod spec:
100m CPU, 64Mi mem request, 256Mi mem limit. Cloud Hypervisor v51 built from
source with userfaultfd OnDemand restore (PR #7800).*
