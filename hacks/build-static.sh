#!/bin/sh

# Containerised build for static binaries.
# Use if you run into issues with static build targets like
#   " the `x86_64-unknown-linux-musl` target may not be installed"
#
# Usage:
#   hacks/build-static.sh <component> [<component>...]
# e.g
#   hacks/build-static.sh containerd-shim-cloudhv cloudhv-agent
# to build both a static shim and agent.

set -euo pipefail

# Runs inside the container
build() {
  host_user_group="$1"
  shift

  echo -e "\n############  Installing build prerequisites ############\n"
  apk add --no-cache \
    lld \
    protoc \
    cargo \
    rust \
    make \
    musl-dev \

  mkdir -p /opt/build
  cd /opt/build

  cp -a /host/* .

  # Force static linking for Rust and C dependencies.
  target="$(rustc -vV | sed -n 's/^host: //p')"
  export CARGO_BUILD_TARGET="${target}"
  TARGET="$(echo "${target}" | tr a-z- A-Z_)" 
  export CARGO_TARGET_${TARGET}_RUSTFLAGS="-C target-feature=+crt-static -L /usr/lib -C link-arg=-Wl,-static"
  export CARGO_TARGET_${TARGET}_PKG_CONFIG_ALL_STATIC=1
  export CARGO_TARGET_${TARGET}_LIBBPF_SYS_STATIC=1
  
  echo -e "\n############  Building static binaries ############"
  for f in "${@}"; do
    echo -e "\n   $f...\n"
    cargo build --release -p ${f}
    # target binaries might not be named exactly like components, so we're fuzzy here
    for a in "$(find "target/${target}/release" -maxdepth 1 -name "${f}*" -type f -executable)"; do
      cp "$a" /host/
      chown -R "$host_user_group" "/host/$(basename "${a}")"
    done
    echo -e "\n   ==> Done: $f\n"
  done
}
# --
  
if [ "$1" = "build" ] ; then
  shift
  build ${@}
else
  uid="$(id -u)"
  gid="$(id -g)"
  docker run --rm -ti -v $(pwd):/host alpine \
      /host/hacks/build-static.sh "build" "$uid:$gid" ${@}
fi
