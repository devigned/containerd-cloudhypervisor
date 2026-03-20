---
name: aks-benchmark
description: |
  Runs a 150-pod scale benchmark comparing containerd-cloudhypervisor against
  Kata Containers (AKS pod sandboxing) on identical AKS infrastructure.
  Collects scale timing, node metrics, per-pod RSS, and generates a report.
---

# AKS Benchmark Skill

## When to Use

Run this skill when asked to benchmark CloudHV vs Kata on AKS, or when a new
release needs performance validation.

## Prerequisites

- An Azure subscription name from the user
- Azure CLI authenticated (`az account set --subscription "${SUBSCRIPTION_NAME}"`) where ${SUBSCRIPTION_NAME} is the user provide subscription name
- `kubectl` and `helm` installed
- A released version of the CloudHV shim Helm chart on GHCR

## Procedure

### 1. Create Infrastructure

Create a resource group and two AKS clusters (one for each runtime) with
identical D8s_v5 worker nodes:

```bash
REGION="westus3"
RG="rg-bench-<version>"
az group create --name "$RG" --location "$REGION"

# Create both clusters in parallel
az aks create --resource-group "$RG" --name cloudhv-bench --location "$REGION" \
  --node-count 1 --node-vm-size Standard_D2s_v5 --nodepool-name system \
  --generate-ssh-keys --network-plugin azure --os-sku AzureLinux &
az aks create --resource-group "$RG" --name kata-bench --location "$REGION" \
  --node-count 1 --node-vm-size Standard_D2s_v5 --nodepool-name system \
  --generate-ssh-keys --network-plugin azure --os-sku AzureLinux &
wait

# Add worker pools in parallel
az aks nodepool add --resource-group "$RG" --cluster-name cloudhv-bench \
  --name cloudhv --node-count 3 --node-vm-size Standard_D8ds_v5 \
  --max-pods 60 --labels workload=cloudhv --os-sku AzureLinux &
az aks nodepool add --resource-group "$RG" --cluster-name kata-bench \
  --name kata --node-count 3 --node-vm-size Standard_D8ds_v5 \
  --max-pods 60 --os-sku AzureLinux --workload-runtime KataMshvVmIsolation &
wait
```

### 2. Install Shim and Warm Up Metrics

```bash
az aks get-credentials --resource-group "$RG" --name cloudhv-bench
helm install cloudhv-installer oci://ghcr.io/devigned/charts/cloudhv-installer \
  --version <VERSION> --namespace kube-system
```

Wait for the installer DaemonSet to roll out:
```bash
kubectl -n kube-system rollout status daemonset/cloudhv-installer --timeout=180s
```

**Known issue**: The installer's `dmsetup create` on loopback sparse files may
hang silently, preventing containerd config patching and CH binary installation.
Check installer logs (`kubectl -n kube-system logs -l app.kubernetes.io/name=cloudhv-installer`).
If logs stop at "using loopback sparse file...", manually complete the setup on
each node via the installer pod (patch containerd config, install CH, restart
containerd).

**Metrics warmup (critical)**: After installation and any containerd restarts,
deploy a small warmup workload (3 pods, one per node) using the `cloudhv`
RuntimeClass. Wait for all warmup pods to reach `Running`, then poll
`kubectl top nodes` until ALL worker nodes report real values (not `<unknown>`).
This confirms metrics-server has recovered from any containerd restart.

```bash
# Deploy 3 warmup pods
kubectl create deployment warmup --image=hashicorp/http-echo:latest \
  --replicas=3 -- -text=warmup -listen=:5678
kubectl patch deployment warmup -p '{"spec":{"template":{"spec":{
  "runtimeClassName":"cloudhv",
  "nodeSelector":{"workload":"cloudhv"},
  "containers":[{"name":"http-echo","resources":{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"memory":"256Mi"}}}]
}}}}'

# Wait for metrics (poll every 10s, up to 5 min)
until kubectl top nodes 2>&1 | grep "aks-cloudhv" | grep -qv "unknown"; do
  sleep 10
done
kubectl top nodes

# Clean up warmup
kubectl delete deployment warmup
sleep 15
```

Only proceed to the benchmark workload after `kubectl top nodes` shows real
CPU/memory values for all worker nodes.

