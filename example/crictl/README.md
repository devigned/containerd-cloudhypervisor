# Running VM-Isolated Containers with crictl

This example walks through running containers inside Cloud Hypervisor microVMs
on a bare Linux machine using `crictl` — no Kubernetes required. By the end
you'll have an HTTP echo server running inside a VM, reachable from the host.

## What You'll See

```text
Host
 └── containerd
      └── containerd-shim-cloudhv-v1
           └── cloud-hypervisor (microVM)
                ├── kernel + agent (PID 1)
                ├── container A: hashicorp/http-echo → :5678
                └── container B: nginx → :80
```

Each pod runs in its own VM with a dedicated kernel. Containers inside the pod
share the VM's network namespace (just like regular Kubernetes pods), so they
can talk to each other over `localhost` and are reachable from the host on the
pod IP.

## Prerequisites

| Component | Purpose |
|-----------|---------|
| Linux host with KVM (`/dev/kvm`) | Hypervisor backend |
| containerd (≥1.7) | Container runtime |
| crictl | CRI command-line client |
| CNI plugins | Pod networking |
| Cloud Hypervisor | VMM binary |
| Guest kernel + rootfs | Pre-built VM image |

## Setup

### 1. Install containerd and crictl

```bash
# containerd
wget -q https://github.com/containerd/containerd/releases/download/v1.7.24/containerd-1.7.24-linux-amd64.tar.gz
sudo tar -C /usr/local -xzf containerd-1.7.24-linux-amd64.tar.gz
sudo systemctl enable --now containerd

# crictl
VERSION="v1.31.0"
wget -q "https://github.com/kubernetes-sigs/cri-tools/releases/download/$VERSION/crictl-$VERSION-linux-amd64.tar.gz"
sudo tar -C /usr/local/bin -xzf "crictl-$VERSION-linux-amd64.tar.gz"

# Configure crictl to talk to containerd
sudo tee /etc/crictl.yaml > /dev/null <<EOF
runtime-endpoint: unix:///run/containerd/containerd.sock
image-endpoint: unix:///run/containerd/containerd.sock
timeout: 10
EOF
```

### 2. Install CNI plugins

```bash
CNI_VERSION="v1.6.1"
sudo mkdir -p /opt/cni/bin
wget -q "https://github.com/containernetworking/plugins/releases/download/$CNI_VERSION/cni-plugins-linux-amd64-$CNI_VERSION.tgz"
sudo tar -C /opt/cni/bin -xzf "cni-plugins-linux-amd64-$CNI_VERSION.tgz"

# Create a basic bridge network config
sudo mkdir -p /etc/cni/net.d
sudo tee /etc/cni/net.d/10-bridge.conflist > /dev/null <<EOF
{
  "cniVersion": "1.0.0",
  "name": "bridge",
  "plugins": [
    {
      "type": "bridge",
      "bridge": "cni0",
      "isGateway": true,
      "ipMasq": true,
      "ipam": { "type": "host-local", "ranges": [[{"subnet": "10.88.0.0/16"}]] }
    },
    { "type": "portmap", "capabilities": {"portMappings": true} },
    { "type": "loopback" }
  ]
}
EOF
```


```bash
# Cloud Hypervisor
CH_VERSION="v44.0"
wget -q "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$CH_VERSION/cloud-hypervisor-static" \
  -O /tmp/cloud-hypervisor
sudo install -m 755 /tmp/cloud-hypervisor /usr/local/bin/cloud-hypervisor

# Or from the containerd-cloudhypervisor installer image (see example/aks/)
```

### 4. Build and install the shim

```bash
# Build from source
cargo build --release -p containerd-shim-cloudhv
sudo install -m 755 target/release/containerd-shim-cloudhv-v1 /usr/local/bin/

# Build the guest kernel
cd guest/kernel && bash build-kernel.sh && cd ../..

# Build the guest rootfs (downloads static crun automatically)
cd guest/rootfs && sudo bash build-rootfs.sh ../../target/x86_64-unknown-linux-musl/release/cloudhv-agent && cd ../..

# Install guest artifacts
sudo mkdir -p /opt/cloudhv
sudo cp guest/kernel/vmlinux /opt/cloudhv/vmlinux
sudo cp guest/rootfs/rootfs.ext4 /opt/cloudhv/rootfs.ext4
```

### 5. Configure the runtime

```bash
# Create runtime config
sudo tee /opt/cloudhv/config.json > /dev/null <<EOF
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.ext4",
  "kernel_args": "console=hvc0 root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "max_containers_per_vm": 5,
  "tpm_enabled": false
}
EOF

# Register the cloudhv runtime with containerd
# Add this to the END of /etc/containerd/config.toml:
cat <<EOF | sudo tee -a /etc/containerd/config.toml

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"
EOF

# Restart containerd to pick up the new runtime
sudo systemctl restart containerd
```

## Running Containers

### Pull an image

```bash
sudo crictl pull hashicorp/http-echo:latest
```

### Create a pod sandbox

The sandbox is the VM. When you create it, Cloud Hypervisor boots a microVM
with its own kernel, and the guest agent starts as PID 1.

