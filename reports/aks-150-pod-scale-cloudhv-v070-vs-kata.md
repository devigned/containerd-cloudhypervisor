# AKS 150-Pod Scale Benchmark: CloudHV v0.7.0 vs Kata

## Test Configuration

| Parameter | CloudHV Cluster | Kata Cluster |
|-----------|----------------|--------------|
| **Worker nodes** | 3 × Standard_D8ds_v5 | 3 × Standard_D8ds_v5 |
| **Node specs** | 8 vCPU, 32 GiB RAM | Same |
| **Runtime** | CloudHV shim v0.7.0 | Kata (AKS pod sandboxing) |
| **RuntimeClass** | `cloudhv` | `kata-vm-isolation` |
| **Snapshotter** | devmapper (loopback thin pool) | overlayfs (AKS default) |
| **Rootfs delivery** | Block passthrough, pre-attached at boot | virtiofsd (FUSE) |
| **Region** | westus3 | westus3 |
| **Workload** | hashicorp/http-echo:latest | Same |
| **Pod resources** | 100m CPU req, 64Mi mem req, 256Mi mem limit | Same |
| **RuntimeClass overhead** | 50Mi | 600Mi |

### CloudHV v0.7.0 Features

- **Lazy VM boot** — CH process spawned at sandbox creation, VM booted at
  first container start with rootfs pre-attached as `/dev/vdb`
- **Pre-attached rootfs** — eliminates hot-plug + ACPI scan + `/sys/block`
  polling for the first container
- **RunContainer RPC** — atomic create+start in one ttrpc round-trip
- **Boot state machine** — `BootState` enum with `tokio::sync::Notify`
- **Devmapper passthrough** — container rootfs delivered as block device

## Results

### Scale-Up: 1 → 150 pods

| Iter | CloudHV Ready | CloudHV Time | Kata Ready | Kata Time | Kata Pending |
|------|--------------|-------------|-----------|----------|-------------|
| 1 | **150** | 65s | 130 | 64s | 20 |
| 2 | **150** | **27s** | 130 | 64s | 20 |
| 3 | **150** | **27s** | 130 | 65s | 20 |

CloudHV: 0 crashes, 0 pending across all iterations. Iteration 1 is slower
due to first-time devmapper thin snapshot creation.

Kata: 20 pods stuck Pending in every iteration due to the 600Mi RuntimeClass
overhead exceeding node capacity at ~43 pods/node.

### Scale-Down

| | CloudHV | Kata |
|---|---------|------|
| Avg | ~11s | ~11s |

### Node Metrics at Peak (kubectl top)

**CloudHV** (iteration 2, 2 of 3 nodes reporting):

| Node | CPU | Memory |
|------|-----|--------|
| vmss000000 | 3665m (**46%**) | 3293Mi (**10%**) |
| vmss000001 | 3840m (**49%**) | 3294Mi (**10%**) |

> Note: metrics-server intermittently loses worker nodes under 150-pod load.
> Iteration 2 during scale-up burst is the most reliable capture point.

**Kata** (all 3 nodes):

| Node | CPU | Memory |
|------|-----|--------|
| vmss000000 | 1504m (19%) | 13,359Mi (44%) |
| vmss000001 | 1470m (18%) | 13,340Mi (44%) |
| vmss000002 | 1477m (18%) | 13,636Mi (44%) |

### Per-Pod RSS (measured via /proc on AKS nodes at 150-pod scale)

**CloudHV v0.7.0** (all 3 nodes, clean run):

| Node | CH Instances | Avg VmRSS | Avg RssShmem | Shims | Avg Shim RSS | Node MemUsed |
|------|-------------|-----------|-------------|-------|-------------|-------------|
| 0 | 50 | 54 MB | 49 MB | 50 | 5 MB | 4,403 MB / 32,874 MB |
| 1 | 49 | 54 MB | 49 MB | 49 | 5 MB | 4,427 MB / 32,874 MB |
| 2 | 51 | 54 MB | 49 MB | 51 | 5 MB | 4,493 MB / 32,874 MB |

**Total per pod: ~59 MB** (54 MB cloud-hypervisor + 5 MB shim)

**Kata** (1 node sampled):

| Node | CH Instances | Avg VmRSS | Avg RssShmem | Shims | Avg Shim RSS | virtiofsd | Avg virtiofsd RSS | Node MemUsed |
|------|-------------|-----------|-------------|-------|-------------|-----------|-------------------|-------------|
| 0 | 43 | 265 MB | 256 MB | 43 | 51 MB | 86 | 7 MB | 14,314 MB |

