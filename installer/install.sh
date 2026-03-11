#!/usr/bin/env bash
set -euo pipefail

# This script runs inside the DaemonSet installer pod with the host
# filesystem mounted at /host. It copies the shim artifacts onto the
# node, patches containerd to register the cloudhv runtime, and
# restarts containerd.

ARTIFACTS=/opt/cloudhv
HOST=/host

echo "[cloudhv] Installing on $(cat /host/etc/hostname)..."

# 1. Copy binaries
echo "[cloudhv] Copying shim binary..."
install -D -m 755 "$ARTIFACTS/containerd-shim-cloudhv-v1" "$HOST/usr/local/bin/containerd-shim-cloudhv-v1"

# 2. Copy virtiofsd
echo "[cloudhv] Copying virtiofsd..."
install -D -m 755 "$ARTIFACTS/virtiofsd" "$HOST/usr/libexec/virtiofsd"

# 2. Copy guest artifacts
echo "[cloudhv] Copying guest kernel and rootfs..."
mkdir -p "$HOST/opt/cloudhv"
install -m 644 "$ARTIFACTS/vmlinux" "$HOST/opt/cloudhv/vmlinux"
install -m 644 "$ARTIFACTS/rootfs.ext4" "$HOST/opt/cloudhv/rootfs.ext4"

# 3. Write runtime config
echo "[cloudhv] Writing runtime config..."
cat > "$HOST/opt/cloudhv/config.json" << 'CONFIG'
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "virtiofsd_binary": "/usr/libexec/virtiofsd",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.ext4",
  "kernel_args": "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "pool_size": 2,
  "max_containers_per_vm": 5,
  "hotplug_memory_mb": 128,
  "hotplug_method": "virtio-mem",
  "tpm_enabled": false
}
CONFIG

# 4. Patch containerd config to add cloudhv runtime handler
echo "[cloudhv] Patching containerd config..."
CONTAINERD_CONFIG="$HOST/etc/containerd/config.toml"

if [ ! -f "$CONTAINERD_CONFIG" ]; then
  echo "[cloudhv] WARNING: $CONTAINERD_CONFIG not found, skipping patch"
else
  # Backup
  cp "$CONTAINERD_CONFIG" "${CONTAINERD_CONFIG}.bak.$(date +%s)"

  # Remove any stale cloudhv config (BinaryName/ConfigPath cause cgroup errors)
  if grep -q "containerd-shim-cloudhv-v1" "$CONTAINERD_CONFIG"; then
    echo "[cloudhv] Removing stale runtime handler config"
    python3 -c "
lines = open('$CONTAINERD_CONFIG').readlines()
out = []
skip = False
for line in lines:
    if 'runtimes.cloudhv' in line:
        skip = True
        continue
    if skip and (line.strip().startswith('runtime_type') or line.strip().startswith('BinaryName') or line.strip().startswith('ConfigPath') or line.strip().startswith('[plugins') and 'cloudhv.options' in line or line.strip() == ''):
        continue
    skip = False
    out.append(line)
open('$CONTAINERD_CONFIG','w').writelines(out)
" 2>/dev/null || true
  fi

  # Add clean cloudhv runtime handler
  cat >> "$CONTAINERD_CONFIG" << 'TOML'

# Cloud Hypervisor VM-isolated runtime
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"
TOML
    echo "[cloudhv] Runtime handler added to containerd config"
fi

# 5. Install cloud-hypervisor and virtiofsd if not present
if [ ! -f "$HOST/usr/local/bin/cloud-hypervisor" ]; then
  echo "[cloudhv] Installing cloud-hypervisor..."
  CH_VERSION="v44.0"
  curl -sL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/cloud-hypervisor-static" \
    -o "$HOST/usr/local/bin/cloud-hypervisor"
  chmod 755 "$HOST/usr/local/bin/cloud-hypervisor"
fi

# 6. Restart containerd to pick up the new runtime
echo "[cloudhv] Restarting containerd..."
chroot "$HOST" systemctl restart containerd
sleep 5

# 7. Verify
if chroot "$HOST" systemctl is-active --quiet containerd; then
  echo "[cloudhv] containerd restarted successfully"
else
  echo "[cloudhv] ERROR: containerd failed to restart"
  exit 1
fi

# 8. Label node as ready
NODE_NAME=$(cat "$HOST/etc/hostname")
echo "[cloudhv] Installation complete on $NODE_NAME"

# Keep running so the DaemonSet stays healthy
echo "[cloudhv] Installer idle. Shim is active."
exec sleep infinity
