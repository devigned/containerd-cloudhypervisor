#!/usr/bin/env bash
#
# Build a minimal Linux kernel for Cloud Hypervisor microVMs.
#
# Based on Cloud Hypervisor's ch_defconfig with additions for:
# - virtio-fs (FUSE + virtiofs)
# - vsock (VIRTIO_VSOCK)
# - overlayfs (container layers)
# - cgroups v2 (resource limits)
# - namespaces (PID, mount, network, user)
# - ext4 filesystem
#
# Usage: ./build-kernel.sh [kernel_version]
#
set -euo pipefail

KERNEL_VERSION="${1:-6.12.8}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz"
KERNEL_DIR="linux-${KERNEL_VERSION}"
CONFIG_FILE="configs/microvm.config"
NPROC=$(nproc)

echo "=== Building minimal kernel ${KERNEL_VERSION} ==="

# Download kernel source if not present
if [ ! -d "${KERNEL_DIR}" ]; then
    echo "Downloading kernel ${KERNEL_VERSION}..."
    wget -q "${KERNEL_URL}" -O "linux-${KERNEL_VERSION}.tar.xz"
    tar xf "linux-${KERNEL_VERSION}.tar.xz"
    rm -f "linux-${KERNEL_VERSION}.tar.xz"
fi

cd "${KERNEL_DIR}"

# Apply our config
if [ -f "../${CONFIG_FILE}" ]; then
    cp "../${CONFIG_FILE}" .config
    make olddefconfig
    # Force-enable PVH boot support (required for Cloud Hypervisor direct kernel boot).
    # The full dependency chain is: HYPERVISOR_GUEST -> PARAVIRT -> XEN -> XEN_PVH -> PVH
    # olddefconfig may disable these if deps aren't fully specified in the config fragment.
    scripts/config --enable HYPERVISOR_GUEST
    scripts/config --enable PARAVIRT
    scripts/config --enable XEN
    scripts/config --enable XEN_PVH
    scripts/config --enable PVH
    make olddefconfig
else
    echo "ERROR: Config file not found: ${CONFIG_FILE}"
    echo "Using Cloud Hypervisor's default config from the kernel tree"
    make ch_defconfig 2>/dev/null || make tinyconfig
    # Enable required options
    scripts/config --enable VIRTIO_VSOCK
    scripts/config --enable FUSE_FS
    scripts/config --enable VIRTIO_FS
    scripts/config --enable OVERLAY_FS
    scripts/config --enable CGROUPS
    scripts/config --enable CGROUP_BPF
    scripts/config --enable NAMESPACES
    scripts/config --enable EXT4_FS
    make olddefconfig
fi

echo "Building kernel with ${NPROC} jobs..."
make -j "${NPROC}" vmlinux

# Copy the kernel binary to a predictable location
cp vmlinux ../vmlinux
KERNEL_SIZE=$(stat -c%s ../vmlinux 2>/dev/null || stat -f%z ../vmlinux)
echo "=== Kernel built: vmlinux ($(( KERNEL_SIZE / 1024 / 1024 )) MB) ==="