### 3. Deploy Workload

Use identical pod specs on both clusters:

```yaml
image: hashicorp/http-echo:latest
imagePullPolicy: IfNotPresent
args: ["-text=Hello!", "-listen=:5678"]
resources:
  requests:
    cpu: "100m"
    memory: "64Mi"
  limits:
    memory: "256Mi"
```

RuntimeClassName: `cloudhv` for CloudHV, `kata-vm-isolation` for Kata.

### 4. Scale Benchmark (3 iterations)

For each runtime, run 3 iterations of:

1. Scale deployment to 150 replicas
2. Poll every 5s until target ready or 60s timeout
3. Record: ready count, time, crash count, pending count
4. Wait 15s for metrics to settle
5. Capture `kubectl top nodes`
6. Scale down to 1
7. Record scale-down time
8. Wait 15s cooldown between iterations

### 5. Per-Pod RSS Measurement

After the scale benchmark, deploy a single pod on each runtime and use
`kubectl debug node/<NODE> -it --image=ubuntu` to inspect:

```bash
# For each cloud-hypervisor process:
grep -E "VmRSS|RssShmem" /proc/<PID>/status

# For shim processes:
grep VmRSS /proc/<PID>/status

# For virtiofsd (Kata only):
grep VmRSS /proc/<PID>/status
```

Filter: only report processes with VmRSS > 10000kB for CH, > 3000kB for shims.

### 6. Key Metrics to Collect

| Metric | How | Why |
|--------|-----|-----|
| Scale-up time | Poll deployment readyReplicas | Startup latency |
| Pods ready/150 | Final readyReplicas count | Density ceiling |
| CrashLoopBackOff | Count pod statuses | Reliability |
| Pending | Count pod statuses | Scheduling limit |
| Actual CPU | `kubectl top nodes` at peak | Host CPU cost |
| Actual memory | `kubectl top nodes` at peak | Host memory cost |
| CH VmRSS | `/proc/<pid>/status` | True per-pod memory |
| CH RssShmem | `/proc/<pid>/status` | Guest pages touched |
| Shim RSS | `/proc/<pid>/status` | Shim overhead |
| virtiofsd RSS | `/proc/<pid>/status` (Kata) | virtiofsd overhead |

### 7. Report Format

Save report to `reports/aks-150-pod-scale-cloudhv-v<VERSION>-vs-kata.md` with:
- Test configuration table
- RuntimeClass overhead table with source citations
- Scale-up results (per-iteration table)
- Node metrics at peak
- Per-pod RSS deep dive (measured via /proc, not kubectl top)
- Analysis section explaining CPU and memory differences
- Conclusions

### 8. Cleanup

```bash
az group delete --name "$RG" --yes --no-wait
```

## Important Notes

- The CloudHV installer automatically configures the devmapper snapshotter
  on AKS nodes by detecting ephemeral disks and creating a thin pool.
  Devmapper passthrough (Tier 1) is active when the installer succeeds.
  If no ephemeral disk is available and loopback setup fails, the ext4
  cache fallback (Tier 2) is used instead.
- **Installer reliability**: `dmsetup create` on loopback sparse files can
  hang for minutes, causing the installer script to skip config patching and
  CH binary installation. Always verify installer logs completed through
  "Installation complete" before proceeding.
- **Metrics warmup is essential**: After containerd restarts (from shim
  installation), metrics-server loses connection to worker nodes. Deploying
  a small warmup workload and waiting for `kubectl top nodes` to report
  real values ensures metrics are available during the benchmark. Without
  this step, worker nodes will show `<unknown>` throughout the benchmark.
- Kata on AKS uses `disable_block_device_use = true` and virtiofsd for rootfs.
- The 600Mi Kata RuntimeClass overhead is set by Microsoft's AKS addon, not
  by the Kata project (which recommends 130Mi for Cloud Hypervisor).
- CloudHV's 50Mi overhead accurately reflects ~59MB actual per-pod RSS.
- Per-pod RSS via /proc (using installer pods with host PID access) is more
  reliable than `kubectl top` for per-process memory measurement.
- Always delete Azure resources after benchmarking to avoid charges.
