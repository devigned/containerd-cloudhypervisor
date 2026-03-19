#!/bin/sh
# vim: et ts=2 sw=2 syn=sh

# Containerised build for the guest kernel (no host dependencies).
#
# Usage:
#   hacks/build-guest-kernel.sh [--arch <arch>]
# arch can be x86-64 or arm64.

KERNEL_SERIES="${KERNEL_SERIES:-6.18}"

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

# Runs inside the container
build() {
  host_user_group="$1"
  shift
  
  echo -e "\n############  Installing build prerequisites ############\n"
  apk add --no-cache \
      build-base linux-headers ncurses-dev bc elfutils-dev openssl-dev flex bison gawk diffutils jq curl perl

  echo -e "\n############  Commencing Kernel Build ############\n"
  mkdir -p /opt/build-kernel
  cd /opt/build-kernel

  cp -a /host/* .
  cd guest/kernel
  ./build-kernel.sh "$(latest_kernel_release "${KERNEL_SERIES}")" $@
  cp vmlinux /host/
  cp .config /host/vmlinux.kconfig
  chown "$host_user_group" /host/vmlinux /host/vmlinux.kconfig
}
# --
  
if [ "${1:-}" = "build" ] ; then
  shift
  build ${@}
else
  docker_wrapper ${@}
fi
