# Development

## Building

```bash
make build          # Build shim (native) + agent (musl static)
make build-shim     # Build shim only
make build-agent    # Build agent only (static musl)
make fmt            # Format code
make clippy         # Run clippy
make test           # Run unit tests

# Requires libseccomp-dev and libcap-ng-dev on Linux
```

### Project Structure

The project uses two Cargo workspaces:

- **Root workspace** (`Cargo.toml`): shim, proto, and common crates.
  Uses ttrpc 0.8 via [shimkit](https://github.com/containerd/runwasi).
- **Agent workspace** (`crates/agent/Cargo.toml`): standalone guest agent binary.
  Uses ttrpc 0.9 for vsock server support.

The split is required because the shim (via shimkit/containerd-shim 0.8) needs
protobuf 3.2, while the agent needs protobuf 3.7+ for ttrpc 0.9's vsock API.
Both binaries are protocol-compatible — the shim's ttrpc 0.8 client communicates
with the agent's ttrpc 0.9 server over vsock.

## Prerequisites

- **Rust** (stable toolchain)
- **protobuf-compiler** (`protoc`) — for ttrpc code generation
- **Linux with KVM** — for integration tests (`/dev/kvm`)
- **Cloud Hypervisor** — VMM binary

## Guest Artifacts

### Kernel

```bash
cd guest/kernel
bash build-kernel.sh    # Downloads Linux 6.12.8, applies minimal config, builds vmlinux
```

The kernel config (`guest/kernel/configs/microvm.config`) includes only what's needed:
PVH boot, virtio (blk, net, vsock, fs), BPF/cgroup v2, ACPI hot-plug, IP_PNP.

For ARM64 builds, the script auto-detects `aarch64` and uses
`guest/kernel/configs/microvm-aarch64.config` instead, which replaces PVH boot with
direct kernel boot, uses PL011 serial (`SERIAL_AMBA_PL011`) instead of 8250, and
enables the ARM GIC interrupt controller.

### Rootfs

```bash
cd guest/rootfs
sudo bash build-rootfs.sh path/to/cloudhv-agent
```

The rootfs contains only the agent binary (as `/init`) and a static crun binary.
No shell, no busybox, no package manager — absolute minimum for running containers.

## Testing

### Unit Tests

```bash
cargo test --workspace           # shim, proto, common
cd crates/agent && cargo test    # agent
```

### Integration Tests

Integration tests boot real VMs and require root + KVM:

```bash
sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture --test-threads=1
```

Set environment variables to override default paths:

| Variable | Default | Description |
|----------|---------|-------------|
| `CLOUDHV_TEST_KERNEL` | `guest/kernel/vmlinux` | Path to guest kernel |
| `CLOUDHV_TEST_ROOTFS` | `guest/rootfs/rootfs.ext4` | Path to guest rootfs |
| `CLOUDHV_TEST_CH_BIN` | `/usr/local/bin/cloud-hypervisor` | Path to CH binary |
| `CLOUDHV_TEST_HTTP_ECHO` | `/usr/local/bin/http-echo` | Path to http-echo binary |

Tests that require `http-echo` (container networking, multi-container, e2e benchmark)
skip gracefully if the binary is not available.

### Benchmarks

```bash
# Criterion micro-benchmarks (image cache, config serialization, CID allocation)
cargo bench -p containerd-shim-cloudhv --bench vm_overhead

# Integration timing benchmark (requires KVM + sudo)
sudo cargo test -p containerd-shim-cloudhv --test integration -- --nocapture test_vm_lifecycle_timing
```

## Remote Development (macOS → Linux VM)

Build and test on a remote Linux VM from macOS:

```bash
make sync REMOTE_HOST=user@host
make remote-build REMOTE_HOST=user@host
make remote-test REMOTE_HOST=user@host
make remote-integration REMOTE_HOST=user@host
```

## ARM64 Builds

> **⚠️ ARM64 support is experimental.** GitHub's ARM64 runners (`ubuntu-24.04-arm`)
> do not expose `/dev/kvm`
> ([actions/partner-runner-images#147](https://github.com/actions/partner-runner-images/issues/147)),
> so ARM64 integration tests are **skipped in CI**. Only builds, linting, and unit
> tests are validated automatically. Integration testing on ARM64 must be done
> manually on a KVM-capable ARM64 host (e.g., an Ampere Altra bare-metal instance
> or an Azure Dpsv6 VM with nested virtualization).

The project supports ARM64 (aarch64) natively. The same `make build` and `cargo build`
commands work on ARM64 hosts — architecture is auto-detected at build time.

Key differences on ARM64:

- **Target triple**: `aarch64-unknown-linux-musl` (agent), `aarch64-unknown-linux-gnu` (shim)
- **Console device**: the shim compiles with `console=ttyAMA0` (PL011 UART) instead of `hvc0`
- **Guest kernel config**: `build-kernel.sh` selects `guest/kernel/configs/microvm-aarch64.config`
  automatically on aarch64 hosts
- **CI runners**: ARM64 CI jobs run on `ubuntu-24.04-arm` runners (builds only — no KVM)
- **Cloud Hypervisor**: requires the `cloud-hypervisor-static-aarch64` binary

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
   on x86_64. ARM64 builds are validated but integration tests are skipped (no KVM on ARM runners)

## Code Quality Standards

- `cargo clippy -- -D warnings` — no suppressed warnings in production code
- Tests must **never false-pass** — use `.expect()`, not silent skip-on-error
- VMs must **always clean up** — `VmManager` implements `Drop` to prevent zombie processes
- Verify on **both macOS and Linux** before pushing
- Every feature must have an **integration test** proving it works end-to-end
