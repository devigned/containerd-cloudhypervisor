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
- **Transparent vCPU sizing** — VM vCPUs match the pod's CPU limit; no limit = host CPU count

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

### System extension for Flatcar and similar container OSes

A self-contained system extension image is shipped with each [release](releases/); there's a Butane snippet included with the release notes for provisioning the extension.
The general pattern is
```
variant: flatcar
version: 1.0.0

storage:
  files:
  - path: /etc/extensions/containerd-cloudhypervisor.raw
    mode: 0644
    contents:
      source: https://github.com/devigned/containerd-cloudhypervisor/releases/download/<release-version>/containerd-cloudhypervisor-<release-version>-x86-64.raw
```

The sysext includes a brief demo to verify if the system is working. Run
```shell
root@flatcar $ /usr/share/cloudhv/demo/demo.sh
```
to verify.

#### Test your builds locally in a Flatcar VM

Sysext integration makes it easy to build the repository and run it locally in a Flatcar VM.

First, build the sysext.
This build is containerised and has no host dependencies (except Docker).
```
bash hacks/build-sysext.sh
```

For local testing, we'll leverage the [`boot` feature](https://github.com/flatcar/sysext-bakery?tab=readme-ov-file#interactively-test-extension-images-in-a-local-vm)
of Flatcar's [sysext bakery](https://github.com/flatcar/sysext-bakery).

1. Check out the bakery repo into a separate directory:
   ```
   git clone --depth 1 https://github.com/flatcar/sysext-bakery.git
   ```
2. Copy `containerd-cloudhypervisor.raw` into the bakery repo root; change into the bakery repo root.
3. Run
   ```
   ./bakery.sh boot containerd-cloudhypervisor.raw
   ```

This will download the latest Flatcar Alpha release for qemu, then start a Flatcar VM in ephemeral mode (no changes will be persisted in the Flatcar OS image).
`bakery.sh boot` will also launch a local Python webserver and generate transient Ignition configuration to provision `containerd-cloudhypervisor.raw` at boot time.

After the VM boot finished, you'll end up on the VM's serial port.
Run the demo included with the extension image to verify:
```bash
sudo /usr/share/cloudhv/demo/demo.sh
```

You can also connect to the local VM via ssh, using the `core` user:
```bash
ssh -p 2222 core@localhost
```

### Manual installation

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
  "max_default_vcpus": 0,
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