```bash
# Write the pod sandbox config
cat > /tmp/sandbox.json <<EOF
{
  "metadata": {
    "name": "echo-pod",
    "namespace": "default",
    "attempt": 1,
    "uid": "echo-pod-uid"
  },
  "log_directory": "/tmp/echo-pod-logs",
  "linux": {}
}
EOF

mkdir -p /tmp/echo-pod-logs

# Create the sandbox (boots the VM)
POD_ID=$(sudo crictl runp --runtime=cloudhv /tmp/sandbox.json)
echo "Pod sandbox created: $POD_ID"
```

At this point, a Cloud Hypervisor VM is running with:
- A custom Linux kernel (~27 MB)
- The cloudhv-agent as PID 1
- A vsock connection back to the shim for control plane RPCs
- A TAP device with the pod's IP address (assigned by CNI)

Check the pod IP:

```bash
POD_IP=$(sudo crictl inspectp $POD_ID | python3 -c \
  "import sys,json; print(json.load(sys.stdin)['status']['network']['ip'])")
echo "Pod IP: $POD_IP"
```

### Start an HTTP echo container

```bash
cat > /tmp/echo-container.json <<EOF
{
  "metadata": { "name": "echo-server" },
  "image": { "image": "hashicorp/http-echo:latest" },
  "command": ["/http-echo", "-text=hello from Cloud Hypervisor microVM!", "-listen=:5678"],
  "log_path": "echo.log"
}
EOF

# Create and start the container (hot-plugs a virtio-blk disk into the VM)
CTR_ID=$(sudo crictl create $POD_ID /tmp/echo-container.json /tmp/sandbox.json)
sudo crictl start $CTR_ID
echo "Container started: $CTR_ID"
```

Behind the scenes:
1. The shim creates an ext4 disk image from the container's rootfs
2. Hot-plugs it into the running VM via Cloud Hypervisor's `vm.add-disk` API
3. The guest agent discovers the new `/dev/vdX` device and mounts it
4. `crun` runs the container with mount + PID namespace isolation

### Verify it works

```bash
# Wait a moment for the server to start
sleep 2

# Curl the echo server from the host
curl http://$POD_IP:5678/
# Output: hello from Cloud Hypervisor microVM!
```

### Check container logs

```bash
sudo crictl logs $CTR_ID
```

### Add a second container to the same pod

Multiple containers share the same VM (and network namespace), just like
a Kubernetes pod:

```bash
sudo crictl pull nginx:alpine

cat > /tmp/nginx-container.json <<EOF
{
  "metadata": { "name": "nginx" },
  "image": { "image": "nginx:alpine" },
  "log_path": "nginx.log"
}
EOF

CTR2_ID=$(sudo crictl create $POD_ID /tmp/nginx-container.json /tmp/sandbox.json)
sudo crictl start $CTR2_ID

sleep 2
curl http://$POD_IP:80/
# Output: nginx welcome page
```

Both containers are running inside the same VM, isolated from each other
with mount + PID namespaces, but sharing the VM's network.

### Inspect the pod

```bash
# List running pods
sudo crictl pods

# List containers in the pod
sudo crictl ps

# Inspect the pod sandbox
sudo crictl inspectp $POD_ID
```

### Clean up

```bash
# Stop and remove containers
sudo crictl stop $CTR_ID $CTR2_ID
sudo crictl rm $CTR_ID $CTR2_ID

# Stop and remove the pod (shuts down the VM)
sudo crictl stopp $POD_ID
sudo crictl rmp $POD_ID
```

## What's Happening Under the Hood

```text
crictl runp sandbox.json
  → containerd invokes containerd-shim-cloudhv-v1
    → shim reads /opt/cloudhv/config.json
    → shim creates TAP device in pod network namespace
    → shim sets up TC redirect (veth ↔ TAP)
    → shim starts cloud-hypervisor inside the pod netns
    → shim calls vm.create + vm.boot via CH HTTP API
    → kernel boots, agent starts, vsock connects back
    → shim reports sandbox ready to containerd

crictl create $POD container.json sandbox.json
  → shim creates ext4 disk image from container rootfs
  → shim calls vm.add-disk to hot-plug into the VM
  → agent discovers /dev/vdX, mounts it
  → agent runs crun with the OCI spec
  → shim forwards container stdout/stderr to containerd

crictl stopp $POD
  → shim sends SIGTERM to all containers
  → shim calls vm.shutdown via CH API
  → shim kills CH process, cleans up state directory
```

## Troubleshooting

**VM doesn't boot**: Check that `/dev/kvm` exists and is accessible.
Verify the kernel and rootfs paths in `/opt/cloudhv/config.json`.

**Container create fails**: Check `journalctl -u containerd` for shim errors.
The guest agent logs are in the containerd log (infrastructure errors) and
container logs are in the pod log directory.

**Networking doesn't work**: Ensure CNI plugins are installed at `/opt/cni/bin`
and a network config exists at `/etc/cni/net.d/`. The shim needs `ip`, `tc`,
and `nsenter` commands available on the host.

**curl times out**: Verify the pod IP with `sudo crictl inspectp $POD_ID`.
Check that `net.ifnames=0` is in the kernel_args config (required for
the kernel IP_PNP parameter to configure eth0).
