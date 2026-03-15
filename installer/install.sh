#!/usr/bin/env bash
set -uo pipefail
# NOTE: intentionally NOT using set -e — devmapper setup has fallback paths

# This script runs inside the DaemonSet installer pod with the host
# filesystem mounted at /host. It copies the shim artifacts onto the
# node, sets up a devmapper thin pool for zero-copy rootfs delivery,
# patches containerd to register the cloudhv runtime with the devmapper
# snapshotter, and restarts containerd.

ARTIFACTS=/opt/cloudhv
HOST=/host

# Detect architecture
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64)
        KERNEL_CONSOLE="console=ttyS0"
        ;;
    aarch64)
        KERNEL_CONSOLE="console=ttyAMA0"
        ;;
    *)
        echo "[cloudhv] ERROR: unsupported architecture: ${HOST_ARCH}"
        exit 1
        ;;
esac

echo "[cloudhv] Installing on $(cat /host/etc/hostname) (${HOST_ARCH})..."

# 0. Ensure tc (traffic control) is available — required for VM TAP networking
if ! nsenter --target 1 --mount -- sh -c 'command -v tc' >/dev/null 2>&1; then
  echo "[cloudhv] tc not found, attempting to install iproute-tc..."
  if nsenter --target 1 --mount --uts --ipc --pid --cgroup -- tdnf install -y iproute-tc 2>&1 | tail -3; then
    echo "[cloudhv] iproute-tc installed"
  elif nsenter --target 1 --mount --uts --ipc --pid --cgroup -- dnf install -y iproute-tc 2>&1 | tail -3; then
    echo "[cloudhv] iproute-tc installed (dnf)"
  elif nsenter --target 1 --mount --uts --ipc --pid --cgroup -- sh -c 'apt-get update -qq && apt-get install -y iproute2' 2>&1 | tail -3; then
    echo "[cloudhv] iproute2 installed (apt)"
  else
    echo "[cloudhv] ERROR: tc (traffic control) is required but not found and could not be installed."
    echo "[cloudhv] ERROR: Install iproute-tc (AzureLinux/Fedora) or iproute2 (Debian/Ubuntu) on the host."
    exit 1
  fi
fi
echo "[cloudhv] tc available: $(nsenter --target 1 --mount -- sh -c 'command -v tc')"

# 1. Copy binaries
echo "[cloudhv] Copying shim binary..."
install -D -m 755 "$ARTIFACTS/containerd-shim-cloudhv-v1" "$HOST/usr/local/bin/containerd-shim-cloudhv-v1"


# 2. Copy guest artifacts
echo "[cloudhv] Copying guest kernel and rootfs..."
mkdir -p "$HOST/opt/cloudhv"
mkdir -p "$HOST/opt/cloudhv/cache"
install -m 644 "$ARTIFACTS/vmlinux" "$HOST/opt/cloudhv/vmlinux"
install -m 644 "$ARTIFACTS/rootfs.ext4" "$HOST/opt/cloudhv/rootfs.ext4"

# 3. Write runtime config
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

# 4. Set up devmapper thin pool for zero-copy rootfs delivery
echo "[cloudhv] Setting up devmapper thin pool..."
POOL_NAME="cloudhv-pool"
DM_DIR="$HOST/var/lib/containerd/devmapper"
POOL_READY=false

# Check if pool already exists
if nsenter --target 1 --mount -- dmsetup info "$POOL_NAME" 2>/dev/null | grep -q "ACTIVE"; then
  echo "[cloudhv] Thin pool $POOL_NAME already exists"
  POOL_READY=true
fi

