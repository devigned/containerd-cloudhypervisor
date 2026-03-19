#!/bin/bash
#
# containerd-cloudhypervisor "echo" demo
#

set -euo pipefail

scriptdir="$(cd "$(dirname "$0")"; pwd)"
cfg_box="${scriptdir}/demo-sandbox.json"
cfg_cont="${scriptdir}/demo-container.json"

function announce() {
  echo -e "\n  * $@\n"
}
# --

pod_id_file="$(mktemp)"
cont_id_file="$(mktemp)"
# used by sandbox config for logging
mkdir -p /tmp/echo-pod-logs
trap "rm -rf '$pod_id_file' '$cont_id_file' '/tmp/echo-pod-logs'" EXIT

announce "Pulling 'echo' container image"
crictl pull hashicorp/http-echo:latest

announce "Creating Sandbox VM pod"
crictl runp --runtime=cloudhv "$cfg_box" | tee "$pod_id_file"
pod_id="$(cat "$pod_id_file")"
pod_ip="$(crictl inspectp "$pod_id" | jq -r '.status.network.ip')"
echo "Pod has IP address '$pod_ip'"

announce "Creating and starting 'echo' container"
crictl create "$pod_id" "$cfg_cont" "$cfg_box" | tee "$cont_id_file"
cont_id="$(cat "$cont_id_file")"
crictl start "$cont_id"

sleep 1
announce "Accessing echo container HTTP endpoint"
curl -sSL http://"$pod_ip":5678

announce "Printing container logs"
for f in /tmp/echo-pod-logs/*; do
  echo " ### $f ###"
  cat "$f"
done

announce "All done. Bye bye."
