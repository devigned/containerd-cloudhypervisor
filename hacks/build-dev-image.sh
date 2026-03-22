#!/bin/bash
# vim: et ts=2 sw=2
#
# Build a dev installer image and push it to GHCR.
#
# This script is the single source of truth for dev image builds.
# It ensures that the shim, agent, kernel, and rootfs are all built
# from the SAME commit, and that the resulting image is tagged with
# that commit's SHA.
#
# Usage:
#   hacks/build-dev-image.sh [--remote <ssh-host>] [--owner <ghcr-owner>]
#
# Examples:
#   # Build on the dev VM and push to ghcr.io/devigned/cloudhv-installer-dev
#   hacks/build-dev-image.sh --remote hl-dev --owner devigned
#
#   # Build locally (requires Docker, Alpine cross-build support)
#   hacks/build-dev-image.sh --owner devigned
#
# The script will:
#   1. Determine the current git SHA
#   2. Sync code to the remote host (if --remote)
#   3. Build the static shim (force rebuild, never use cached binary)
#   4. Build the guest (kernel + rootfs with agent)
#   5. Verify the shim is the correct architecture (x86-64)
#   6. Build and push the Docker image tagged with the git SHA
#   7. Print the exact helm/kubectl commands to deploy it
#
# The image is pushed to ghcr.io/<owner>/cloudhv-installer-dev:<sha>

set -euo pipefail

REMOTE=""
OWNER="devigned"
SCRIPT_DIR="$(cd "$(dirname "$0")"; pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.."; pwd)"

while [ $# -gt 0 ]; do
  case "$1" in
    --remote) REMOTE="$2"; shift 2 ;;
    --owner)  OWNER="$2";  shift 2 ;;
    *)        echo "Unknown arg: $1"; exit 1 ;;
  esac
done

SHA="$(cd "$REPO_DIR" && git rev-parse --short HEAD)"
IMAGE="ghcr.io/${OWNER}/cloudhv-installer-dev:${SHA}"

echo "========================================"
echo " CloudHV Dev Image Builder"
echo "========================================"
echo " Commit:  ${SHA}"
echo " Image:   ${IMAGE}"
echo " Remote:  ${REMOTE:-local}"
echo "========================================"
echo ""

if [ -n "$REMOTE" ]; then
  # ── Remote build ──────────────────────────────────────────────────

  echo "==> Step 1: Syncing code to ${REMOTE}..."
  cd "$REPO_DIR"
  # rsync exit code 23 (partial transfer from vanishing target/ files) is normal
  make sync REMOTE_HOST="${REMOTE}" 2>&1 | tail -3 || true
  echo ""

  echo "==> Step 2: Building static shim and daemon (force rebuild)..."
  ssh "$REMOTE" "cd ~/containerd-cloudhypervisor && rm -f containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon && bash hacks/build-static-rust.sh containerd-shim-cloudhv cloudhv-sandbox-daemon" 2>&1 | tail -5
  echo ""

  echo "==> Step 3: Verifying shim architecture..."
  ARCH=$(ssh "$REMOTE" "file ~/containerd-cloudhypervisor/containerd-shim-cloudhv-v1" 2>&1)
  echo "  $ARCH"
  if ! echo "$ARCH" | grep -q "x86-64"; then
    echo "ERROR: Shim is not x86-64! Got: $ARCH"
    exit 1
  fi
  echo ""

  echo "==> Step 4: Building guest (kernel + rootfs)..."
  ssh "$REMOTE" "cd ~/containerd-cloudhypervisor && bash hacks/build-guest.sh" 2>&1 | tail -5
  echo ""

  echo "==> Step 5: Building and pushing Docker image..."
  ssh "$REMOTE" "bash -s" << ENDSCRIPT
set -e
cd ~/containerd-cloudhypervisor

# Verify all artifacts exist
for f in containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon vmlinux rootfs.erofs installer/install.sh; do
  if [ ! -f "\$f" ]; then
    echo "ERROR: Missing artifact: \$f"
    exit 1
  fi
done

rm -rf /tmp/image-root && mkdir -p /tmp/image-root/opt/cloudhv
cp containerd-shim-cloudhv-v1 /tmp/image-root/opt/cloudhv/
cp cloudhv-sandbox-daemon     /tmp/image-root/opt/cloudhv/
cp vmlinux                    /tmp/image-root/opt/cloudhv/
cp rootfs.erofs               /tmp/image-root/opt/cloudhv/
cp installer/install.sh       /tmp/image-root/opt/cloudhv/
chmod +x /tmp/image-root/opt/cloudhv/containerd-shim-cloudhv-v1
chmod +x /tmp/image-root/opt/cloudhv/cloudhv-sandbox-daemon
chmod +x /tmp/image-root/opt/cloudhv/install.sh

docker build -t ${IMAGE} -f installer/Dockerfile /tmp/image-root
docker push ${IMAGE}
ENDSCRIPT
  echo ""

else
  # ── Local build ───────────────────────────────────────────────────
  echo "ERROR: Local build not yet implemented. Use --remote."
  exit 1
fi

echo "========================================"
echo " SUCCESS: ${IMAGE}"
echo "========================================"
echo ""
echo "To deploy on an AKS cluster:"
echo ""
echo "  # Install (first time):"
echo "  helm install cloudhv-installer \\"
echo "    oci://ghcr.io/${OWNER}/charts/cloudhv-installer \\"
echo "    --version 0.5.3 --namespace kube-system \\"
echo "    --set image.repository=ghcr.io/${OWNER}/cloudhv-installer-dev \\"
echo "    --set image.tag=${SHA}"
echo ""
echo "  # Or update existing:"
echo "  kubectl set image daemonset/cloudhv-installer -n kube-system \\"
echo "    installer=${IMAGE}"
echo ""
echo "  # If private image, create pull secret first:"
echo "  kubectl create secret docker-registry ghcr-secret \\"
echo "    --docker-server=ghcr.io --docker-username=${OWNER} \\"
echo "    --docker-password=\$(gh auth token) -n kube-system"
echo ""
