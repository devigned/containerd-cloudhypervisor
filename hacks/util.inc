#!/bin/sh
# vim: et ts=2 sw=2 syn=sh

_translate_arch() {
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
  arch="$(_translate_arch)"

  while [ "$#" -gt 0 ]; do
    if [ "${1}" = "--arch" ]; then
      shift
      arch="$(_translate_arch "$1")"
    fi
    shift
  done

  echo "$arch"
}
# --

docker_platform() {
  local arch="${1:-$(_translate_arch)}"

  case "$arch" in
    x86-64)  echo "--platform linux/x86-64";;
    arm64)   echo "--platform linux/arm64/v8";;
  esac
}
# --
