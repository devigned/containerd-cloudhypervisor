#!/bin/sh
# vim: et ts=2 sw=2 syn=sh

# Containerised build for the guest kernel (no host dependencies).
#
# Usage:
#   hacks/build-guest-kernel.sh [--arch <arch>]
# arch can be x86-64 or arm64.

LTS_SERIES="6.18"

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

# Runs inside the container
build() {
  host_user_group="$1"
  shift
  
  _latest_release() {
    curl -sSL https://www.kernel.org/releases.json \
      | jq -r ".releases[] | select(.version | startswith(\"${LTS_SERIES}\") ) | \"\(.version)\""
  }

  echo -e "\n############  Installing build prerequisites ############\n"
  apk add --no-cache \
      build-base linux-headers ncurses-dev bc elfutils-dev openssl-dev flex bison gawk diffutils jq curl perl

  echo -e "\n############  Commencing Kernel Build ############\n"
  mkdir -p /opt/build
  cd /opt/build

  cp -a /host/* .
  cd guest/kernel
  ./build-kernel.sh "$(_latest_release)" $@
  cp vmlinux /host/
  cp .config /host/vmlinux.kconfig
  chown "$host_user_group" /host/vmlinux /host/vmlinux.kconfig
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
  if [ "${1:-}" == "--arch" ] ; then
    shift; shift
  fi
  docker run --rm -i -v $(pwd):/host \
      $platform \
      alpine \
      "/host/hacks/${scriptname}" "build" "$uid:$gid" $@
fi
