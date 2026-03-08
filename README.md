# containerd-cloudhypervisor

A purpose-built [containerd](https://containerd.io/) shim for [Cloud Hypervisor](https://www.cloudhypervisor.org/)
that runs container workloads inside lightweight microVMs with maximum density and minimal memory overhead.

## Why This Shim?

### When to Choose containerd-cloudhypervisor

Choose this shim when you are building a **platform** where you control the stack and need:
- **Maximum density** — 105ms boot, ~25 MB overhead per VM, sub-second container start from pool
- **Minimal trusted computing base** — shim (2.4 MB) + agent (1.5 MB), no shell, no package manager in guest
- **Dual hypervisor support** — same binary runs on KVM (Linux) and MSHV (Azure/Hyper-V) with zero code changes
- **Tight VMM control** — direct Cloud Hypervisor API integration, configurable hotplug, virtio-mem, TPM

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
|---|---|---|
| **Boot time** | 105ms (KVM), 178ms (MSHV) | ~500ms–1s |
| **Shim binary** | 2.4 MB | ~50 MB |
| **Agent binary** | 1.5 MB (static) | ~20 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Guest rootfs** | 18 MB (agent + runc only) | ~150 MB |
| **TCB** | Minimal | Full Linux userspace |
| **Language** | Rust | Go |
| **Complexity** | Purpose-built, simple | Feature-rich, complex |

## Performance

Measured on Azure D2s_v5 (KVM) and D16as_v6 (MSHV):

| Metric | KVM | MSHV |
|--------|-----|------|
| Guest boot + agent ready | **105ms** | **178ms** |
| VM config serialization | 1.4μs | 1.4μs |
| Image cache hit | 140ns | 140ns |
| vCPU hotplug resize | ✅ | ✅ |
| Pool acquire (pre-warmed) | O(1) | O(1) |
| Integration tests (13) | 5.9s | 6.8s |

## Architecture

```
containerd ──ttrpc──► containerd-shim-cloudhv-v1 ──HTTP/UDS──► cloud-hypervisor
                                                                     │
                                                              ┌──────▼──────┐
                                                              │  Guest VM   │
                                                              │  ┌────────┐ │
                                              ttrpc/vsock ◄───┤  │ Agent  │ │
                                                              │  └───┬────┘ │
                                                              │  ┌───▼────┐ │
                                                              │  │ runc   │ │
                                                              │  └────────┘ │
                                                              └─────────────┘
```

- **Host shim** (`containerd-shim-cloudhv-v1`): Implements containerd shim v2 protocol, manages
  Cloud Hypervisor VM lifecycle via REST API, communicates with the guest agent over vsock/ttrpc.
- **Guest agent** (`cloudhv-agent`): Runs as PID 1 (init) inside the VM, receives container
  lifecycle commands over ttrpc/vsock, delegates to runc for container execution.
- **Communication**: vsock + ttrpc — no network stack required for the control plane.
- **Storage**: virtio-fs with shared memory — avoids guest page cache duplication.
- **Kernel**: Minimal custom kernel (~23 MB) with PVH boot, virtio, vsock, overlayfs, cgroups v2.

## Features

### Core
- **Dual hypervisor backend**: KVM and MSHV — Cloud Hypervisor auto-selects at runtime
- **VM pooling**: Pre-warmed VMs for instant container start
- **CPU/Memory hotplug**: Dynamic resource allocation via `vm.resize` API
- **virtio-mem**: Sub-block memory granularity for tight packing
- **File-based I/O proxy**: Container stdout/stderr via virtio-fs shared directory
- **Multi-container per VM**: Amortized VM overhead with reference-counted cleanup

### Security
- **TPM 2.0**: swtpm integration for measured boot
- **Minimal guest**: No shell, no package manager — agent + runc only
- **VmManager Drop cleanup**: Processes killed and state removed even on panic

### Operations
- **Live migration**: Zero-downtime VMM upgrades via `send_migration`/`receive_migration`
- **Snapshot/restore**: Pause → save state → restore with optional config changes
- **OCI image layer cache**: Reference-counted shared layers, deduplication across VMs
- **Hypervisor detection**: Logged on startup for operational visibility

## Crates

| Crate | Description |
|-------|-------------|
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
- `virtiofsd` (for virtio-fs)

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
cat > /etc/containerd/cloudhv-runtime.json << EOF
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "virtiofsd_binary": "/usr/libexec/virtiofsd",
  "kernel_path": "$PWD/guest/kernel/vmlinux",
  "rootfs_path": "$PWD/guest/rootfs/rootfs.ext4",
  "kernel_args": "console=hvc0 root=/dev/vda rw quiet init=/init",
  "default_vcpus": 1,
  "default_memory_mb": 128,
  "pool_size": 0,
  "tpm_enabled": false
}
EOF

# 5. Install the shim binary where containerd can find it
sudo install -m 755 target/release/containerd-shim-cloudhv-v1 /usr/local/bin/

# 6. Run the integration tests
sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture --test-threads=1
```

### Configuration

Runtime configuration is loaded from `/etc/containerd/cloudhv-runtime.json`:

| Field | Default | Description |
|-------|---------|-------------|
| `cloud_hypervisor_binary` | `/usr/local/bin/cloud-hypervisor` | Path to CH binary |
| `virtiofsd_binary` | `/usr/libexec/virtiofsd` | Path to virtiofsd |
| `kernel_path` | — | Path to guest vmlinux |
| `rootfs_path` | — | Path to guest rootfs.ext4 |
| `kernel_args` | `console=hvc0 root=/dev/vda rw quiet init=/init` | Guest kernel cmdline |
| `default_vcpus` | `1` | Boot vCPUs per VM |
| `default_memory_mb` | `128` | Boot memory in MiB |
| `pool_size` | `0` | Pre-warmed VM pool size (0 = disabled) |
| `max_containers_per_vm` | `1` | Max containers sharing a VM |
| `hotplug_memory_mb` | `0` | Hotpluggable memory (0 = disabled) |
| `hotplug_method` | `acpi` | `acpi` or `virtio-mem` |
| `tpm_enabled` | `false` | Enable TPM 2.0 via swtpm |

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
