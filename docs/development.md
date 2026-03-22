# Development

## Building

```bash
make build          # Build shim (native) + agent (musl static)
make build-shim     # Build shim only
make build-agent    # Build agent only (static musl)
make fmt            # Format code
make clippy         # Run clippy
make test           # Run unit tests

# Build the daemon
cargo build --release -p cloudhv-sandbox-daemon

# Requires libseccomp-dev and libcap-ng-dev on Linux
```

### Project Structure

The project has 3 main crates in the root workspace, plus the agent as a separate workspace:

- **Root workspace** (`Cargo.toml`): shim, daemon, proto, and common crates.
  Uses ttrpc 0.8 via [shimkit](https://github.com/containerd/runwasi).
  - `crates/shim` — containerd shim v2 (`containerd-shim-cloudhv-v1`)
  - `crates/daemon` — sandbox daemon (`cloudhv-sandbox-daemon`)
  - `crates/proto` — protobuf/ttrpc service definitions
  - `crates/common` — shared types, constants, error handling, netlink utilities
- **Agent workspace** (`crates/agent/Cargo.toml`): standalone guest agent binary.
  Uses ttrpc 0.9 for vsock server support.

The split is required because the shim (via shimkit/containerd-shim 0.8) needs
protobuf 3.2, while the agent needs protobuf 3.7+ for ttrpc 0.9's vsock API.
Both binaries are protocol-compatible — the shim's ttrpc 0.8 client communicates
with the agent's ttrpc 0.9 server over vsock.

The agent is excluded from the root workspace (`exclude = ["crates/agent"]`)
and must be built and checked separately:

```bash
cd crates/agent && cargo check
cd crates/agent && cargo test
```

## Prerequisites

- **Rust** (stable toolchain)
- **protobuf-compiler** (`protoc`) — for ttrpc code generation
- **Linux with KVM** — for integration tests (`/dev/kvm`)
- **Cloud Hypervisor >= v51.0** — required for daemon OnDemand restore (build from source until released: [PR #7800](https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7800))

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
cargo test --workspace           # shim, daemon, proto, common
cd crates/agent && cargo test    # agent (separate workspace)
```

### Daemon Integration Tests

The daemon has integration tests that exercise the full VM lifecycle (requires root + KVM):

```bash
cd crates/daemon && cargo test
```

### Benchmarks

```bash
# Criterion micro-benchmarks (image cache, config serialization, CID allocation)
cargo bench -p containerd-shim-cloudhv --bench vm_overhead
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

4. **Daemon integration tests require root + KVM** (for Cloud Hypervisor and `/run/cloudhv/`):

   ```bash
   cd crates/daemon && cargo test
   ```

5. **Submit a PR** — CI runs lint, build (gnu + musl), unit tests, and integration tests with KVM
   on x86_64. ARM64 builds are validated but integration tests are skipped (no KVM on ARM runners)

## Code Quality Standards

- `cargo clippy -- -D warnings` — no suppressed warnings in production code
- Tests must **never false-pass** — use `.expect()`, not silent skip-on-error
- VMs must **always clean up** — the daemon destroys CH processes on release to prevent zombie VMs
- Verify on **both macOS and Linux** before pushing
- Every feature must have an **integration test** proving it works end-to-end
