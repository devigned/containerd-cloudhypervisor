# Running VM-Isolated Containers on AKS

This example walks through deploying the containerd-cloudhypervisor shim on
Azure Kubernetes Service using the published Helm chart and release artifacts.

## Prerequisites

- Azure CLI (`az`) authenticated
- `kubectl` and `helm` installed
- An Azure subscription with quota for D-series VMs

## 1. Create an AKS Cluster

```bash
REGION="westus3"
RG="rg-cloudhv-demo"
CLUSTER="cloudhv-demo"

# Create resource group
az group create --name "$RG" --location "$REGION"

# Create cluster with a system node pool
az aks create \
  --resource-group "$RG" \
  --name "$CLUSTER" \
  --location "$REGION" \
  --node-count 1 \
  --node-vm-size Standard_D2s_v5 \
  --nodepool-name system \
  --generate-ssh-keys \
  --network-plugin azure

# Add a worker pool with the cloudhv label (3 nodes with nested virt)
az aks nodepool add \
  --resource-group "$RG" \
  --cluster-name "$CLUSTER" \
  --name cloudhv \
  --node-count 3 \
  --node-vm-size Standard_D4s_v5 \
  --labels workload=cloudhv

# Get credentials
az aks get-credentials --resource-group "$RG" --name "$CLUSTER"
```

## 2. Install the Shim with Helm

The Helm chart is published to GHCR as an OCI artifact with each release.

```bash
# Install the latest release
helm install cloudhv-installer \
  oci://ghcr.io/devigned/charts/cloudhv-installer \
  --version 0.1.2 \
  --namespace kube-system
```

This creates:
- A **DaemonSet** that installs the shim, kernel, rootfs, virtiofsd, and
  Cloud Hypervisor onto each node labeled `workload=cloudhv`
- A **RuntimeClass** named `cloudhv` with pod overhead annotations

Verify installation:

```bash
# Check installer pods (should show Running on each worker node)
kubectl -n kube-system get pods -l app.kubernetes.io/name=cloudhv-installer

# Check installer logs
kubectl -n kube-system logs -l app.kubernetes.io/name=cloudhv-installer --tail=5

# Verify RuntimeClass exists
kubectl get runtimeclass cloudhv
```

## 3. Run a Container in a MicroVM

```bash
kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: echo-test
spec:
  runtimeClassName: cloudhv
  containers:
    - name: echo
      image: hashicorp/http-echo:latest
      args: ["-text=Hello from Cloud Hypervisor on AKS!", "-listen=:5678"]
      ports:
        - containerPort: 5678
  restartPolicy: Never
EOF

# Wait for it to start
kubectl get pod echo-test -w

# Verify it responds
POD_IP=$(kubectl get pod echo-test -o jsonpath='{.status.podIP}')
kubectl run curl-test --image=curlimages/curl:latest --rm -it --restart=Never \
  -- curl -s "http://$POD_IP:5678/"
# Output: Hello from Cloud Hypervisor on AKS!

# Check logs
kubectl logs echo-test
```

## 4. Customize VM Resources (Optional)

Override VM memory and vCPUs per-pod using annotations:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: large-vm-pod
  annotations:
    io.cloudhv.config.hypervisor.default_memory: "2048"
    io.cloudhv.config.hypervisor.default_vcpus: "4"
spec:
  runtimeClassName: cloudhv
  containers:
    - name: app
      image: myapp:latest
```

See [Configuration — Pod Annotations](../../docs/configuration.md#pod-annotations) for
the full list of supported annotations.

## 5. Clean Up

```bash
# Delete test pods
kubectl delete pod echo-test --ignore-not-found

# Uninstall the shim
helm uninstall cloudhv-installer --namespace kube-system

# Delete the AKS cluster
az aks delete --resource-group "$RG" --name "$CLUSTER" --yes --no-wait
az group delete --name "$RG" --yes --no-wait
```

## Helm Chart Values

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` | `ghcr.io/devigned/cloudhv-installer` | Installer image |
| `image.tag` | `v<appVersion>` | Image tag |
| `nodeSelector` | `workload: cloudhv` | Target nodes |
| `runtimeClass.enabled` | `true` | Create RuntimeClass |
| `runtimeClass.overhead.memory` | `50Mi` | Pod overhead |

See [`charts/cloudhv-installer/values.yaml`](../../charts/cloudhv-installer/values.yaml)
for all configurable values.
