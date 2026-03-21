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
- Azure CLI authenticated (`az account set --subscription "${SUBSCRIPTION_NAME}"`)
- `kubectl` and `helm` installed
- A dev image built with `hacks/build-dev-image.sh` OR a released version

## Procedure

### 1. Create Infrastructure

Create a resource group, an Azure Monitor Workspace (for Managed Prometheus),
and two AKS clusters with identical D8ds_v5 worker nodes:

```bash
REGION="westus3"
RG="rg-bench-<version>"
az group create --name "$RG" --location "$REGION"

# Create Azure Monitor Workspace for Managed Prometheus
az monitor account create --name bench-monitor \
  --resource-group "$RG" --location "$REGION" -o none
MONITOR_ID=$(az monitor account show --name bench-monitor \
  --resource-group "$RG" --query id -o tsv)

# Create both clusters in parallel
az aks create --resource-group "$RG" --name cloudhv-bench --location "$REGION" \
  --node-count 1 --node-vm-size Standard_D2s_v5 --nodepool-name system \
  --generate-ssh-keys --network-plugin azure --os-sku AzureLinux -o none &
az aks create --resource-group "$RG" --name kata-bench --location "$REGION" \
  --node-count 1 --node-vm-size Standard_D2s_v5 --nodepool-name system \
  --generate-ssh-keys --network-plugin azure --os-sku AzureLinux -o none &
wait

# Add worker pools in parallel
az aks nodepool add --resource-group "$RG" --cluster-name cloudhv-bench \
  --name cloudhv --node-count 3 --node-vm-size Standard_D8ds_v5 \
  --max-pods 60 --labels workload=cloudhv --os-sku AzureLinux -o none &
az aks nodepool add --resource-group "$RG" --cluster-name kata-bench \
  --name kata --node-count 3 --node-vm-size Standard_D8ds_v5 \
  --max-pods 60 --os-sku AzureLinux --workload-runtime KataMshvVmIsolation -o none &
wait
```

### 2. Enable Managed Prometheus

Enable Azure Managed Prometheus on **both** clusters. This deploys an
`ama-metrics` DaemonSet that collects node metrics independently of the
kubelet metrics endpoint (which times out under heavy VM load).

```bash
az aks update --resource-group "$RG" --name cloudhv-bench \
  --enable-azure-monitor-metrics \
  --azure-monitor-workspace-resource-id "$MONITOR_ID" -o none &
az aks update --resource-group "$RG" --name kata-bench \
  --enable-azure-monitor-metrics \
  --azure-monitor-workspace-resource-id "$MONITOR_ID" -o none &
wait
```

**Why Managed Prometheus?** Under 150-pod load, CloudHV nodes run ~50
cloud-hypervisor processes that make kubelet slow to respond. The default
metrics-server times out (`"timeout to access kubelet"`) and reports
`<unknown>` for worker nodes. Managed Prometheus scrapes independently
and does not have this problem.

Verify the metrics agent is running after enablement:

```bash
kubectl get pods -n kube-system -l dsName=ama-metrics-node
```

### 3. Install CloudHV Shim

Use dedicated KUBECONFIG files to prevent context drift between clusters:

```bash
export KUBECONFIG=/tmp/cloudhv-bench-kubeconfig
az aks get-credentials --resource-group "$RG" --name cloudhv-bench --overwrite-existing
```

For dev builds, create a pull secret and install from the **local chart**
(the published chart may not have the latest `imagePullSecrets` support):

```bash
SHA=$(git rev-parse --short HEAD)
GH_TOKEN=$(gh auth token)
kubectl create secret docker-registry ghcr-secret \
  --docker-server=ghcr.io --docker-username="${GHCR_OWNER}" \
  --docker-password="${GH_TOKEN}" -n kube-system

helm install cloudhv-installer ./charts/cloudhv-installer \
  --namespace kube-system \
  --set image.repository=ghcr.io/${GHCR_OWNER}/cloudhv-installer-dev \
  --set image.tag=${SHA} \
  --set "imagePullSecrets[0].name=ghcr-secret"
```

For released versions:

```bash
helm install cloudhv-installer oci://ghcr.io/devigned/charts/cloudhv-installer \
  --version <VERSION> --namespace kube-system
```

Wait for all installer pods to complete:

```bash
kubectl rollout status daemonset/cloudhv-installer -n kube-system --timeout=300s
# Verify all 3 nodes show "Installer idle. Shim is active."
kubectl logs -n kube-system -l app.kubernetes.io/name=cloudhv-installer --tail=3
```

### 4. Deploy Workload

