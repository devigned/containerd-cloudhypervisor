#!/usr/bin/env bash
set -uo pipefail

# CloudHV installer — runs inside the DaemonSet pod with the host
# filesystem mounted at /host. Copies shim artifacts, configures the
# erofs snapshotter, and restarts containerd.

ARTIFACTS=/opt/cloudhv
HOST=/host

HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64)  KERNEL_CONSOLE="console=ttyS0" ;;
    aarch64) KERNEL_CONSOLE="console=ttyAMA0" ;;
    *)
        echo "[cloudhv] ERROR: unsupported architecture: ${HOST_ARCH}"
        exit 1
        ;;
esac

echo "[cloudhv] Installing on $(cat /host/etc/hostname) (${HOST_ARCH})..."

# 1. Ensure tc (traffic control) is available for VM TAP networking
if ! nsenter --target 1 --mount -- sh -c 'command -v tc' >/dev/null 2>&1; then
  echo "[cloudhv] tc not found, installing iproute-tc..."
  cat > "$HOST/tmp/cloudhv-install-tc.sh" << 'TCEOF'
#!/bin/sh
if command -v tdnf >/dev/null 2>&1; then
  tdnf install -y iproute-tc
elif command -v dnf >/dev/null 2>&1; then
  dnf install -y iproute-tc
elif command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq && apt-get install -y iproute2
else
  echo "No supported package manager found" >&2
  exit 1
fi
TCEOF
  chmod +x "$HOST/tmp/cloudhv-install-tc.sh"
  if nsenter --target 1 --mount --uts --ipc --pid -- /tmp/cloudhv-install-tc.sh 2>&1 | tail -5; then
    echo "[cloudhv] iproute-tc installed"
  else
    echo "[cloudhv] ERROR: tc is required but could not be installed"
    rm -f "$HOST/tmp/cloudhv-install-tc.sh"
    exit 1
  fi
  rm -f "$HOST/tmp/cloudhv-install-tc.sh"
fi

# 2. Load erofs kernel module and install erofs-utils
nsenter --target 1 --mount -- modprobe erofs 2>/dev/null || true
if ! nsenter --target 1 --mount -- sh -c 'command -v mkfs.erofs' >/dev/null 2>&1; then
  echo "[cloudhv] mkfs.erofs not found, installing erofs-utils..."
  cat > "$HOST/tmp/cloudhv-install-erofs.sh" << 'EROFSEOF'
#!/bin/sh
if command -v tdnf >/dev/null 2>&1; then
  tdnf install -y erofs-utils
elif command -v dnf >/dev/null 2>&1; then
  dnf install -y erofs-utils
elif command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq && apt-get install -y erofs-utils
else
  echo "No supported package manager found" >&2
  exit 1
fi
EROFSEOF
  chmod +x "$HOST/tmp/cloudhv-install-erofs.sh"
  if nsenter --target 1 --mount --uts --ipc --pid -- /tmp/cloudhv-install-erofs.sh 2>&1 | tail -5; then
    echo "[cloudhv] erofs-utils installed"
  else
    echo "[cloudhv] ERROR: mkfs.erofs is required but could not be installed"
    rm -f "$HOST/tmp/cloudhv-install-erofs.sh"
    exit 1
  fi
  rm -f "$HOST/tmp/cloudhv-install-erofs.sh"
fi

# 3. Copy binaries and guest artifacts
echo "[cloudhv] Copying binaries and guest artifacts..."
install -D -m 755 "$ARTIFACTS/containerd-shim-cloudhv-v1" "$HOST/usr/local/bin/containerd-shim-cloudhv-v1"
install -m 755 "$ARTIFACTS/cloud-hypervisor" "$HOST/usr/local/bin/cloud-hypervisor"
mkdir -p "$HOST/opt/cloudhv"
install -m 644 "$ARTIFACTS/vmlinux" "$HOST/opt/cloudhv/vmlinux"
install -m 644 "$ARTIFACTS/rootfs.ext4" "$HOST/opt/cloudhv/rootfs.ext4"

# 4. Write runtime config
echo "[cloudhv] Writing runtime config..."
cat > "$HOST/opt/cloudhv/config.json" << CONFIG
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.ext4",
  "kernel_args": "${KERNEL_CONSOLE} root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 512,
  "max_containers_per_vm": 5,
  "hotplug_memory_mb": 0,
  "hotplug_method": "acpi",
  "tpm_enabled": false
}
CONFIG

# 5. Patch containerd config
echo "[cloudhv] Patching containerd config..."
CONTAINERD_CONFIG="$HOST/etc/containerd/config.toml"

if [ ! -f "$CONTAINERD_CONFIG" ]; then
  echo "[cloudhv] WARNING: $CONTAINERD_CONFIG not found, skipping"
else
  cp "$CONTAINERD_CONFIG" "${CONTAINERD_CONFIG}.bak.$(date +%s)"
  ORIG_MODE=$(stat -c '%a' "$CONTAINERD_CONFIG" 2>/dev/null || echo "644")

  # Remove existing cloudhv runtime sections (idempotent)
  awk '
    /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
    skip && /^\[/ && !/cloudhv/ { skip=0 }
    !skip { print }
  ' "$CONTAINERD_CONFIG" > "${CONTAINERD_CONFIG}.tmp" && mv "${CONTAINERD_CONFIG}.tmp" "$CONTAINERD_CONFIG"
  chmod "$ORIG_MODE" "$CONTAINERD_CONFIG" 2>/dev/null || true

  # Add erofs snapshotter config (if not already present)
  if ! grep -q 'snapshotter.v1.erofs' "$CONTAINERD_CONFIG" 2>/dev/null; then
    cat >> "$CONTAINERD_CONFIG" << 'EROFS'

# CloudHV erofs snapshotter for direct image layer passthrough
[plugins."io.containerd.snapshotter.v1.erofs"]
EROFS
    echo "[cloudhv] erofs snapshotter configured"
  fi

  # Add cloudhv runtime handler with erofs snapshotter
  {
    echo ""
    echo "# Cloud Hypervisor VM-isolated runtime"
    echo '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]'
    echo '  runtime_type = "io.containerd.cloudhv.v1"'
    echo '  snapshotter = "erofs"'
  } >> "$CONTAINERD_CONFIG"
  echo "[cloudhv] Runtime handler added"
fi

# 6. Restart containerd
echo "[cloudhv] Scheduling containerd restart..."
nsenter --target 1 --mount --uts --ipc --pid -- \
  bash -c 'nohup bash -c "sleep 5 && systemctl restart containerd" &>/dev/null &'

sleep 10
if chroot "$HOST" systemctl is-active --quiet containerd; then
  echo "[cloudhv] containerd restarted successfully"
else
  echo "[cloudhv] WARNING: containerd may still be restarting"
fi

echo "[cloudhv] Installation complete on $(cat $HOST/etc/hostname)"
echo "[cloudhv] Rootfs delivery: erofs layer passthrough"

# Keep running so the DaemonSet stays healthy
echo "[cloudhv] Installer idle. Shim is active."
exec sleep infinity
