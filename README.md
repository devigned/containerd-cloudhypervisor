# containerd-cloudhypervisor

A [containerd](https://containerd.io/) shim for [Cloud Hypervisor](https://www.cloudhypervisor.org/)
that runs container workloads inside lightweight microVMs with maximum density and minimal memory overhead.

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

- **Host shim** (`containerd-shim-cloudhv-v1`): Implements the containerd shim v2 protocol,
  manages Cloud Hypervisor VM lifecycle, communicates with the in-VM agent over vsock.
- **Guest agent** (`cloudhv-agent`): Runs as PID 1 inside the VM, receives container lifecycle
  commands over ttrpc/vsock, delegates to runc for actual container execution.
- **Communication**: vsock + ttrpc (minimal overhead, no network stack required for control plane).
- **Storage**: virtio-fs with shared memory (avoids guest page cache duplication for density).
- **Kernel**: Minimal custom kernel based on Cloud Hypervisor's `ch_defconfig`.

## Features

- **Dual hypervisor backend**: KVM (Linux) and MSHV (Azure/Hyper-V)
- **Minimal memory footprint**: ~25-40 MB total overhead per VM
- **Fast boot**: < 200ms with minimal kernel and rootfs
- **Multi-container per VM**: Amortize VM overhead across containers
- **virtio-fs**: Memory-efficient container image delivery

## Crates

| Crate | Description |
|-------|-------------|
| `crates/shim` | containerd shim binary (`containerd-shim-cloudhv-v1`) |
| `crates/agent` | In-VM guest agent binary (`cloudhv-agent`) |
| `crates/proto` | Protobuf/ttrpc service definitions (shared) |
| `crates/common` | Shared types, errors, and constants |

## Building

```bash
# Build host shim (native)
cargo build --release -p containerd-shim-cloudhv

# Build guest agent (static musl binary for minimal rootfs)
cargo build --release -p cloudhv-agent --target x86_64-unknown-linux-musl

# Build everything
make build
```

## Prerequisites

- Rust toolchain (stable)
- `protobuf-compiler` (`protoc`)
- `x86_64-unknown-linux-musl` target (for guest agent)
- Linux host with KVM or MSHV support (for running VMs)
- Cloud Hypervisor binary
- `virtiofsd` (for virtio-fs support)

## License

MIT — see [LICENSE](LICENSE).
