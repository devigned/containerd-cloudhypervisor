#!/bin/sh
# vim: et ts=2 sw=2 syn=sh

translate_arch() {
  local arch="${1:-$(uname -m)}"
  case "$arch" in
    x86_64)  echo "x86-64";;
    x86-64)  echo "x86-64";;
    aarch64) echo "arm64";;
    arm64)   echo "arm64";;
    *) echo "ERROR: unknown arch '$1'"; exit 1;;
  esac
}
# --

docker_arch() {
  local i=1
  local arch=""
  arch="$(translate_arch)"

  while [ "$#" -gt 0 ]; do
    if [ "${1}" = "--arch" ]; then
      shift
      arch="$(translate_arch "$1")"
    fi
    shift
  done

  echo "$arch"
}
# --

docker_platform() {
  local arch="${1:-$(translate_arch)}"

  case "$arch" in
    x86-64)  echo "--platform linux/x86-64";;
    arm64)   echo "--platform linux/arm64/v8";;
  esac
}
# --

latest_kernel_release() {
  local series="${1}"

  curl -sSL https://www.kernel.org/releases.json \
    | jq -r ".releases[] | select(.version | startswith(\"${series}\") ) | \"\(.version)\""
}
# --

docker_wrapper() {
  local uid gid scriptname platform arch
  uid="$(id -u)"
  gid="$(id -g)"
  scriptname="$(basename "$0")"

  arch="$(docker_arch "$@")"
  if [ "${1:-}" == "--arch" ] ; then
    shift; shift
  fi
  platform="$(docker_platform "$arch")"

  docker run --rm -i -v $(pwd):/host \
      $platform \
      alpine \
      "/host/hacks/${scriptname}" "build" "$uid:$gid" ${@}
}
# --

build_if_missing() {
  local host_user_group="$1"
  local build_command="$2"
  shift; shift

  local build_opts=""
  while [ "$1" != "--" ] ; do
    build_opts="$build_opts $1"
    shift
  done
  shift # "--"

  local f build="false"
  for f in ${@}; do
    if [ ! -f "/host/${f}" ] ; then
      echo -e "\n --- '${f}' not found, running '${build_command} ${build_opts}' to build it ---\n"
      $build_command build "$host_user_group" ${build_opts}
      build="true"
      break
    fi
  done

  if [ "$build" = "false" ] ; then
    echo -e "\n --- Skipping '${build_command}', all artifacts found: ${@} ---\n"
  fi
}
# --
