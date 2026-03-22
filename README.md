# containerd-cloudhypervisor

A purpose-built [containerd](https://containerd.io/) shim for [Cloud Hypervisor](https://www.cloudhypervisor.org/)
that runs container workloads inside lightweight microVMs with maximum density and minimal memory overhead.

## Highlights

- **Sandbox daemon** — long-running systemd daemon pre-boots a pool of VMs from a base snapshot via CH v51 OnDemand restore (~25ms each, ~6 MB idle RSS). Shadow VMs create warm workload snapshots in the background — no production pod pausing
- **150/150 pods in 11s** on 3 × D8ds_v5 nodes (96 GiB total RAM), ~24 MB per VM, 10% node memory utilization
- **Thin shim** — the containerd shim (~1,180 lines) handles TAP networking, erofs conversion, and daemon RPCs (`AcquireSandbox`, `AddContainer`, `ReleaseSandbox`)
- **VM isolation** — each pod runs in its own Cloud Hypervisor microVM with dedicated kernel
- **erofs rootfs cache** — content-addressable, flock-serialized, shared across pods
- **inotify device discovery** — hot-plugged container disks detected in <1ms via inotify (no polling)
- **Pure libc networking** — TAP/tc setup via in-process netlink (<1ms, no subprocess)
- **Dual hypervisor** — same binary runs on KVM (Linux) and MSHV (Azure/Hyper-V)
- **Multi-container pods** — up to 5 containers per VM with mount + PID isolation
- **Pod networking** — transparent CNI integration via TAP + TC redirect
- **Kata-compatible annotations** — per-pod memory/vCPU sizing with `io.cloudhv.*` or `io.katacontainers.*`
- **Transparent vCPU sizing** — VM vCPUs match the pod's CPU limit; no limit = host CPU count

## When to Use

Choose this shim when you're building a **platform** where you control the stack and need
VM isolation without the overhead of a full-featured VMM stack. Ideal for AI agent sandboxes,
serverless/FaaS platforms, and security-sensitive workloads where density matters.

For general-purpose Kubernetes with multi-hypervisor support, GPU passthrough, or live
migration, consider [Kata Containers](https://katacontainers.io/) instead.

| | containerd-cloudhypervisor | Kata Containers |
| --- | --- | --- |
| **Cold start (shim inner)** | ~74ms | ~500ms–1s |
| **Warm restore** | ~168ms | N/A |
| **Memory per pod** | ~24 MB (OnDemand CoW) | ~330 MB |
| **150-pod scale** | 150/150 in 11s | 130/150 (OOM) |
| **Shim binary** | 4.6 MB | ~50 MB |
| **Guest rootfs** | 5.4 MB (agent + crun, erofs) | ~150 MB |
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
cargo build --release -p cloudhv-sandbox-daemon
cargo build --release -p cloudhv-agent --target x86_64-unknown-linux-musl
cd guest/kernel && bash build-kernel.sh && cd ../..
cd guest/rootfs && sudo bash build-rootfs.sh ../../target/x86_64-unknown-linux-musl/release/cloudhv-agent && cd ../..

# Install binaries
sudo install -m 755 target/release/containerd-shim-cloudhv-v1 /usr/local/bin/
sudo install -m 755 target/release/cloudhv-sandbox-daemon /usr/local/bin/
sudo mkdir -p /opt/cloudhv /run/cloudhv/erofs-cache /run/cloudhv/daemon
sudo cp guest/kernel/vmlinux guest/rootfs/rootfs.ext4 /opt/cloudhv/

# Shim config (see docs/configuration.md for full reference)
sudo tee /opt/cloudhv/config.json > /dev/null <<EOF
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "max_default_vcpus": 0,
  "default_memory_mb": 512,
  "max_containers_per_vm": 5,
  "daemon_socket": "/run/cloudhv/daemon.sock"
}
EOF

# Daemon config
sudo tee /opt/cloudhv/daemon.json > /dev/null <<EOF
{
  "pool_size": 3,
  "max_pool_size": 10,
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0",
  "socket_path": "/run/cloudhv/daemon.sock",
  "state_dir": "/run/cloudhv/daemon",
  "warmup_duration_secs": 30,
  "max_snapshots": 100
}
EOF

# Systemd unit for the daemon
sudo tee /etc/systemd/system/cloudhv-sandbox-daemon.service > /dev/null <<EOF
[Unit]
Description=CloudHV Sandbox Daemon
After=containerd.service
Requires=containerd.service

[Service]
Type=simple
ExecStartPre=/bin/mkdir -p /run/cloudhv/daemon
ExecStart=/usr/local/bin/cloudhv-sandbox-daemon /opt/cloudhv/daemon.json
Restart=always
RestartSec=5
Environment=RUST_LOG=info
MemoryMax=4G

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now cloudhv-sandbox-daemon
```

## Documentation

See the **[docs/](docs/)** folder for detailed documentation:

- **[Architecture](docs/architecture.md)** — system design, daemon + shim + agent components, networking
- **[Sandbox Daemon](docs/sandbox-daemon.md)** — daemon design, VM pool, shadow snapshots, benchmark results
- **[Configuration](docs/configuration.md)** — shim and daemon config reference, pod annotations
- **[Performance](docs/performance.md)** — benchmarks, latency breakdown, density, comparison with Kata
- **[Development](docs/development.md)** — building, testing, contributing, code quality standards
- **[Releasing](docs/releasing.md)** — release workflow, published artifacts, installation

## Examples

- **[Bare Linux with crictl](example/crictl/)** — run containers with crictl, no Kubernetes required
- **[Azure Kubernetes Service](example/aks/)** — deploy on AKS with DaemonSet installer

## License

MIT — see [LICENSE](LICENSE).
