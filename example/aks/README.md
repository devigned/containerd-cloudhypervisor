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
  --version 0.1.3 \
  --namespace kube-system
```

This creates:
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

## 3. Deploy an Echo Service

Deploy an HTTP echo server as a Deployment with a Service, making it easy to
scale up and down. Each replica runs inside its own Cloud Hypervisor microVM.

```bash
kubectl apply -f - <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: echo-cloudhv
  labels:
    app: echo-cloudhv
spec:
  replicas: 1
  selector:
    matchLabels:
      app: echo-cloudhv
  template:
    metadata:
      labels:
        app: echo-cloudhv
    spec:
      runtimeClassName: cloudhv
      containers:
        - name: echo
          image: hashicorp/http-echo:latest
          args: ["-text=Hello from Cloud Hypervisor on AKS!", "-listen=:5678"]
          ports:
            - containerPort: 5678
          resources:
            requests:
              cpu: "100m"
              memory: "64Mi"
---
apiVersion: v1
kind: Service
metadata:
  name: echo-cloudhv
spec:
  selector:
    app: echo-cloudhv
  ports:
    - port: 80
      targetPort: 5678
  type: LoadBalancer
EOF

# Wait for the deployment to be ready
kubectl rollout status deployment echo-cloudhv

# Get the external IP (may take a minute to provision)
echo "Waiting for external IP..."
kubectl get service echo-cloudhv -w

# Once EXTERNAL-IP is assigned:
EXTERNAL_IP=$(kubectl get service echo-cloudhv -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
curl http://$EXTERNAL_IP/
# Output: Hello from Cloud Hypervisor on AKS!
```

### Scale Up

Each new replica boots a fresh microVM in ~300ms:

```bash
# Scale to 5 replicas (5 VMs across 3 nodes)
kubectl scale deployment echo-cloudhv --replicas=5
kubectl get pods -l app=echo-cloudhv -o wide -w
```

### Scale Down

```bash
kubectl scale deployment echo-cloudhv --replicas=1
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
# Delete the echo service
kubectl delete deployment echo-cloudhv
kubectl delete service echo-cloudhv

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
