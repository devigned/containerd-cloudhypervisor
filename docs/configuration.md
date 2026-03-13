# Configuration

## Runtime Config (`/opt/cloudhv/config.json`)

The shim loads its configuration from `/opt/cloudhv/config.json` at startup.

| Field | Default | Description |
|-------|---------|-------------|
| `cloud_hypervisor_binary` | `/usr/local/bin/cloud-hypervisor` | Path to CH binary |
| `kernel_path` | — | Path to guest vmlinux |
| `rootfs_path` | — | Path to guest rootfs.ext4 |
| `kernel_args` | `console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0` | Guest kernel cmdline (see [Architecture Notes](#architecture-notes)) |
| `default_vcpus` | `1` | Boot vCPUs per VM |
| `default_memory_mb` | `128` | Boot memory in MiB |
| `pool_size` | `2` | Pre-warmed VM pool size (0 = disabled) |
| `max_containers_per_vm` | `5` | Max containers sharing a VM |
| `hotplug_memory_mb` | `0` | Hotpluggable memory (0 = disabled) |
| `hotplug_method` | `acpi` | `acpi` or `virtio-mem` |
| `tpm_enabled` | `false` | Enable TPM 2.0 via swtpm |

### Example

```json
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.ext4",
  "kernel_args": "console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "pool_size": 2,
  "max_containers_per_vm": 5,
  "tpm_enabled": false
}
```

### Notes

- `net.ifnames=0` in `kernel_args` is **required for networking**. It forces classic
  interface naming (`eth0`) so the kernel IP_PNP parameter can configure the correct
  device at boot.

#### Architecture Notes

- The default `kernel_args` console device is set at compile time based on the target
  architecture. On **x86_64** it defaults to `console=hvc0` (virtio console); on
  **ARM64 (aarch64)** it defaults to `console=ttyAMA0` (PL011 UART). If you override
  `kernel_args` in the config file, use the correct console for your architecture.
- The kernel config used to build the guest kernel also differs per architecture:
  `guest/kernel/configs/microvm.config` for x86_64 and
  `guest/kernel/configs/microvm-aarch64.config` for ARM64.
- `pool_size` controls how many VMs are pre-booted and kept ready. Set to `0` to
  disable pooling (every pod gets a cold-booted VM).
- `max_containers_per_vm` limits density. Each container gets its own hot-plugged
  disk and mount + PID namespace isolation within the shared VM.

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
