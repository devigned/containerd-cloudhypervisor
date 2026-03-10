# AKS Example: VM-Isolated Containers with Cloud Hypervisor

This example deploys the containerd-cloudhypervisor shim onto an AKS cluster,
demonstrating VM-isolated container workloads with block device rootfs delivery.

## What This Demo Shows

1. **VM isolation on Kubernetes** — each pod runs inside its own Cloud Hypervisor
   microVM with a dedicated kernel, providing hardware-level isolation
2. **Block device rootfs** — container images are delivered as hot-plugged virtio-blk
   disks (no FUSE), enabling proper mount namespaces and multi-container pods
3. **Cold start latency** — ~460ms from pod creation to running (257ms VM boot +
   58ms disk image + 145ms container start)

## Architecture

```text
┌── AKS Cluster ──────────────────────────────────────────────┐
│                                                             │
│  System Pool (D2s_v5)        Worker Pool (D4s_v5, KVM)      │
│  ┌──────────────┐            ┌────────────────────────────┐ │
│  │ kube-system  │            │ ┌─ Pod (cloudhv) ────────┐ │ │
│  │ coredns, etc │            │ │  cloud-hypervisor VMM  │ │ │
│  └──────────────┘            │ │  ┌───────────────────┐ │ │ │
│                              │ │  │ Guest VM          │ │ │ │
│  RuntimeClass: cloudhv       │ │  │  agent → crun     │ │ │ │
│  ┌──────────────────┐        │ │  │  /dev/vdb (rootfs)│ │ │ │
│  │ handler: cloudhv │────►   │ │  └───────────────────┘ │ │ │
│  └──────────────────┘        │ └────────────────────────┘ │ │
│                              │                            │ │
│  DaemonSet: installer        │  Installs: shim, kernel,   │ │
│  (one per node)              │  rootfs, virtiofsd, CH     │ │
│                              └────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

**Container rootfs flow:**

1. containerd pulls image → overlayfs snapshot on host
2. Shim creates ext4 disk image from snapshot
3. Hot-plugs disk into VM via Cloud Hypervisor `vm.add-disk` API
4. Agent discovers `/dev/vdX`, mounts ext4, runs crun

## Prerequisites

- Azure CLI (`az`) logged in with AKS quota
- `kubectl` configured
- Docker with buildx (to build the installer image)
- A container registry (ACR recommended)

## Quick Start

### 1. Set environment variables

```bash
export AZURE_SUBSCRIPTION="<your-subscription-id>"
export AZURE_REGION="eastus2"
export RESOURCE_GROUP="rg-cloudhv-demo"
export CLUSTER_NAME="aks-cloudhv-demo"
export INSTALLER_IMAGE="<your-registry>/cloudhv-installer:latest"
```

### 2. Build and push the installer image

From the repository root:

```bash
docker buildx build -t "$INSTALLER_IMAGE" -f example/aks/installer/Dockerfile . --push
```

This multi-stage build compiles the shim, agent, guest kernel (with BPF + ACPI
hot-plug), crun, virtiofsd, and rootfs into a single image.

### 3. Create cluster and deploy

```bash
# Create AKS cluster with KVM-capable nodes
bash example/aks/setup.sh

# Install shim on nodes via DaemonSet
sed "s|INSTALLER_IMAGE_PLACEHOLDER|${INSTALLER_IMAGE}|g" \
  example/aks/manifests/daemonset.yaml | kubectl apply -f -
kubectl apply -f example/aks/manifests/runtimeclass.yaml

# Wait for installer to complete
kubectl -n kube-system wait --for=condition=ready pod -l app=cloudhv-installer --timeout=120s
```

### 4. Run a VM-isolated container

```bash
kubectl run test --image=busybox:latest --restart=Never --runtime-class=cloudhv -- echo hello
kubectl get pod test        # Should show 1/1 Running
kubectl delete pod test
```

### 5. Deploy the echo server workload

```bash
kubectl apply -f example/aks/manifests/echo-deployment.yaml
kubectl get pods -l app=echo-cloudhv
```

## Performance

Measured on Azure D8s_v5 (8 vCPU, KVM):

| Phase | Latency |
| ------- | --------- |
| VM boot (sandbox creation) | 257ms |
| Container create (disk image + hot-plug) | 58ms |
| Container start (crun run) | 145ms |
| **Total cold start** | **~460ms** |
| Sandbox stop + cleanup | 92ms |

| Resource | Per-VM Overhead |
| ---------- | ---------------- |
| Cloud Hypervisor VMM | ~50 MB RSS |
| virtiofsd | ~5 MB RSS |
| Shim processes | ~10 MB RSS |
| VM guest memory | 512 MB (configurable) |

## Files

```text
example/aks/
├── README.md                          # This file
├── setup.sh                           # Create AKS cluster
├── teardown.sh                        # Delete cluster + resource group
├── demo.sh                            # Full demo orchestration
├── installer/
│   ├── Dockerfile                     # Multi-stage: shim + kernel + rootfs + crun + virtiofsd
│   └── install.sh                     # Node-level install (copies binaries, patches containerd)
└── manifests/
    ├── runtimeclass.yaml              # RuntimeClass: cloudhv
    ├── daemonset.yaml                 # DaemonSet: shim installer
    └── echo-deployment.yaml           # Demo: echo server + LoadBalancer
```

## Cleanup

```bash
# Delete workloads only
kubectl delete -f example/aks/manifests/echo-deployment.yaml
kubectl delete -f example/aks/manifests/runtimeclass.yaml
kubectl -n kube-system delete daemonset cloudhv-installer

# Delete everything (cluster + resource group)
bash example/aks/teardown.sh
```