Use identical pod specs on both clusters. The `command` array format is
required for CloudHV (arg-only format causes `exec format error`):

```yaml
containers:
  - name: http-echo
    image: hashicorp/http-echo:latest
    imagePullPolicy: IfNotPresent
    command: ["/http-echo", "-text=Hello!", "-listen=:5678"]
    resources:
      requests:
        cpu: "100m"
        memory: "64Mi"
      limits:
        memory: "256Mi"
```

RuntimeClassName: `cloudhv` for CloudHV, `kata-vm-isolation` for Kata.
(Note: Kata RuntimeClass is `kata-vm-isolation`, NOT `kata-mshv-vm-isolation`.)

For CloudHV with warm snapshot restore, deploy 1 replica first, wait for
it to become Ready, then wait 35s for the snapshot cache to build before
scaling:

```bash
kubectl scale deployment bench --replicas=1
kubectl wait --for=condition=Ready pod -l app=bench --timeout=60s
sleep 35  # snapshot cache creation
kubectl scale deployment bench --replicas=150
```

### 5. Scale Benchmark (3 iterations)

For each runtime, use the runtime's dedicated KUBECONFIG and run 3 iterations:

1. Scale deployment to 150 replicas
2. Poll every 5s until target ready or 180s timeout
3. Record: ready count, time, crash count, pending count
4. Capture node memory via `free -m` on each worker node (VMSS run-command)
5. Scale down to 0
6. Wait 30s cooldown between iterations

### 6. Per-Pod RSS Measurement

After the scale benchmark, deploy a single pod on each runtime and inspect
via VMSS run-command on the hosting node:

```bash
NODE_RG=$(az aks show -g $RG -n cloudhv-bench --query nodeResourceGroup -o tsv)
VMSS=$(az vmss list --resource-group "$NODE_RG" \
  --query "[?contains(name,'cloudhv')].name" -o tsv)

az vmss run-command invoke -g "$NODE_RG" -n "$VMSS" --instance-id 0 \
  --command-id RunShellScript --scripts '
for pid in $(pgrep -f cloud-hyper); do
  echo "CH PID $pid:"
  grep -E "VmRSS|RssShmem" /proc/$pid/status
done
for pid in $(pgrep -f containerd-shim-cloudhv); do
  echo "Shim PID $pid:"
  grep VmRSS /proc/$pid/status
done
'
```

### 7. Key Metrics to Collect

| Metric | How | Why |
|--------|-----|-----|
| Scale-up time | Poll deployment readyReplicas | Startup latency |
| Pods ready/150 | Final readyReplicas count | Density ceiling |
| CrashLoopBackOff | Count pod statuses | Reliability |
| Pending | Count pod statuses | Scheduling limit |
| Node memory | `free -m` via VMSS run-command | True node memory |
| Node CPU | Managed Prometheus or `kubectl top` | Host CPU cost |
| CH VmRSS | `/proc/<pid>/status` | True per-pod memory |
| CH RssShmem | `/proc/<pid>/status` | Guest pages touched |
| Shim RSS | `/proc/<pid>/status` | Shim overhead |

### 8. Report Format

Save report to `reports/aks-150-pod-scale-cloudhv-v<VERSION>-vs-kata.md` with:
- Test configuration table (date, VM sizes, versions, image SHA)
- Scale-up results (per-iteration table)
- Node memory at peak (from `free -m`, not `kubectl top`)
- Per-pod RSS deep dive (measured via /proc)
- Analysis section explaining memory differences
- Conclusions

### 9. Cleanup

```bash
az group delete --name "$RG" --yes --no-wait
```

## Important Notes

- **Always use `hacks/build-dev-image.sh`** for dev builds. Never build
  components separately — this prevents version skew.
- **Always use dedicated KUBECONFIG files** per cluster (`export
  KUBECONFIG=/tmp/<name>-kubeconfig`) to prevent context drift when
  operating on multiple clusters.
- **Never patch the DaemonSet after install.** If settings are wrong,
  `helm uninstall` and reinstall with correct `--set` values.
- **Managed Prometheus is required** for reliable metrics under load.
  Without it, `kubectl top nodes` returns `<unknown>` for CloudHV workers
  because kubelet times out under heavy CH process load.
- **Use `command` array format** for http-echo pods, not `args`. CloudHV
  requires the full command path.
- Kata on AKS uses `kata-vm-isolation` RuntimeClass (not
  `kata-mshv-vm-isolation`).
- For CloudHV with warm snapshots, seed the cache with 1 pod + 35s wait
  before scaling. Different container images produce different snapshots.
- Always delete Azure resources after benchmarking to avoid charges.
