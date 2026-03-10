#!/usr/bin/env bash
#
# Build a minimal guest rootfs for Cloud Hypervisor microVMs.
#
# Contents:
# - cloudhv-agent binary (statically linked, as PID 1)
# - crun binary (statically linked, lightweight OCI runtime)
# - Minimal /etc (passwd, group)
# - Essential directory structure
#
# No shell, no busybox, no package manager — absolute minimum for containers.
#
# Usage: ./build-rootfs.sh <path-to-cloudhv-agent-binary>
#
set -euo pipefail

AGENT_BINARY="${1:?Usage: build-rootfs.sh <path-to-cloudhv-agent-binary>}"
ROOTFS_DIR="rootfs"
IMAGE_FILE="rootfs.ext4"
IMAGE_SIZE_MB=16

echo "=== Building minimal guest rootfs ==="

# Verify agent binary exists and is static
if [ ! -f "${AGENT_BINARY}" ]; then
    echo "ERROR: Agent binary not found: ${AGENT_BINARY}"
    exit 1
fi

if file "${AGENT_BINARY}" | grep -q "dynamically linked"; then
    echo "WARNING: Agent binary is dynamically linked. Should be static (musl)."
fi

# Clean previous build
rm -rf "${ROOTFS_DIR}" "${IMAGE_FILE}"
mkdir -p "${ROOTFS_DIR}"

# Create directory structure
echo "Creating directory structure..."
mkdir -p "${ROOTFS_DIR}"/{bin,dev,etc,proc,sys,tmp,run,var,containers}
mkdir -p "${ROOTFS_DIR}"/dev/{pts,shm}
mkdir -p "${ROOTFS_DIR}"/sys/fs/cgroup

# Install agent as /init (PID 1)
echo "Installing agent binary as /init..."
cp "${AGENT_BINARY}" "${ROOTFS_DIR}/init"
chmod 755 "${ROOTFS_DIR}/init"

# Also place at /bin/cloudhv-agent for convenience
cp "${AGENT_BINARY}" "${ROOTFS_DIR}/bin/cloudhv-agent"
chmod 755 "${ROOTFS_DIR}/bin/cloudhv-agent"

# Install crun (lightweight OCI runtime, must be statically linked for the VM guest)
echo "Installing crun..."
CRUN_VERSION="1.20"
ARCH=$(uname -m)
case "${ARCH}" in
    x86_64) CRUN_ARCH="amd64" ;;
    aarch64) CRUN_ARCH="arm64" ;;
    *) echo "Unsupported arch: ${ARCH}"; exit 1 ;;
esac
wget -q "https://github.com/containers/crun/releases/download/${CRUN_VERSION}/crun-${CRUN_VERSION}-linux-${CRUN_ARCH}-disable-systemd" \
    -O "${ROOTFS_DIR}/bin/crun"
chmod 755 "${ROOTFS_DIR}/bin/crun"

# Verify crun is static (dynamically-linked crun from the host won't work in the VM)
if file "${ROOTFS_DIR}/bin/crun" | grep -q "dynamically linked"; then
    echo "ERROR: crun binary is dynamically linked — must be static for guest rootfs"
    exit 1
fi

# Minimal /etc
echo "Creating /etc files..."
cat > "${ROOTFS_DIR}/etc/passwd" << 'EOF'
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/bin/false
EOF

cat > "${ROOTFS_DIR}/etc/group" << 'EOF'
root:x:0:
nobody:x:65534:
EOF

cat > "${ROOTFS_DIR}/etc/hosts" << 'EOF'
127.0.0.1 localhost
::1       localhost
EOF

cat > "${ROOTFS_DIR}/etc/resolv.conf" << 'EOF'
nameserver 8.8.8.8
nameserver 8.8.4.4
EOF

# Agent configuration
if [ -f "agent.conf" ]; then
    cp agent.conf "${ROOTFS_DIR}/etc/cloudhv-agent.conf"
fi

# Create ext4 image
echo "Creating ext4 image (${IMAGE_SIZE_MB} MB)..."
dd if=/dev/zero of="${IMAGE_FILE}" bs=1M count="${IMAGE_SIZE_MB}" status=none
mkfs.ext4 -q -F -L rootfs "${IMAGE_FILE}"

# Mount and populate
MOUNT_DIR=$(mktemp -d)
sudo mount -o loop "${IMAGE_FILE}" "${MOUNT_DIR}"
sudo cp -a "${ROOTFS_DIR}/." "${MOUNT_DIR}/"
sudo umount "${MOUNT_DIR}"
rmdir "${MOUNT_DIR}"

# Report size
IMAGE_ACTUAL_SIZE=$(du -sh "${IMAGE_FILE}" | cut -f1)
ROOTFS_SIZE=$(du -sh "${ROOTFS_DIR}" | cut -f1)
echo "=== Rootfs built ==="
echo "  Directory: ${ROOTFS_SIZE} (${ROOTFS_DIR}/)"
echo "  Image:     ${IMAGE_ACTUAL_SIZE} (${IMAGE_FILE})"
echo "  Contents:"
find "${ROOTFS_DIR}" -type f -exec ls -lh {} \; | awk '{print "    " $5 " " $9}'
