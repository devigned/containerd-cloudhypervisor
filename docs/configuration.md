# Configuration

## Runtime Config (`/opt/cloudhv/config.json`)

The shim loads its configuration from `/opt/cloudhv/config.json` at startup.

| Field | Default | Description |
|-------|---------|-------------|
| `cloud_hypervisor_binary` | `/usr/local/bin/cloud-hypervisor` | Path to CH binary |
| `kernel_path` | — | Path to guest vmlinux |
| `rootfs_path` | — | Path to guest rootfs.erofs |
| `kernel_args` | `console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0` | Guest kernel cmdline |
| `default_vcpus` | `1` | Boot vCPUs per VM |
| `max_default_vcpus` | `0` | Max vCPUs when no CPU limit (0 = host CPU count) |
| `default_memory_mb` | `128` | Boot memory in MiB |
| `max_containers_per_vm` | `5` | Max containers sharing a VM |
| `hotplug_memory_mb` | `0` | Hotpluggable memory (0 = disabled) |
| `hotplug_method` | `acpi` | `acpi` or `virtio-mem` |
| `tpm_enabled` | `false` | Enable TPM 2.0 via swtpm |
| `daemon_socket` | `""` | Path to daemon Unix socket (required for daemon mode) |

### Example

```json
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 128,
  "max_containers_per_vm": 5,
  "tpm_enabled": false,
  "daemon_socket": "/run/cloudhv/daemon.sock"
}
```

### Daemon Socket

When `daemon_socket` points to a valid Unix socket (e.g., `/run/cloudhv/daemon.sock`),
the shim delegates VM lifecycle to the sandbox daemon instead of spawning Cloud
Hypervisor directly. The shim becomes a thin client that handles only TAP networking,
erofs conversion, and daemon RPCs.

When `daemon_socket` is empty (default), the shim falls back to direct CH spawn —
the daemon is purely additive.

## Daemon Configuration (`/opt/cloudhv/daemon.json`)

The sandbox daemon loads its configuration from `/opt/cloudhv/daemon.json`.

| Field | Default | Description |
|-------|---------|-------------|
| `pool_size` | `3` | Target number of pre-booted idle VMs |
| `max_pool_size` | `10` | Hard cap on idle pool VMs |
| `default_vcpus` | `1` | vCPUs per pool VM |
| `default_memory_mb` | `128` | Memory per pool VM in MiB |
| `cloud_hypervisor_binary` | `/usr/local/bin/cloud-hypervisor` | Path to CH binary (requires v51+) |
| `kernel_path` | — | Path to guest vmlinux |
| `rootfs_path` | — | Path to guest rootfs.erofs |
| `kernel_args` | `console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0` | Guest kernel cmdline |
| `socket_path` | `/run/cloudhv/daemon.sock` | Unix socket for shim communication |
| `state_dir` | `/run/cloudhv/daemon` | Base snapshot, pool state, warm snapshots |
| `warmup_duration_secs` | `30` | Shadow VM warmup before snapshot |
| `max_snapshots` | `100` | Max cached warm workload snapshots |

### Example

```json
{
  "pool_size": 3,
  "max_pool_size": 10,
  "default_vcpus": 1,
  "default_memory_mb": 128,
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0",
  "socket_path": "/run/cloudhv/daemon.sock",
  "state_dir": "/run/cloudhv/daemon",
  "warmup_duration_secs": 30,
  "max_snapshots": 100
}
```

### Systemd Unit

The daemon runs as a systemd service at `/etc/systemd/system/cloudhv-sandbox-daemon.service`:

```ini
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
```

The installer creates this unit automatically. `MemoryMax=4G` caps total daemon
memory including pool VMs.

### Cache Directories

| Directory | Contents |
|-----------|----------|
| `/run/cloudhv/erofs-cache/` | Content-addressed erofs images (flock-serialized) |
| `/run/cloudhv/daemon/` | Base snapshot, pool VM state, warm workload snapshots |

### Notes

- `net.ifnames=0` in `kernel_args` is **required for networking**. It forces classic
  interface naming (`eth0`) so the kernel IP_PNP parameter can configure the correct
  device at boot.

#### Architecture Notes

- **Console device:** The example above uses `console=ttyS0`, which is correct for
  Cloud Hypervisor's serial port output and is what the installer writes. The compile-time
  default in code is `console=hvc0` for historical reasons (virtio-console mode), but
  `ttyS0` is the recommended setting for production and is required for `cloud-hypervisor
  --serial tty` output. On **ARM64 (aarch64)** use `console=ttyAMA0` (PL011 UART).
  If you override `kernel_args` in the config file, use the correct console for your
  architecture.