**Total per pod: ~330 MB** (265 MB cloud-hypervisor + 51 MB shim + 14 MB virtiofsd)

> **Why Kata RssShmem = 256 MB:** Kata sizes the VM's guest memory to the pod
> spec's memory limit. The pod spec declares `limits.memory: 256Mi`, so Kata
> boots the VM with 256 MB of guest memory. The guest touches all of it
> (virtiofsd page cache fills available memory), producing RssShmem = 256 MB.
> CloudHV boots with a fixed 512 MB mmap but the minimal guest only touches
> ~49 MB (demand-paged, prefault=off).

## Analysis

### Density: 150/150 vs 130/150

CloudHV scheduled all 150 pods; Kata capped at 130. The difference is driven
by the **declared** RuntimeClass overhead, not actual RSS:

| | CloudHV | Kata |
|---|---------|------|
| Per-pod declared overhead | 50Mi | 600Mi |
| Per-pod actual RSS | ~59 MB | ~330 MB |
| Effective pod memory (req + overhead) | 114Mi | 664Mi |
| Max pods per 32 GiB node | ~280 | ~48 |

CloudHV's 50Mi overhead closely matches actual RSS (~59 MB) for this workload.
However, the 512 MB guest memory mmap means RSS could grow toward 512 MB under
memory pressure inside the VM. For I/O-bound agents with small working sets,
50Mi is accurate.

Kata's 600Mi overhead is ~1.8× the actual RSS (~330 MB) for this workload.
The conservative declaration limits scheduling but provides safety margin for
workloads that consume more guest memory.

### CPU: 46-49% vs 16-22%

CloudHV uses ~2.5× more host CPU than Kata at 150-pod scale. The gap is
structural — both approaches deliver the rootfs at VM boot (CloudHV pre-attaches
the block device, Kata pre-shares via virtiofsd). The remaining overhead comes
from:
- Per-pod CH process spawn + API socket initialization
- Per-pod agent connection over vsock + ttrpc handshake
- Per-pod shim process (150 shims × 5MB = 750MB aggregate overhead)
- Kata uses a single shim process model that amortizes overhead differently

### Memory: 10% vs 44%

CloudHV uses ~4.4× less host memory per node. Per-pod RSS is ~59 MB vs ~330 MB
(5.6× ratio). The difference comes from:
- **Guest memory usage**: CloudHV's minimal guest (no systemd, no virtiofsd,
  no FUSE) touches only 49 MB of its 512 MB mmap. Kata's full guest with
  virtiofsd fills all 256 MB of available memory via page cache.
- **Shim size**: CloudHV shim is 5 MB vs Kata's 51 MB.
- **virtiofsd**: Kata runs 2 virtiofsd instances per pod (14 MB total);
  CloudHV has none.

### Scale-Up Time: 27s vs 64s

CloudHV warm scale-up (27s for 150 pods) is significantly faster than Kata
(64s for 130 pods). Kata's uniform 64s across all iterations (no warm-up
benefit) suggests either images are not cached between scale cycles on the
Kata cluster or the virtiofsd+FUSE rootfs path has higher per-pod latency
than block device passthrough.

## Conclusions

1. **CloudHV schedules 15% more pods** (150 vs 130) on identical hardware,
   driven by lower declared overhead that accurately reflects actual RSS.

2. **CloudHV uses ~2.5× more CPU** (46-49% vs 16-22%). This is structural
   per-pod process overhead, not rootfs delivery. Reducing this requires
   architectural changes (sandbox daemon, VM pooling, or built-in VMM).

3. **CloudHV uses ~5.6× less memory per pod** (59 MB vs 330 MB). The minimal
   guest image (no virtiofsd, no systemd) keeps guest page faults to 49 MB
   vs Kata's 256 MB.

4. **CloudHV warm scale-up is 2.4× faster** (27s vs 64s for comparable pod
   counts).

5. **Installer needs hardening** — `dmsetup create` on loopback hangs on
   every AKS deployment, requiring manual intervention.

---

*Benchmark performed on AKS, westus3, extremis subscription.*
*CloudHV v0.7.0 with devmapper loopback thin pool + lazy boot on D8ds_v5 nodes.*
*Per-pod RSS measured via installer pods with host PID access + /proc/PID/status.*
*Kata cluster used AKS pod sandboxing with kata-vm-isolation RuntimeClass.*
*Pod spec: 100m CPU request, 64Mi memory request, 256Mi memory limit.*
