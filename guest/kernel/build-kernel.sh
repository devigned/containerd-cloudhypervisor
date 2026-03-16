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
NPROC=$(nproc)

# Select config based on host architecture
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64)  CONFIG_FILE="configs/microvm.config" ;;
    aarch64) CONFIG_FILE="configs/microvm-aarch64.config" ;;
    *)       echo "ERROR: unsupported architecture: ${HOST_ARCH}"; exit 1 ;;
esac

echo "=== Building minimal kernel ${KERNEL_VERSION} (${HOST_ARCH}) ==="

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

    # Architecture-specific force-enables
    if [ "${HOST_ARCH}" = "x86_64" ]; then
        # PVH boot (required for Cloud Hypervisor direct kernel boot on x86)
        scripts/config --enable HYPERVISOR_GUEST
        scripts/config --enable PARAVIRT
        scripts/config --enable XEN
        scripts/config --enable XEN_PVH
        scripts/config --enable PVH
    elif [ "${HOST_ARCH}" = "aarch64" ]; then
        # ARM64 serial console (PL011 UART)
        scripts/config --enable SERIAL_AMBA_PL011
        scripts/config --enable SERIAL_AMBA_PL011_CONSOLE
        # ARM GIC interrupt controller
        scripts/config --enable ARM_GIC
        scripts/config --enable ARM_GIC_V3
    fi

    # Common force-enables (both architectures)
    scripts/config --enable VSOCKETS
    scripts/config --enable VIRTIO_VSOCKETS
    scripts/config --enable PCI_MSI
    scripts/config --enable BPF
    scripts/config --enable BPF_SYSCALL
    scripts/config --enable CGROUP_BPF
    scripts/config --enable BPF_JIT
    scripts/config --enable HOTPLUG_PCI
    scripts/config --enable NETDEVICES
    scripts/config --enable NET_CORE
    scripts/config --enable IP_PNP
    scripts/config --enable HOTPLUG_PCI_ACPI
    scripts/config --enable MEMORY_HOTPLUG
    scripts/config --enable MEMORY_HOTREMOVE
    scripts/config --enable VIRTIO_MEM
    scripts/config --enable VIRTIO_BALLOON
    scripts/config --enable PAGE_REPORTING
    scripts/config --enable PSI
    scripts/config --disable PSI_DEFAULT_DISABLED
    scripts/config --enable EROFS_FS
    scripts/config --enable EROFS_FS_XATTR
    scripts/config --enable EROFS_FS_POSIX_ACL
    scripts/config --enable EROFS_FS_SECURITY
    make olddefconfig

    # Verify critical configs (common + arch-specific)
    VERIFY_OPTS="PCI_MSI VSOCKETS VIRTIO_VSOCKETS BPF_SYSCALL CGROUP_BPF HOTPLUG_PCI VIRTIO_MEM PSI EROFS_FS"
    if [ "${HOST_ARCH}" = "x86_64" ]; then
        VERIFY_OPTS="${VERIFY_OPTS} PVH"
    elif [ "${HOST_ARCH}" = "aarch64" ]; then
        VERIFY_OPTS="${VERIFY_OPTS} SERIAL_AMBA_PL011 ARM_GIC"
    fi
    for opt in ${VERIFY_OPTS}; do
        if ! grep -q "CONFIG_${opt}=y" .config; then
            echo "ERROR: CONFIG_${opt} is not enabled!"
            exit 1
        fi
    done
    echo "Kernel config verified (${HOST_ARCH}): all critical configs enabled"
else
    echo "ERROR: Config file not found: ${CONFIG_FILE}"
    echo "Using Cloud Hypervisor's default config from the kernel tree"
    make ch_defconfig 2>/dev/null || make tinyconfig
    # Enable required options
    scripts/config --enable VIRTIO_VSOCKETS
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

# Strip debug info for smaller kernel (~27MB → ~5MB)
echo "Stripping kernel debug info..."
strip --strip-debug vmlinux 2>/dev/null || true

# Copy the kernel binary to a predictable location
cp vmlinux ../vmlinux
KERNEL_SIZE=$(stat -c%s ../vmlinux 2>/dev/null || stat -f%z ../vmlinux)
echo "=== Kernel built: vmlinux ($(( KERNEL_SIZE / 1024 / 1024 )) MB) ==="