- The kernel config used to build the guest kernel also differs per architecture:
  `guest/kernel/configs/microvm.config` for x86_64 and
  `guest/kernel/configs/microvm-aarch64.config` for ARM64.
- `max_containers_per_vm` limits density. Each container gets its own hot-plugged
  disk and mount + PID namespace isolation within the shared VM.

## Rootfs Image Cache

The shim caches rootfs ext4 images per unique container image to eliminate
`mkfs.ext4` from the container startup hot path.

| Item | Value |
|------|-------|
| **Cache directory** | `/opt/cloudhv/cache/` |
| **Cache key** | FNV-1a hash of rootfs file metadata (path, size, mode) |
| **File naming** | `/opt/cloudhv/cache/<hash>.img` |
| **Lifetime** | Persistent until manual deletion |
| **Typical size** | 64–500 MB per unique image |

The installer creates the cache directory automatically. To clear:

```bash
sudo rm -f /opt/cloudhv/cache/*.img
```

> **Note:** Only clear the cache when no containers are actively starting.

## Pod Annotations

VM resources can be overridden per-pod using OCI spec annotations. This allows
different pods to request different memory/vCPU allocations without changing the
global runtime config.

### Dual-Prefix Resolution

The shim accepts annotations from two prefixes:

| Prefix | Priority | Purpose |
|--------|----------|---------|
| `io.cloudhv.` | **Primary** — always wins if present | Native namespace |
| `io.katacontainers.` | **Fallback** — used if no `io.cloudhv.` equivalent | Kata migration compatibility |

If both prefixes specify the same setting, `io.cloudhv.` takes precedence. This allows
Kata Containers users to migrate without changing their pod annotations.

### Supported Annotations

| Annotation Suffix | Type | Description | Validation |
|-------------------|------|-------------|------------|
| `config.hypervisor.default_memory` | u64 (MiB) | VM boot memory | min 128 MiB |
| `config.hypervisor.memory_limit` | u64 (MiB) | Max memory (hotplug ceiling) | must be > default_memory |
| `config.hypervisor.default_vcpus` | u32 | VM vCPU count | must be > 0 |
| `config.hypervisor.default_max_vcpus` | u32 | Max vCPUs for hotplug | must be ≥ default_vcpus |
| `config.hypervisor.kernel_params` | string | Extra kernel boot params | appended to config |
| `config.hypervisor.enable_virtio_mem` | bool | Use virtio-mem hotplug | `true`/`false` |

Invalid values are logged as warnings and ignored (the config default is preserved).

### Examples

#### Kubernetes Pod Spec

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: memory-intensive-app
  annotations:
    # Request 2GB memory and 4 vCPUs for this pod's VM
    io.cloudhv.config.hypervisor.default_memory: "2048"
    io.cloudhv.config.hypervisor.default_vcpus: "4"
spec:
  runtimeClassName: cloudhv
  containers:
    - name: app
      image: myapp:latest
```

#### Kata-Compatible Annotations

```yaml
annotations:
  # These work too (Kata migration path)
  io.katacontainers.config.hypervisor.default_memory: "1024"
  io.katacontainers.config.hypervisor.default_vcpus: "2"
```

#### Precedence When Both Present

```yaml
annotations:
  io.katacontainers.config.hypervisor.default_memory: "1024"  # ignored
  io.cloudhv.config.hypervisor.default_memory: "4096"          # ← wins
```

#### Extra Kernel Parameters

```yaml
annotations:
  io.cloudhv.config.hypervisor.kernel_params: "quiet loglevel=0"
```

### crictl Usage

With `crictl`, annotations are set in the pod sandbox config:

```json
{
  "metadata": { "name": "my-pod", "namespace": "default", "uid": "my-uid" },
  "annotations": {
    "io.cloudhv.config.hypervisor.default_memory": "2048",
    "io.cloudhv.config.hypervisor.default_vcpus": "4"
  },
  "log_directory": "/tmp/my-pod-logs",
  "linux": {}
}
```

## containerd Registration

Add the cloudhv runtime to your containerd config (`/etc/containerd/config.toml`):

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"
```

Then restart containerd:

```bash
sudo systemctl restart containerd
```
