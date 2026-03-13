#!/usr/bin/env sh
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
ROOTFS_DIR="rootfs-base"
IMAGE_FILE="rootfs.erofs"

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
rm -f "${IMAGE_FILE}"

echo "Installing agent binary as /init..."
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

# Agent configuration
if [ -f "agent.conf" ]; then
    cp agent.conf "${ROOTFS_DIR}/etc/cloudhv-agent.conf"
fi

# Create filesystem image
echo "Creating root filesystem image"
mkfs.erofs --all-root --exclude-regex '.*\.gitkeep' "${IMAGE_FILE}" "${ROOTFS_DIR}"

# Report size
IMAGE_ACTUAL_SIZE=$(du -sh "${IMAGE_FILE}" | cut -f1)
ROOTFS_SIZE=$(du -sh "${ROOTFS_DIR}" | cut -f1)
echo "=== Rootfs built ==="
echo "  Contents:"
find "${ROOTFS_DIR}" -type f -not -name '*.gitkeep' -exec ls -lh {} \; | awk '{print "    " $5 " " $9}'
echo "----"
echo "  Directory: ${ROOTFS_SIZE} (${ROOTFS_DIR}/)"
echo "  Image:     ${IMAGE_ACTUAL_SIZE} (${IMAGE_FILE})"
