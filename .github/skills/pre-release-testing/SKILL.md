---
name: pre-release-testing
description: |
  Build the shim, agent, kernel, and rootfs locally via containerized builds,
  then deploy to an AKS cluster for validation before cutting a release.
  Use this when asked to test a PR or branch on AKS without making a release.
---

# Pre-Release Testing on AKS

## When to Use

Use this skill to validate a code change on AKS **before** merging a PR or
cutting a release. It builds all artifacts on the dev VM, packages them into
a dev installer image, and deploys to a running AKS cluster.

## Inputs

The user must provide:

- **REMOTE_HOST**: SSH target for the dev VM (e.g. `azureuser@<IP>`)
- **GHCR_OWNER**: GitHub Container Registry owner (e.g. `devigned`)
- **AKS_RESOURCE_GROUP**: Azure resource group containing the AKS cluster
- **AKS_CLUSTER_NAME**: Name of the AKS cluster
- **AZURE_SUBSCRIPTION**: Azure subscription name to use

## Procedure

### 1. Sync Code to Dev VM

```bash
make sync REMOTE_HOST="${REMOTE_HOST}"
```

rsync exit code 23 (partial transfer) is normal — it's caused by
vanishing files in `target/`. Ignore it.

### 2. Build Static Shim

The shim must be statically linked for AKS (AzureLinux may have an older
glibc than the dev VM). Use the containerized build:

```bash
ssh ${REMOTE_HOST} 'cd ~/containerd-cloudhypervisor && bash hacks/build-static-rust.sh containerd-shim-cloudhv'
```

Verify the output is static:

```bash
ssh ${REMOTE_HOST} 'file ~/containerd-cloudhypervisor/containerd-shim-cloudhv-v1'
# Should show: "statically linked"
```

### 3. Build Guest (Kernel + Rootfs)

```bash
ssh ${REMOTE_HOST} 'cd ~/containerd-cloudhypervisor && bash hacks/build-guest.sh'
```

This takes ~10 minutes (kernel compilation). Artifacts land in `_build/x86-64/`:
- `vmlinux` (~23MB)
- `vmlinux.kconfig`
- `rootfs.erofs` (~5MB)

### 4. Build and Push Dev Installer Image

Use the **commit SHA** as the image tag to avoid caching issues:

```bash
SHA=$(git rev-parse --short HEAD)
ssh ${REMOTE_HOST} "bash -s" << ENDSCRIPT
set -e
cd ~/containerd-cloudhypervisor
rm -rf /tmp/image-root && mkdir -p /tmp/image-root/opt/cloudhv
cp containerd-shim-cloudhv-v1 /tmp/image-root/opt/cloudhv/
cp _build/x86-64/vmlinux /tmp/image-root/opt/cloudhv/
cp _build/x86-64/rootfs.erofs /tmp/image-root/opt/cloudhv/
cp installer/install.sh /tmp/image-root/opt/cloudhv/
chmod +x /tmp/image-root/opt/cloudhv/containerd-shim-cloudhv-v1
chmod +x /tmp/image-root/opt/cloudhv/install.sh
docker build -t ghcr.io/${GHCR_OWNER}/cloudhv-installer-dev:${SHA} \
  -f installer/Dockerfile /tmp/image-root
docker push ghcr.io/${GHCR_OWNER}/cloudhv-installer-dev:${SHA}
ENDSCRIPT
```

If the push fails with `permission_denied`, the user needs to:
1. Run `gh auth refresh --scopes write:packages` locally
2. Then re-authenticate Docker on the dev VM:
   ```bash
   TOKEN=$(gh auth token)
   ssh ${REMOTE_HOST} "echo '${TOKEN}' | docker login ghcr.io -u ${GHCR_OWNER} --password-stdin"
   ```

### 5. Ensure Dev Image Is Pullable

The `cloudhv-installer-dev` package is created as **private** by default.
Either make it public via GitHub UI, or create an image pull secret on AKS:

```bash
GH_TOKEN=$(gh auth token)
kubectl create secret docker-registry ghcr-secret \
  --docker-server=ghcr.io \
  --docker-username="${GHCR_OWNER}" \
  --docker-password="${GH_TOKEN}" \
  -n kube-system
```

### 6. Deploy to AKS

```bash
az account set --subscription "${AZURE_SUBSCRIPTION}"
az aks get-credentials --resource-group "${AKS_RESOURCE_GROUP}" --name "${AKS_CLUSTER_NAME}"

# Update the installer DaemonSet image and add pull secret
kubectl set image daemonset/cloudhv-installer -n kube-system \
  installer=ghcr.io/${GHCR_OWNER}/cloudhv-installer-dev:${SHA}

kubectl patch daemonset cloudhv-installer -n kube-system --type=json \
  -p '[{"op":"add","path":"/spec/template/spec/imagePullSecrets","value":[{"name":"ghcr-secret"}]}]'
```

Wait for the installer to roll out and verify:

```bash
kubectl rollout status daemonset/cloudhv-installer -n kube-system --timeout=120s
kubectl logs -n kube-system -l app=cloudhv-installer --tail=15
```

The logs should end with:
```
[cloudhv] Installation complete on <node>
[cloudhv] Installer idle. Shim is active.
```

### 7. Validate

Run whatever test is appropriate. For lifecycle validation, the Agent
Sandbox test is the standard:

```bash
kubectl port-forward svc/sandbox-router-svc 8080:8080 &
python3 example/agent-sandbox/run_fibonacci.py
# Run multiple times to verify consecutive runs work
```

### 8. Cleanup

Delete the AKS cluster when done:

```bash
az group delete --name "${AKS_RESOURCE_GROUP}" --yes --no-wait
```

## Important Notes

- **Always use SHA tags** for dev images. Mutable tags like `latest` or
  `pr82-test` cause stale image issues on AKS nodes.
- **Static linking is required.** The dev VM (Ubuntu) has a newer glibc
  than AKS nodes (AzureLinux). A dynamically linked shim will fail with
  `GLIBC_2.39 not found`.
- The `hacks/build-static-rust.sh` script runs inside an Alpine container,
  producing a fully static musl binary.
- The `hacks/build-guest.sh` script also runs containerized (Alpine),
  building the kernel and rootfs.erofs.
- For ARM64 testing, pass `--arch arm64` to the build scripts.
