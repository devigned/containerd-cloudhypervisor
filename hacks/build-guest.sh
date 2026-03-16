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

  /host/hacks/build-guest-kernel.sh build "$host_user_group" 
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
  uid="$(id -u)"
  gid="$(id -g)"
  scriptname="$(basename "$0")"

  arch="$(docker_arch "$@")"
  platform="$(docker_platform "$arch")"
  dest="_build/${arch}"

  rm -rf vmlinux vmlinux.kconfig rootfs.erofs "${dest}"
  echo -e "\n ------=======#######  Full Guest Build to '${dest}' #######=======-------\n"
  docker run --rm -i \
      -v $(pwd):/host \
      $platform \
      alpine \
      "/host/hacks/${scriptname}" "build" "$uid:$gid" $@
  mkdir -p "${dest}"
  mv vmlinux vmlinux.kconfig rootfs.erofs "${dest}"
  echo -e "\n ------=======#######  Full Guest Build done #######=======-------\n"
  echo "${dest}:"
  ls -lah "${dest}"
fi