if [ "$POOL_READY" = "false" ]; then
  mkdir -p "$DM_DIR"

  # Detect available block devices for the thin pool.
  # Priority: ephemeral/temp disk > resource disk > loopback sparse file
  BACKING_DEV=""

  # Azure VMs often have an ephemeral resource disk at /dev/sdb
  # Check for unpartitioned or unmounted block devices
  for DEV in /dev/sdb /dev/nvme1n1 /dev/vdb; do
    # Check in host mount namespace for accurate device/mount info
    if nsenter --target 1 --mount -- test -b "$DEV" && \
       ! nsenter --target 1 --mount -- findmnt -n -o TARGET "$DEV" >/dev/null 2>&1; then
      SIZE_BYTES=$(nsenter --target 1 --mount -- blockdev --getsize64 "$DEV" 2>/dev/null || echo 0)
      if [ "$SIZE_BYTES" -gt 10737418240 ]; then
        BACKING_DEV="$DEV"
        echo "[cloudhv] Found ephemeral disk: $DEV ($(( SIZE_BYTES / 1073741824 )) GiB)"
        break
      fi
    fi
  done

  if [ -n "$BACKING_DEV" ]; then
    # Use the real block device directly — best performance
    echo "[cloudhv] Creating thin pool on $BACKING_DEV..."

    # Partition: 95% for data, 5% for metadata
    TOTAL_SECTORS=$(blockdev --getsz "$BACKING_DEV")
    META_SECTORS=$(( TOTAL_SECTORS / 20 ))
    DATA_SECTORS=$(( TOTAL_SECTORS - META_SECTORS ))

    # Create data and metadata devices using dmsetup linear targets
    # Clean up any stale mappings first
    nsenter --target 1 --mount -- dmsetup remove "${POOL_NAME}-data" 2>/dev/null || true
    nsenter --target 1 --mount -- dmsetup remove "${POOL_NAME}-meta" 2>/dev/null || true

    if ! nsenter --target 1 --mount -- dmsetup create "${POOL_NAME}-data" \
      --table "0 $DATA_SECTORS linear $BACKING_DEV 0"; then
      echo "[cloudhv] WARNING: failed to create data mapping"
    elif ! nsenter --target 1 --mount -- dmsetup create "${POOL_NAME}-meta" \
      --table "0 $META_SECTORS linear $BACKING_DEV $DATA_SECTORS"; then
      echo "[cloudhv] WARNING: failed to create meta mapping"
      nsenter --target 1 --mount -- dmsetup remove "${POOL_NAME}-data" 2>/dev/null || true
    else
      # Zero metadata and create thin-pool
      nsenter --target 1 --mount -- dd if=/dev/zero of="/dev/mapper/${POOL_NAME}-meta" bs=4096 count=100 2>/dev/null
      if nsenter --target 1 --mount -- dmsetup create "$POOL_NAME" \
        --table "0 $DATA_SECTORS thin-pool /dev/mapper/${POOL_NAME}-meta /dev/mapper/${POOL_NAME}-data 128 32768 1 skip_block_zeroing"; then
        POOL_READY=true
      elif nsenter --target 1 --mount -- dmsetup create "$POOL_NAME" \
        --table "0 $DATA_SECTORS thin-pool /dev/mapper/${POOL_NAME}-meta /dev/mapper/${POOL_NAME}-data 128 32768"; then
        echo "[cloudhv] WARNING: skip_block_zeroing not supported, pool creation may be slow"
        POOL_READY=true
      else
        echo "[cloudhv] WARNING: thin-pool creation failed, cleaning up"
        nsenter --target 1 --mount -- dmsetup remove "${POOL_NAME}-meta" 2>/dev/null || true
        nsenter --target 1 --mount -- dmsetup remove "${POOL_NAME}-data" 2>/dev/null || true
      fi
    fi
  fi

  if [ "$POOL_READY" = "false" ]; then
    # Fallback: loopback sparse file (works everywhere, slightly lower performance)
    echo "[cloudhv] No ephemeral disk found, using loopback sparse file..."

    # Paths as seen from the container (for file creation via /host mount)
    DATA_FILE="$DM_DIR/data"
    META_FILE="$DM_DIR/meta"
    # Paths as seen from the host (for losetup/dmsetup via nsenter)
    HOST_DATA="/var/lib/containerd/devmapper/data"
    HOST_META="/var/lib/containerd/devmapper/meta"

    # Ensure directory exists on host and both files exist with correct sizes
    mkdir -p "$DM_DIR"
    if [ ! -f "$DATA_FILE" ] || [ ! -f "$META_FILE" ]; then
      rm -f "$DATA_FILE" "$META_FILE"
      truncate -s 10G "$DATA_FILE"
      truncate -s 1G "$META_FILE"
    fi

    # losetup runs in host mount namespace — must use host-side paths
    DATA_DEV=$(nsenter --target 1 --mount -- losetup --find --show "$HOST_DATA" 2>/dev/null || true)
    META_DEV=$(nsenter --target 1 --mount -- losetup --find --show "$HOST_META" 2>/dev/null || true)

    if [ -n "$DATA_DEV" ] && [ -n "$META_DEV" ]; then
      DATA_SIZE=$(nsenter --target 1 --mount -- blockdev --getsize64 "$DATA_DEV" 2>/dev/null || echo 0)
      if [ "$DATA_SIZE" -gt 0 ]; then
        LENGTH=$(( DATA_SIZE / 512 ))
        if nsenter --target 1 --mount -- dmsetup create "$POOL_NAME" \
          --table "0 $LENGTH thin-pool $META_DEV $DATA_DEV 128 32768 1 skip_block_zeroing"; then
          POOL_READY=true
        elif nsenter --target 1 --mount -- dmsetup create "$POOL_NAME" \
          --table "0 $LENGTH thin-pool $META_DEV $DATA_DEV 128 32768"; then
          echo "[cloudhv] WARNING: skip_block_zeroing not supported, pool creation may be slow"
          POOL_READY=true
        else
          echo "[cloudhv] WARNING: loopback thin-pool creation failed"
          nsenter --target 1 --mount -- losetup -d "$DATA_DEV" 2>/dev/null || true
          nsenter --target 1 --mount -- losetup -d "$META_DEV" 2>/dev/null || true
        fi
      else
        echo "[cloudhv] WARNING: blockdev failed for $DATA_DEV"
      fi
    else
      echo "[cloudhv] WARNING: losetup failed (DATA_DEV='$DATA_DEV' META_DEV='$META_DEV')"
    fi
  fi
