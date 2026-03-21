#!/bin/bash
#
# Test script for the sysext demo
#

set -euo pipefail

function read_until() {
  local fd="$1"
  shift
  local match_string="${@}"

  echo "Read until: looking for '${match_string}'"

  local line
  while read -ru ${fd} line; do \
    echo "VM :: $line"
    if [[ "$line" == *"${match_string}"* ]] ; then
      echo -e "\n MATCH: '${match_string}'"
      break
    fi
  done
}
# --

function child_procs() {
  local ppid="$(ps -o pid= --ppid $1 | tr -d '\n')"
  if [[ -n "$ppid" ]]; then
    echo -n "$ppid "
    child_procs "$ppid"
  fi
}
# --

if [[ ! -f containerd-cloudhypervisor.raw ]]; then
  echo "ERROR: system extension image 'containerd-cloudhypervisor.raw' not found."
  exit 1
fi

echo "Fetching bakery for Flatcar VM automation"
if [[ ! -d sysext-bakery ]]; then 
  git clone --depth 1 https://github.com/flatcar/sysext-bakery.git
fi
cp containerd-cloudhypervisor.raw sysext-bakery
cd sysext-bakery

echo "Running the test (includes Flatcar image download)"
coproc flatcar { ./bakery.sh boot containerd-cloudhypervisor.raw 2>&1; }
read_until "${flatcar[0]}" "localhost login:"
sleep 1
echo "sudo /usr/share/cloudhv/demo/demo.sh; rc=\"\$?\"; echo 'TEST_IS_DONE'; echo \"TEST_EXIT_CODE=\$rc\"" >&${flatcar[1]}
read_until "${flatcar[0]}" "TEST_IS_DONE" # input is echoed on the console
read_until "${flatcar[0]}" "TEST_IS_DONE"
read -ru "${flatcar[0]}" exit_code_line

exit_code="$(echo "${exit_code_line}" | sed 's/.*TEST_EXIT_CODE=\([0-9]*\).*/\1/')"
echo "Exit code is '$exit_code'"
kill -9 $(child_procs "$flatcar_PID")

exit $exit_code
