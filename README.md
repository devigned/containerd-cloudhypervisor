# containerd-cloudhypervisor

A purpose-built [containerd](https://containerd.io/) shim for [Cloud Hypervisor](https://www.cloudhypervisor.org/)
that runs container workloads inside lightweight microVMs with maximum density and minimal memory overhead.

## Highlights

- **~530ms end-to-end container lifecycle (single pod via crictl)**
- **VM isolation** — each pod runs in its own Cloud Hypervisor microVM with dedicated kernel
- **Rootfs delivery** — devmapper block passthrough (zero-copy) with ext4 cache fallback
- **Block device rootfs** — container images delivered as hot-plugged virtio-blk disks; devmapper snapshots passed directly, overlayfs cached as ext4
- **Dual hypervisor** — same binary runs on KVM (Linux) and MSHV (Azure/Hyper-V)
- **Multi-container pods** — up to 5 containers per VM with mount + PID isolation
- **Pod networking** — transparent CNI integration via TAP + TC redirect
- **Kata-compatible annotations** — per-pod memory/vCPU sizing with `io.cloudhv.*` or `io.katacontainers.*`

## When to Use

Choose this shim when you're building a **platform** where you control the stack and need
VM isolation without the overhead of a full-featured VMM stack. Ideal for serverless/FaaS
platforms, container-as-a-service offerings, and security-sensitive workloads.

For general-purpose Kubernetes with multi-hypervisor support, GPU passthrough, or live
migration, consider [Kata Containers](https://katacontainers.io/) instead.

| | containerd-cloudhypervisor | Kata Containers |
| --- | --- | --- |
| **Cold start** | ~530ms (crictl) | ~500ms–1s |
| **Shim binary** | 2.4 MB | ~50 MB |
| **Guest rootfs** | 16 MB (agent + crun) | ~150 MB |
| **Language** | Rust | Go |

## Quick Start

```bash
# Build
cargo build --release -p containerd-shim-cloudhv
cargo build --release -p cloudhv-agent --target x86_64-unknown-linux-musl
cd guest/kernel && bash build-kernel.sh && cd ../..
cd guest/rootfs && sudo bash build-rootfs.sh ../../target/x86_64-unknown-linux-musl/release/cloudhv-agent && cd ../..

# Install
sudo install -m 755 target/release/containerd-shim-cloudhv-v1 /usr/local/bin/
sudo mkdir -p /opt/cloudhv /opt/cloudhv/cache
sudo cp guest/kernel/vmlinux guest/rootfs/rootfs.ext4 /opt/cloudhv/

# Configure (see docs/configuration.md for full reference)
sudo tee /opt/cloudhv/config.json > /dev/null <<EOF
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.ext4",
  "kernel_args": "console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "max_containers_per_vm": 5
}
EOF

# Test
sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture --test-threads=1
```

## Documentation

See the **[docs/](docs/)** folder for detailed documentation:

- **[Architecture](docs/architecture.md)** — system design, rootfs caching, inline metadata, networking
- **[Configuration](docs/configuration.md)** — runtime config reference, cache management, pod annotations
- **[Performance](docs/performance.md)** — benchmarks, cache hit/miss latencies, resource overhead
- **[Development](docs/development.md)** — building, testing, contributing, code quality standards

## Examples

- **[Bare Linux with crictl](example/crictl/)** — run containers with crictl, no Kubernetes required
- **[Azure Kubernetes Service](example/aks/)** — deploy on AKS with DaemonSet installer

## License

MIT — see [LICENSE](LICENSE).
