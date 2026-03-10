#!/usr/bin/env bash
set -euo pipefail

# Required environment variables
: "${AZURE_SUBSCRIPTION:?Set AZURE_SUBSCRIPTION to your Azure subscription ID}"
: "${AZURE_REGION:?Set AZURE_REGION to an Azure region (e.g. eastus2, westus3)}"
: "${RESOURCE_GROUP:?Set RESOURCE_GROUP to the Azure resource group name}"
: "${CLUSTER_NAME:?Set CLUSTER_NAME to the AKS cluster name}"

echo "=== Creating AKS cluster ==="
echo "  Subscription: $AZURE_SUBSCRIPTION"
echo "  Region:       $AZURE_REGION"
echo "  Group:        $RESOURCE_GROUP"
echo "  Cluster:      $CLUSTER_NAME"

az account set --subscription "$AZURE_SUBSCRIPTION"

echo "[1/3] Creating resource group..."
az group create --name "$RESOURCE_GROUP" --location "$AZURE_REGION" -o none

echo "[2/3] Creating AKS cluster with system nodepool..."
az aks create \
  --resource-group "$RESOURCE_GROUP" \
  --name "$CLUSTER_NAME" \
  --location "$AZURE_REGION" \
  --node-count 1 \
  --node-vm-size Standard_D2s_v5 \
  --nodepool-name system \
  --generate-ssh-keys \
  --network-plugin azure \
  -o none

echo "[3/3] Adding worker nodepool (3x D4s_v5, KVM-capable)..."
az aks nodepool add \
  --resource-group "$RESOURCE_GROUP" \
  --cluster-name "$CLUSTER_NAME" \
  --name cloudhv \
  --node-count 3 \
  --node-vm-size Standard_D4s_v5 \
  --labels workload=cloudhv \
  --max-pods 110 \
  -o none

echo "[*] Getting credentials..."
az aks get-credentials \
  --resource-group "$RESOURCE_GROUP" \
  --name "$CLUSTER_NAME" \
  --overwrite-existing

echo "=== Cluster ready ==="
kubectl get nodes -o wide
