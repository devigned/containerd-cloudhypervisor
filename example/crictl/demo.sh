#!/usr/bin/env bash
#
# Demo: Run an HTTP echo server inside a Cloud Hypervisor microVM using crictl.
#
# Prerequisites: containerd + cloudhv runtime installed (see README.md)
#
# Usage: sudo bash demo.sh
#
set -euo pipefail

echo "╔══════════════════════════════════════════════════════════╗"
echo "║  Cloud Hypervisor microVM Container Demo (crictl)       ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""

# Clean up any previous demo pods
crictl rmp -fa 2>/dev/null || true
sleep 1

# ── Step 1: Pull the image ────────────────────────────────────
echo "▸ Pulling hashicorp/http-echo image..."
crictl pull hashicorp/http-echo:latest 2>/dev/null

# ── Step 2: Create pod sandbox (boots a microVM) ──────────────
echo "▸ Creating pod sandbox (booting microVM)..."

SANDBOX_CONFIG=$(mktemp)
cat > "$SANDBOX_CONFIG" <<EOF
{
  "metadata": {
    "name": "demo-pod",
    "namespace": "default",
    "attempt": 1,
    "uid": "demo-pod-uid"
  },
  "log_directory": "/tmp/demo-pod-logs",
  "linux": {}
}
EOF
mkdir -p /tmp/demo-pod-logs

BOOT_START=$(date +%s%N)
POD_ID=$(crictl runp --runtime=cloudhv "$SANDBOX_CONFIG")
BOOT_END=$(date +%s%N)
BOOT_MS=$(( (BOOT_END - BOOT_START) / 1000000 ))
echo "  Pod: ${POD_ID:0:12}... (booted in ${BOOT_MS}ms)"

POD_IP=$(crictl inspectp "$POD_ID" | python3 -c \
  "import sys,json; print(json.load(sys.stdin)['status']['network']['ip'])")
echo "  IP:  $POD_IP"

# ── Step 3: Create and start the echo container ───────────────
echo "▸ Starting HTTP echo container (port 5678)..."

CONTAINER_CONFIG=$(mktemp)
cat > "$CONTAINER_CONFIG" <<EOF
{
  "metadata": { "name": "echo-server" },
  "image": { "image": "hashicorp/http-echo:latest" },
  "command": ["/http-echo", "-text=Hello from a Cloud Hypervisor microVM! 🚀", "-listen=:5678"],
  "log_path": "echo.log"
}
EOF

CTR_START=$(date +%s%N)
CTR_ID=$(crictl create "$POD_ID" "$CONTAINER_CONFIG" "$SANDBOX_CONFIG")
crictl start "$CTR_ID"
CTR_END=$(date +%s%N)
CTR_MS=$(( (CTR_END - CTR_START) / 1000000 ))
echo "  Container: ${CTR_ID:0:12}... (started in ${CTR_MS}ms)"

# ── Step 4: Test the endpoint ─────────────────────────────────
echo "▸ Waiting for server to start..."
sleep 2

echo "▸ Curling http://$POD_IP:5678/ ..."
echo ""
RESPONSE=$(curl -s "http://$POD_IP:5678/" || echo "FAILED")
echo "  Response: $RESPONSE"
echo ""

if echo "$RESPONSE" | grep -q "Cloud Hypervisor"; then
    echo "✅ Success! The container is running inside a microVM."
else
    echo "❌ Unexpected response. Check containerd logs:"
    echo "   journalctl -u containerd --since '2 minutes ago' | tail -20"
fi

# ── Step 5: Show what's running ───────────────────────────────
echo ""
echo "▸ Running pods:"
crictl pods --no-trunc
echo ""
echo "▸ Running containers:"
crictl ps --no-trunc
echo ""
echo "▸ Container logs:"
crictl logs "$CTR_ID" 2>&1 | tail -5

# ── Cleanup instructions ──────────────────────────────────────
echo ""
echo "────────────────────────────────────────────────────────────"
echo "To clean up:"
echo "  sudo crictl stop $CTR_ID"
echo "  sudo crictl rm $CTR_ID"
echo "  sudo crictl stopp $POD_ID"
echo "  sudo crictl rmp $POD_ID"
echo ""
echo "Or: sudo crictl rmp -fa"

# Clean up temp files
rm -f "$SANDBOX_CONFIG" "$CONTAINER_CONFIG"
