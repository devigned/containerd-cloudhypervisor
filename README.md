# containerd-cloudhypervisor

A purpose-built [containerd](https://containerd.io/) shim for [Cloud Hypervisor](https://www.cloudhypervisor.org/)
that runs container workloads inside lightweight microVMs with maximum density and minimal memory overhead.

## Why This Shim?

### When to Choose containerd-cloudhypervisor

Choose this shim when you are building a **platform** where you control the stack and need:

- **Fast cold start** — ~460ms sandbox boot + container start on 8-vCPU host
- **VM isolation** — each pod runs in its own Cloud Hypervisor microVM with dedicated kernel
- **Block device rootfs** — container images delivered as hot-plugged virtio-blk disks (no FUSE)
- **Dual hypervisor support** — same binary runs on KVM (Linux) and MSHV (Azure/Hyper-V)
- **Multi-container pods** — mount + PID namespace isolation within the VM
- **Pod networking** — transparent CNI integration via TAP + TC redirect

This is ideal for serverless/FaaS platforms, container-as-a-service offerings, and security-sensitive
workloads where VM isolation is required but Kata's full feature set is unnecessary overhead.

### When to Choose Kata Containers Instead

Choose [Kata Containers](https://katacontainers.io/) when:

- You need **multi-hypervisor flexibility** (swap between Cloud Hypervisor, QEMU, Firecracker)
- You want **mature Kubernetes integration** (RuntimeClass, annotations, full CRI support)
- You need **advanced features**: GPU passthrough, live migration (QEMU), full CSI support
- **Community support** and security patching cadence matter
- You are running a general-purpose Kubernetes platform, not a purpose-built service

### Key Differences

| | containerd-cloudhypervisor | Kata Containers |
| --- | --- | --- |
| **Cold start** | ~460ms (VM boot + container start) | ~500ms–1s |
| **Shim binary** | 2.4 MB | ~50 MB |
| **Agent binary** | 1.5 MB (static) | ~20 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Guest rootfs** | 16 MB (agent + crun only) | ~150 MB |
| **TCB** | Minimal | Full Linux userspace |
| **Language** | Rust | Go |
| **Complexity** | Purpose-built, simple | Feature-rich, complex |

## Performance

Measured on Azure D8s_v5 (KVM, 8 vCPU):

| Phase | Latency |
| ------- | --------- |
| VM boot (sandbox) | **257ms** |
| Container create (disk image + hot-plug) | **58ms** |
| Container start (crun run) | **145ms** |
| **Total cold start** | **~460ms** |
| Sandbox stop + cleanup | **92ms** |

| Resource | Overhead |
| ---------- | ---------- |
| Cloud Hypervisor (VMM) | ~50 MB RSS |
| virtiofsd | ~5 MB RSS |
| Shim process | ~10 MB RSS |
| VM guest memory | 512 MB (configurable) |

## Architecture

```text
                     ┌─────────────────────────────────────────────────────────┐
                     │  Pod Network Namespace                                  │
containerd           │                                                         │
   │                 │  ┌──────┐  TC redirect  ┌──────┐                        │
   │ ttrpc           │  │ veth ├──────────────►│ TAP  │                        │
   │                 │  │(eth0)│◄──────────────┤      │                        │
   ▼                 │  └───┬──┘               └──┬───┘                        │
┌──────────────┐     │      │                     │                            │
│  shim-v1     ├─────┤      │ IP flushed          │ virtio-net                 │
│              │     │      │ (VM owns pod IP)    │                            │
│  • disk img  │     │  ┌───┴─────────────────────┴────────────────────────┐   │
│  • hot-plug  │     │  │  cloud-hypervisor (VMM)                          │   │
│  • logs      │     │  │  ┌─────────────────────────────────────────────┐ │   │
│              │     │  │  │  Guest VM (custom kernel)                   │ │   │
└──────┬───────┘     │  │  │                                             │ │   │
       │ vsock       │  │  │  eth0 ← kernel ip= (IP_PNP at boot)         │ │   │
       │             │  │  │                                             │ │   │
       │             │  │  │  ┌───────────┐     ┌──────┐                 │ │   │
       └─────────────┤  │  │  │   Agent   │────►│ crun │ (containers)    │ │   │
                     │  │  │  │  (PID 1)  │     └──────┘                 │ │   │
                     │  │  │  └───────────┘                              │ │   │
                     │  │  └─────────────────────────────────────────────┘ │   │
                     │  └──────────────────────────────────────────────────┘   │
                     └─────────────────────────────────────────────────────────┘
```

### Sandbox and Container Split

The shim uses the `io.kubernetes.cri.container-type` annotation to distinguish between
sandbox creation and application containers:

- **Sandbox** (`container-type=sandbox`): boots the Cloud Hypervisor VM. The VM **is** the
  sandbox — no pause container needed. Networking (TAP + TC redirect) is set up at this stage.
- **App container** (`container-type=container`): creates an ext4 disk image from the container
  rootfs, hot-plugs it into the running VM, and the guest agent runs it with crun.

### Container Rootfs via Block Devices

Following the same approach as [firecracker-containerd](https://github.com/firecracker-microvm/firecracker-containerd):

1. Host shim mounts the container rootfs from containerd's overlayfs snapshot
2. Creates an ext4 disk image containing the OCI bundle + rootfs
3. Hot-plugs the disk into the running VM via Cloud Hypervisor's `vm.add-disk` API
4. Guest agent discovers the new `/dev/vdX` block device and mounts it
5. `crun` runs the container with mount + PID namespaces on the real ext4 filesystem

This avoids FUSE in the container data path and enables proper `pivot_root`.

### Networking

VM networking follows the [tc-redirect-tap](https://github.com/firecracker-microvm/firecracker-containerd)
pattern used by firecracker-containerd, adapted for Cloud Hypervisor:

1. **TAP creation**: the shim creates a TAP device inside the pod's network namespace
2. **TC redirect**: bidirectional `tc filter` rules redirect all traffic between the
   CNI veth and the TAP device at layer 2
3. **IP flush**: the pod IP is removed from the veth so packets traverse TC into the VM
4. **Kernel IP_PNP**: the pod IP, gateway, and netmask are passed as a kernel boot
   parameter (`ip=<addr>::<gw>:<mask>::eth0:off`), so the guest kernel configures the
   interface at boot — no agent-side networking code needed
5. **CH in netns**: Cloud Hypervisor is launched inside the pod network namespace
   (via `nsenter`) so it can access the TAP device

The result is that the VM's `eth0` has the pod IP and responds to traffic on the pod
network, fully transparent to CNI and Kubernetes services.

### Container Logs

Container stdout/stderr flows from the guest to `kubectl logs` without any agent-side
log infrastructure:

1. `crun` inside the VM writes stdout/stderr to files on the virtio-fs shared directory
2. The host shim tails these files and forwards lines to containerd's stdio FIFOs
3. containerd delivers them as standard container logs (`crictl logs`, `kubectl logs`)

Infrastructure errors (VM boot failures, API errors, disk hot-plug issues) are logged
via the shim's own logger and appear in the containerd log (`journalctl -u containerd`),
keeping operator diagnostics separate from application output.

### Components

- **Host shim** (`containerd-shim-cloudhv-v1`): containerd shim v2, manages VM lifecycle,
  creates disk images, hot-plugs block devices, sets up networking, forwards logs.
- **Guest agent** (`cloudhv-agent`): PID 1 in the VM, discovers hot-plugged disks, adapts
  OCI specs, delegates to crun.
- **Communication**: vsock + ttrpc — no network stack for the control plane.
- **Container runtime**: crun (1.5 MB static) — lighter than runc (10 MB).
- **Kernel**: Custom kernel (~27 MB) with PVH boot, virtio, vsock, BPF, ACPI hot-plug,
  IP_PNP, and virtio-net.

## Features

### Core

- **Dual hypervisor backend**: KVM and MSHV — Cloud Hypervisor auto-selects at runtime
- **VM pooling**: Pre-warmed VMs for instant container start
- **CPU/Memory hotplug**: Dynamic resource allocation via `vm.resize` API
- **virtio-mem**: Sub-block memory granularity for tight packing
- **Block device rootfs**: Hot-plugged virtio-blk disks via CH `vm.add-disk` API
- **Multi-container per VM**: Mount + PID namespace isolation per container
- **Pod networking**: TAP + TC redirect with kernel IP_PNP — zero guest userspace networking

### Security

- **TPM 2.0**: swtpm integration for measured boot
- **Minimal guest**: No shell, no package manager — agent + crun only
- **VmManager Drop cleanup**: Processes killed and state removed even on panic

### Operations

- **Container logs**: Forwarded from guest to containerd FIFOs (`kubectl logs` works)
- **Hypervisor detection**: Logged on startup for operational visibility

## Crates

| Crate | Description |
| ------- | ------------- |
| `crates/shim` | Host shim binary (`containerd-shim-cloudhv-v1`) |
| `crates/agent` | Guest agent binary (`cloudhv-agent`) |
| `crates/proto` | Protobuf/ttrpc service definitions (auto-generated) |
| `crates/common` | Shared types, errors, configuration, and constants |

## Getting Started (KVM)

### Prerequisites

- Linux host with KVM support (`/dev/kvm`)
- Rust toolchain (stable)
- `protobuf-compiler` (`protoc`)
- Cloud Hypervisor (`cloud-hypervisor` binary)
- `virtiofsd` (for virtio-fs shared directory)

### Quick Start

```bash
# 1. Build the shim and agent
cargo build --release -p containerd-shim-cloudhv
cargo build --release -p cloudhv-agent --target x86_64-unknown-linux-musl

# 2. Build the guest kernel
cd guest/kernel
bash build-kernel.sh    # Downloads Linux 6.12.8, applies minimal config, builds vmlinux
cd ../..

# 3. Build the guest rootfs
cd guest/rootfs
sudo bash build-rootfs.sh ../../target/x86_64-unknown-linux-musl/release/cloudhv-agent
cd ../..

# 4. Create a runtime config
sudo mkdir -p /opt/cloudhv
cat | sudo tee /opt/cloudhv/config.json << EOF
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "virtiofsd_binary": "/usr/libexec/virtiofsd",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.ext4",
  "kernel_args": "console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 128,
  "pool_size": 0,
  "tpm_enabled": false
}
EOF

# 5. Copy artifacts and install the shim binary
sudo cp guest/kernel/vmlinux /opt/cloudhv/vmlinux
sudo cp guest/rootfs/rootfs.ext4 /opt/cloudhv/rootfs.ext4
sudo install -m 755 target/release/containerd-shim-cloudhv-v1 /usr/local/bin/

# 6. Run the integration tests
sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture --test-threads=1
```

### Configuration

Runtime configuration is loaded from `/opt/cloudhv/config.json`:

| Field | Default | Description |
| ------- | --------- | ------------- |
| `cloud_hypervisor_binary` | `/usr/local/bin/cloud-hypervisor` | Path to CH binary |
| `virtiofsd_binary` | `/usr/libexec/virtiofsd` | Path to virtiofsd |
| `kernel_path` | — | Path to guest vmlinux |
| `rootfs_path` | — | Path to guest rootfs.ext4 |
| `kernel_args` | `console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0` | Guest kernel cmdline |
| `default_vcpus` | `1` | Boot vCPUs per VM |
| `default_memory_mb` | `128` | Boot memory in MiB |
| `pool_size` | `0` | Pre-warmed VM pool size (0 = disabled) |
| `max_containers_per_vm` | `1` | Max containers sharing a VM |
| `hotplug_memory_mb` | `0` | Hotpluggable memory (0 = disabled) |
| `hotplug_method` | `acpi` | `acpi` or `virtio-mem` |
| `tpm_enabled` | `false` | Enable TPM 2.0 via swtpm |

> **Note**: `net.ifnames=0` in `kernel_args` is required for networking. It forces
> classic interface naming (`eth0`) so the kernel IP_PNP parameter can configure the
> correct device at boot.

## Development

### Building

```bash
make build          # Build shim (native) + agent (musl static)
make build-shim     # Build shim only
make build-agent    # Build agent only (static musl)
make fmt            # Format code
make clippy         # Run clippy
make test           # Run unit tests
```

### Remote Development (macOS → Linux VM)

```bash
# Sync code to a Linux VM and build/test remotely
make sync REMOTE_HOST=user@host
make remote-build REMOTE_HOST=user@host
make remote-test REMOTE_HOST=user@host
make remote-integration REMOTE_HOST=user@host
```

### Running Benchmarks

```bash
# Criterion micro-benchmarks (image cache, config serialization, CID allocation)
cargo bench -p containerd-shim-cloudhv --bench vm_overhead

# Integration timing benchmark (requires KVM or MSHV + sudo)
sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture test_vm_lifecycle_timing
```

### AKS Example

See [`example/aks/`](example/aks/) for a complete example of running VM-isolated containers on Azure Kubernetes Service. The example includes:

- DaemonSet installer that deploys the shim, kernel, rootfs, and cloud-hypervisor onto AKS nodes
- RuntimeClass configuration for the `cloudhv` runtime
- Setup/teardown scripts for AKS cluster provisioning

```bash
# Quick test on an AKS cluster with the cloudhv runtime installed:
kubectl run test --image=busybox:latest --restart=Never --runtime-class=cloudhv -- echo hello
kubectl logs test
kubectl delete pod test
```

## Contributing

1. **Fork and clone** the repository
2. **Set up a Linux VM** with KVM for testing (Azure D-series VMs with nested virt work well)
3. **Build and test** on both macOS (compile check) and Linux (integration tests):

   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --workspace
   ```

4. **Integration tests require root** (for `/run/cloudhv/` and Cloud Hypervisor):

   ```bash
   sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture --test-threads=1
   ```

5. **Submit a PR** — CI runs lint, build (gnu + musl), unit tests, and integration tests with KVM

### Code Quality Standards

- `cargo clippy -- -D warnings` with **zero suppressions** — no `#[allow(dead_code)]` or `#[allow(unused_imports)]`
- Tests must **never false-pass** — use `.expect()`, not silent skip-on-error
- VMs must **always clean up** — `VmManager` implements `Drop` to prevent zombie processes
- Verify on **both macOS and Linux** before pushing

## License

MIT — see [LICENSE](LICENSE).
