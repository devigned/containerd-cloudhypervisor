#!/bin/sh
# vim: et ts=2 sw=2 syn=sh
#
# Containerised build for the full guest (kernel and root FS)
# Usage:
#   hacks/build-guest.sh [--arch <arch>]
# The optional <arch> can be x86-64 or arm64.

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

# Runs inside the container
build() {
  host_user_group="$1"
  shift

  apk add --no-cache erofs-utils

  if [ ! -f /host/vmlinux -o ! -f /host/vmlinux.kconfig ] ; then
    /host/hacks/build-guest-kernel.sh build "$host_user_group"
  else
    echo -e "\n --- Found 'vmlinux', will use it for the guest instead of rebuilding from scratch. ---\n"
  fi
  /host/hacks/build-static-rust.sh build "$host_user_group" crates/agent/cloudhv-agent

  mv /host/cloudhv-agent /opt/build/guest/rootfs
  cd /opt/build/guest/rootfs
  ./build-rootfs.sh cloudhv-agent
  
  cp rootfs.erofs /host/
  chown "$host_user_group" /host/rootfs.erofs

}
# --
  
if [ "${1:-}" = "build" ] ; then
  shift
  build ${@}
else
  arch="$(docker_arch "$@")"
  dest="_build/${arch}"

  rm -rf rootfs.erofs "${dest}"
  mkdir -p "${dest}"
  echo -e "\n ------=======#######  Full Guest Build to '${dest}' #######=======-------\n"
  docker_wrapper ${@}
  mv vmlinux vmlinux.kconfig rootfs.erofs "${dest}"
  echo -e "\n ------=======#######  Full Guest Build done #######=======-------\n"
  echo "${dest}:"
  ls -lah "${dest}"
fi
