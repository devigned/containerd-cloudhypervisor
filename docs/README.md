# Documentation

## Table of Contents

| Document | Description |
|----------|-------------|
| [Architecture](architecture.md) | System design, rootfs delivery (devmapper passthrough + ext4 cache), inline metadata, networking, logging |
| [Configuration](configuration.md) | Runtime config reference, cache management, pod annotations |
| [Performance](performance.md) | Benchmarks, cache hit/miss latencies, resource overhead |
| [Development](development.md) | Building, testing, contributing, code quality standards |
| [Releasing](releasing.md) | Release process, Helm chart, GHCR artifacts |

## Overview

containerd-cloudhypervisor is a purpose-built [containerd](https://containerd.io/) shim
that runs container workloads inside [Cloud Hypervisor](https://www.cloudhypervisor.org/)
microVMs. Each pod gets its own VM with a dedicated kernel, providing strong isolation
without the overhead of a full-featured VMM stack.

### Design Principles

- **Minimal trusted computing base** — the guest contains only the agent binary (1.5 MB)
  and crun (1.8 MB). No shell, no package manager, no libc.
- **Block device rootfs** — container images are delivered as hot-plugged virtio-blk disks,
  avoiding FUSE in the data path. Rootfs images are cached per unique container image
  at `/opt/cloudhv/cache/` to eliminate `mkfs.ext4` from the hot path.
- **Kernel-level networking** — the guest kernel configures its own IP via boot parameters
  (IP_PNP). No agent-side networking code.
- **Infrastructure logs separate from workload logs** — shim errors go to containerd's log
  (`journalctl`); container stdout/stderr goes to `kubectl logs`.
- **Every feature has an integration test** — if there's no test proving it works e2e,
  we assume it doesn't work.

### Crates

| Crate | Description |
|-------|-------------|
| `crates/shim` | Host shim binary (`containerd-shim-cloudhv-v1`) |
| `crates/agent` | Guest agent binary (`cloudhv-agent`) |
| `crates/proto` | Protobuf/ttrpc service definitions (auto-generated) |
| `crates/common` | Shared types, errors, configuration, and constants |

### Examples

| Example | Description |
|---------|-------------|
| [`example/crictl/`](../example/crictl/) | Run containers on bare Linux with crictl (no Kubernetes) |
| [`example/aks/`](../example/aks/) | Deploy on Azure Kubernetes Service with DaemonSet installer |
