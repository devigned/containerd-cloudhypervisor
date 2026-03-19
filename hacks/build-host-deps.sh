#!/bin/sh
# vim: et ts=2 sw=2 syn=sh
#
# Containerised build for static versions of all host dependencies.
# Currently "mkfs.erofs" (from erofs-utils)
#
# Usage:
#   hacks/build-host-deps.sh [--arch <arch>]
# arch can be x86-64 or arm64.

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

EROFS_VERSION="1.9.1"

# Runs inside the container
build() {
  host_user_group="$1"
  shift
  
  echo -e "\n############  Installing build prerequisites ############\n"
  apk add --no-cache build-base \
      autoconf automake libtool lz4-dev util-linux-dev \
      util-linux-static zlib-dev zlib-static zstd-static lz4-static xz-static \
      openssl-dev openssl-libs-static

  mkdir /opt/build-host-deps
  cd /opt/build-host-deps
  wget "https://git.kernel.org/pub/scm/linux/kernel/git/xiang/erofs-utils.git/snapshot/erofs-utils-${EROFS_VERSION}.tar.gz"
  tar xzf "erofs-utils-${EROFS_VERSION}.tar.gz"
  cd "erofs-utils-${EROFS_VERSION}"

  autoreconf -fiv
  CFLAGS='--static --static -Wl,-static' LDFLAGS="-static --static -Wl,-static" \
    ./configure --prefix=/usr/local/ --enable-static --disable-shared
  CFLAGS='--static --static -Wl,-static' LDFLAGS="-static --static -Wl,-static" \
    make -j
  make DESTDIR="$(pwd)/_install" install

  cp _install/usr/local/bin/mkfs.erofs /host
  strip /host/mkfs.erofs
  chown "$host_user_group" /host/mkfs.erofs
}
# --
  
if [ "${1:-}" = "build" ] ; then
  shift
  build ${@}
else
  docker_wrapper ${@}
fi
