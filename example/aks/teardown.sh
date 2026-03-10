#!/usr/bin/env bash
set -euo pipefail

: "${RESOURCE_GROUP:?Set RESOURCE_GROUP to the Azure resource group name}"

echo "=== Tearing down AKS resources ==="
echo "  Resource group: $RESOURCE_GROUP"
echo ""
read -p "Are you sure? This will delete EVERYTHING in $RESOURCE_GROUP. (y/N) " confirm
if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
  echo "Aborted."
  exit 0
fi

az group delete --name "$RESOURCE_GROUP" --yes --no-wait
echo "=== Deletion started (async). Resources will be removed in a few minutes. ==="
