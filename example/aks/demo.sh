#!/usr/bin/env bash
set -euo pipefail

# ============================================================
# containerd-cloudhypervisor AKS Demo
#
# Demonstrates VM-isolated container workloads on AKS, measuring
# boot latency, memory overhead, and workload density.
#
# Required env vars:
#   AZURE_SUBSCRIPTION  — Azure subscription ID
#   AZURE_REGION        — Azure region (e.g. eastus2)
#   RESOURCE_GROUP      — Resource group name
#   CLUSTER_NAME        — AKS cluster name
#   INSTALLER_IMAGE     — Container image with shim artifacts
#                         (built from example/aks/installer/Dockerfile)
# ============================================================

: "${AZURE_SUBSCRIPTION:?Set AZURE_SUBSCRIPTION}"
: "${AZURE_REGION:?Set AZURE_REGION}"
: "${RESOURCE_GROUP:?Set RESOURCE_GROUP}"
: "${CLUSTER_NAME:?Set CLUSTER_NAME}"
: "${INSTALLER_IMAGE:?Set INSTALLER_IMAGE to the pushed cloudhv-installer image}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

section() { echo -e "\n\033[1;36m=== $1 ===\033[0m"; }
info()    { echo -e "  \033[0;32m✓\033[0m $1"; }
measure() { echo -e "  \033[1;33m▸\033[0m $1"; }

# ── Step 1: Create AKS cluster ─────────────────────────────
section "Step 1: Create AKS Cluster"
bash "$SCRIPT_DIR/setup.sh"
info "Cluster ready with 3 worker nodes"

# ── Step 2: Install the shim via DaemonSet ──────────────────
section "Step 2: Install Cloud Hypervisor Shim"

# Patch the DaemonSet with the actual installer image
sed "s|INSTALLER_IMAGE_PLACEHOLDER|${INSTALLER_IMAGE}|g" \
  "$SCRIPT_DIR/manifests/daemonset.yaml" | kubectl apply -f -

kubectl apply -f "$SCRIPT_DIR/manifests/runtimeclass.yaml"

echo "  Waiting for installer pods to be ready..."
kubectl -n kube-system rollout status daemonset/cloudhv-installer --timeout=300s
info "Shim installed on all worker nodes"

# Show installer logs
echo ""
echo "  Installer output:"
kubectl -n kube-system logs -l app=cloudhv-installer --tail=5 | sed 's/^/    /'

# ── Step 3: Deploy echo workload ────────────────────────────
section "Step 3: Deploy VM-Isolated Workload"

kubectl apply -f "$SCRIPT_DIR/manifests/echo-deployment.yaml"

echo "  Waiting for pod to be ready..."
START=$(date +%s%N)
kubectl rollout status deployment/echo-cloudhv --timeout=120s
END=$(date +%s%N)
BOOT_MS=$(( (END - START) / 1000000 ))
measure "First pod ready in ${BOOT_MS}ms (includes image pull + VM boot)"

# ── Step 4: Test the workload ───────────────────────────────
section "Step 4: Verify Workload"

echo "  Waiting for LoadBalancer IP..."
for i in $(seq 1 30); do
  LB_IP=$(kubectl get svc echo-cloudhv -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)
  if [ -n "$LB_IP" ]; then break; fi
  sleep 5
done

if [ -n "$LB_IP" ]; then
  RESPONSE=$(curl -s --max-time 5 "http://$LB_IP" || echo "connection failed")
  info "Response from VM-isolated pod: $RESPONSE"
else
  echo "  LoadBalancer IP not available yet (may take a few minutes)"
fi

# ── Step 5: Measure memory overhead ─────────────────────────
section "Step 5: Memory Overhead"

echo "  Per-node memory usage:"
for node in $(kubectl get nodes -l workload=cloudhv -o name); do
  NODE_NAME=$(echo "$node" | cut -d/ -f2)
  ALLOC=$(kubectl describe "$node" | grep -A5 "Allocated resources" | grep memory | awk '{print $2}')
  CAPACITY=$(kubectl describe "$node" | grep -A1 "Capacity:" | grep memory | awk '{print $2}')
  echo "    $NODE_NAME: allocated=$ALLOC / capacity=$CAPACITY"
done

echo ""
echo "  RuntimeClass overhead per pod: 50Mi memory, 50m CPU"
echo "  Estimated per-VM overhead: ~40 MB (CH + kernel + agent + virtiofsd + shim)"

# ── Step 6: Scale up ────────────────────────────────────────
section "Step 6: Scale Up (1 → 10 replicas)"

START=$(date +%s%N)
kubectl scale deployment/echo-cloudhv --replicas=10
kubectl rollout status deployment/echo-cloudhv --timeout=120s
END=$(date +%s%N)
SCALE_MS=$(( (END - START) / 1000000 ))
measure "Scaled to 10 replicas in ${SCALE_MS}ms"

echo ""
echo "  Pod distribution across nodes:"
kubectl get pods -l app=echo-cloudhv -o wide --no-headers | awk '{print "    " $7}' | sort | uniq -c | sort -rn

echo ""
echo "  Node memory after scale-up:"
for node in $(kubectl get nodes -l workload=cloudhv -o name); do
  NODE_NAME=$(echo "$node" | cut -d/ -f2)
  PODS=$(kubectl get pods -l app=echo-cloudhv --field-selector spec.nodeName="$NODE_NAME" --no-headers 2>/dev/null | wc -l | tr -d ' ')
  ALLOC=$(kubectl describe "$node" | grep -A5 "Allocated resources" | grep memory | awk '{print $2}')
  echo "    $NODE_NAME: $PODS pods, memory allocated=$ALLOC"
done

# ── Step 7: Scale down ──────────────────────────────────────
section "Step 7: Scale Down (10 → 1 replica)"

START=$(date +%s%N)
kubectl scale deployment/echo-cloudhv --replicas=1
sleep 5
kubectl wait --for=condition=available deployment/echo-cloudhv --timeout=30s
END=$(date +%s%N)
SCALE_DOWN_MS=$(( (END - START) / 1000000 ))
measure "Scaled down in ${SCALE_DOWN_MS}ms"

# ── Summary ─────────────────────────────────────────────────
section "Summary"

echo ""
echo "  ┌─────────────────────────────────────────────────┐"
echo "  │  containerd-cloudhypervisor AKS Demo Results    │"
echo "  ├─────────────────────────────────────────────────┤"
echo "  │  First pod boot:      ${BOOT_MS}ms                "
echo "  │  Scale 1→10:          ${SCALE_MS}ms               "
echo "  │  Scale 10→1:          ${SCALE_DOWN_MS}ms          "
echo "  │  VM overhead per pod: ~40 MB + 50m CPU            "
echo "  │  Worker nodes:        3x D4s_v5 (16 GB each)     "
echo "  └─────────────────────────────────────────────────┘"
echo ""

# ── Cleanup prompt ──────────────────────────────────────────
echo ""
read -p "Delete the cluster and all resources? (y/N) " cleanup
if [[ "$cleanup" == "y" || "$cleanup" == "Y" ]]; then
  section "Cleanup"
  kubectl delete -f "$SCRIPT_DIR/manifests/echo-deployment.yaml" --ignore-not-found
  kubectl delete -f "$SCRIPT_DIR/manifests/runtimeclass.yaml" --ignore-not-found
  kubectl -n kube-system delete daemonset cloudhv-installer --ignore-not-found
  bash "$SCRIPT_DIR/teardown.sh"
fi
