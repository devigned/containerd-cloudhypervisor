# Releasing

## Overview

Releases are fully automated via GitHub Actions. Pushing a semantic version tag
triggers the release workflow, which builds all artifacts, publishes OCI images
to GHCR, and creates a GitHub Release with auto-generated notes.

## Creating a Release

```bash
# Tag the release
git tag v0.1.0
git push origin v0.1.0
```

The tag must match `v<major>.<minor>.<patch>` (e.g., `v0.1.0`, `v1.2.3`).
Pre-release tags like `v0.2.0-beta.1` are also supported.

## What Gets Published

### GitHub Release

The release page at `github.com/devigned/containerd-cloudhypervisor/releases`
contains downloadable binaries and checksums:

| Asset | Description |
|-------|-------------|
| `containerd-shim-cloudhv-v1-linux-amd64` | Host shim binary |
| `cloudhv-agent-linux-amd64` | Guest agent (static musl) |
| `vmlinux` | Guest kernel (PVH boot, virtio, vsock, IP_PNP) |
| `rootfs.ext4` | Guest rootfs (agent + crun, 16 MB) |
| `cloudhv-installer-chart-<version>.tgz` | Helm chart archive |
| `checksums-sha256.txt` | SHA-256 checksums for all assets |

### GHCR OCI Artifacts

| Image | Purpose |
|-------|---------|
| `ghcr.io/devigned/cloudhv-installer:<tag>` | DaemonSet installer image |
| `ghcr.io/devigned/charts/cloudhv-installer:<version>` | Helm chart (OCI) |

Both are tagged with the release version and `latest`.

## Release Notes

Release notes are auto-generated from conventional commit messages between
the current tag and the previous tag. Commits are grouped by type:

| Prefix | Section |
|--------|---------|
| `feat:` | Features |
| `fix:` | Bug Fixes |
| `perf:` | Performance |
| `docs:` | Documentation |
| `test:` | Tests |
| `build:` | Build |
| `chore:` | Maintenance |

The notes also include installation instructions for binaries, Helm, and
the container image.

## Installing from a Release

### Bare Linux (binaries)

Download from the GitHub Release page:

```bash
VERSION="v0.1.0"
BASE="https://github.com/devigned/containerd-cloudhypervisor/releases/download/$VERSION"

# Download and install
wget "$BASE/containerd-shim-cloudhv-v1-linux-amd64"
wget "$BASE/vmlinux"
wget "$BASE/rootfs.ext4"
wget "$BASE/checksums-sha256.txt"

# Verify checksums
sha256sum -c checksums-sha256.txt

# Install
sudo install -m 755 containerd-shim-cloudhv-v1-linux-amd64 /usr/local/bin/containerd-shim-cloudhv-v1
sudo mkdir -p /opt/cloudhv
sudo cp vmlinux rootfs.ext4 /opt/cloudhv/
```

Then create `/opt/cloudhv/config.json` — see [Configuration](configuration.md).

### Kubernetes (Helm chart from GHCR)

```bash
helm install cloudhv-installer \
  oci://ghcr.io/devigned/charts/cloudhv-installer \
  --version 0.1.0 \
  --namespace kube-system
```

Override values:

```bash
helm install cloudhv-installer \
  oci://ghcr.io/devigned/charts/cloudhv-installer \
  --version 0.1.0 \
  --namespace kube-system \
  --set nodeSelector.workload=my-pool \
  --set image.tag=v0.1.0
```

### Kubernetes (installer image directly)

If not using Helm, reference the installer image in your own DaemonSet:

```
ghcr.io/devigned/cloudhv-installer:v0.1.0
```

## Helm Chart

The chart is at `charts/cloudhv-installer/` and installs:

- **DaemonSet**: copies shim, kernel, rootfs, virtiofsd, and Cloud Hypervisor
  onto each selected node, patches containerd config, restarts containerd
- **RuntimeClass**: registers the `cloudhv` runtime handler with pod overhead
  annotations for accurate Kubernetes scheduler accounting

### Values

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` | `ghcr.io/devigned/cloudhv-installer` | Installer image |
| `image.tag` | Chart `appVersion` | Image tag |
| `image.pullPolicy` | `IfNotPresent` | Pull policy |
| `nodeSelector` | `workload: cloudhv` | Target nodes |
| `tolerations` | `[{operator: Exists}]` | Tolerate all taints |
| `resources.requests.cpu` | `100m` | Installer CPU request |
| `resources.requests.memory` | `128Mi` | Installer memory request |
| `runtimeClass.enabled` | `true` | Create RuntimeClass |
| `runtimeClass.name` | `cloudhv` | RuntimeClass name |
| `runtimeClass.overhead.memory` | `50Mi` | Pod overhead memory |
| `runtimeClass.overhead.cpu` | `50m` | Pod overhead CPU |

## Workflow Details

The release workflow (`.github/workflows/release.yml`) runs these jobs:

```
build-binaries ──┐
                 ├── build-installer-image ──┐
build-kernel ────┤                           ├── release
                 │                           │
build-rootfs ────┘                           │
  (needs build-binaries)                     │
                                             │
  (creates GitHub Release, pushes Helm to GHCR)
```

1. **build-binaries**: compiles shim (gnu) and agent (musl) in parallel
2. **build-kernel**: builds or restores cached guest kernel
3. **build-rootfs**: creates rootfs ext4 image with agent + crun
4. **build-installer-image**: packages everything into a container, pushes to GHCR
5. **release**: creates GitHub Release with binaries, checksums, Helm chart, and notes