fi

if [ "$POOL_READY" = "true" ]; then
  echo "[cloudhv] Thin pool $POOL_NAME ready"
else
  echo "[cloudhv] WARNING: devmapper thin pool not available, using ext4 cache fallback"
fi

# 5. Patch containerd config to add cloudhv runtime handler + devmapper
echo "[cloudhv] Patching containerd config..."
CONTAINERD_CONFIG="$HOST/etc/containerd/config.toml"

if [ ! -f "$CONTAINERD_CONFIG" ]; then
  echo "[cloudhv] WARNING: $CONTAINERD_CONFIG not found, skipping patch"
else
  # Backup
  cp "$CONTAINERD_CONFIG" "${CONTAINERD_CONFIG}.bak.$(date +%s)"
  ORIG_MODE=$(stat -c '%a' "$CONTAINERD_CONFIG" 2>/dev/null || echo "644")

  # Remove existing cloudhv runtime sections (idempotent)
  # Uses awk to delete lines from [*runtimes.cloudhv*] to the next non-cloudhv section header
  awk '
    /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
    skip && /^\[/ && !/cloudhv/ { skip=0 }
    !skip { print }
  ' "$CONTAINERD_CONFIG" > "${CONTAINERD_CONFIG}.tmp" && mv "${CONTAINERD_CONFIG}.tmp" "$CONTAINERD_CONFIG"
  chmod "$ORIG_MODE" "$CONTAINERD_CONFIG" 2>/dev/null || true

  # Add devmapper snapshotter config if pool is ready
  if [ "$POOL_READY" = "true" ]; then
    # Remove existing devmapper snapshotter config
    awk '
      /^\[.*\.snapshotter.*devmapper/ { skip=1; next }
      skip && /^\[/ && !/devmapper/ { skip=0 }
      !skip { print }
    ' "$CONTAINERD_CONFIG" > "${CONTAINERD_CONFIG}.tmp" && mv "${CONTAINERD_CONFIG}.tmp" "$CONTAINERD_CONFIG"
    chmod "$ORIG_MODE" "$CONTAINERD_CONFIG" 2>/dev/null || true

    cat >> "$CONTAINERD_CONFIG" << DMCONF

# CloudHV devmapper snapshotter for zero-copy rootfs delivery
[plugins."io.containerd.snapshotter.v1.devmapper"]
  root_path = "/var/lib/containerd/devmapper"
  pool_name = "$POOL_NAME"
  base_image_size = "1024MB"
DMCONF
    echo "[cloudhv] Devmapper snapshotter configured"
  fi

  # Add cloudhv runtime
  {
    echo ""
    echo "# Cloud Hypervisor VM-isolated runtime"
    echo '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]'
    echo '  runtime_type = "io.containerd.cloudhv.v1"'
    if [ "$POOL_READY" = "true" ]; then
      echo '  snapshotter = "devmapper"'
    fi
  } >> "$CONTAINERD_CONFIG"
  echo "[cloudhv] Runtime handler added to containerd config"
fi

if [ ! -f "$HOST/usr/local/bin/cloud-hypervisor" ]; then
  echo "[cloudhv] Copying cloud-hypervisor binary (${HOST_ARCH})..."
  install -m 755 "$ARTIFACTS/cloud-hypervisor" "$HOST/usr/local/bin/cloud-hypervisor"
fi

# 6. Schedule a deferred containerd restart on the host.
echo "[cloudhv] Scheduling deferred containerd restart (5s delay)..."
nsenter --target 1 --mount --uts --ipc --pid -- \
  bash -c 'nohup bash -c "sleep 5 && systemctl restart containerd" &>/dev/null &'

# 7. Verify containerd comes back
sleep 10
if chroot "$HOST" systemctl is-active --quiet containerd; then
  echo "[cloudhv] containerd restarted successfully"
else
  echo "[cloudhv] WARNING: containerd may still be restarting"
fi

# 8. Label node as ready
NODE_NAME=$(cat "$HOST/etc/hostname")
echo "[cloudhv] Installation complete on $NODE_NAME"
if [ "$POOL_READY" = "true" ]; then
  echo "[cloudhv] Rootfs delivery: devmapper passthrough (zero-copy)"
else
  echo "[cloudhv] Rootfs delivery: ext4 cache (fallback)"
fi

# Keep running so the DaemonSet stays healthy
echo "[cloudhv] Installer idle. Shim is active."
exec sleep infinity
