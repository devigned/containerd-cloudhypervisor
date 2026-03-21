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

- **REMOTE_HOST**: SSH target for the dev VM (default: `hl-dev`)
- **GHCR_OWNER**: GitHub Container Registry owner (default: `devigned`)
- **AKS_RESOURCE_GROUP**: Azure resource group containing the AKS cluster
- **AKS_CLUSTER_NAME**: Name of the AKS cluster
- **AZURE_SUBSCRIPTION**: Azure subscription name to use

## Procedure

### 1. Build Dev Image (Single Command)

Use the `build-dev-image.sh` script. This is the **only** way to build
dev images — it ensures all artifacts (shim, agent, kernel, rootfs) are
built from the same commit, verifies architecture, and pushes with the
correct SHA tag.

```bash
bash hacks/build-dev-image.sh --remote "${REMOTE_HOST}" --owner "${GHCR_OWNER}"
```

The script will:
1. Sync code to the dev VM (excluding build artifacts)
2. Force-rebuild the static shim (never uses cached binary)
3. Verify the shim is x86-64 (fails if ARM64)
4. Build the guest kernel + rootfs (with the matching agent)
5. Build and push the Docker image tagged with the git SHA
6. Print the exact deploy commands

**NEVER** build the shim, guest, or image separately. The script ensures
version consistency.

rsync exit code 23 (partial transfer) is normal — it's caused by
vanishing files in `target/`. Ignore it.

If the push fails with `permission_denied`, the user needs to:
1. Run `gh auth refresh --scopes write:packages` locally
2. Then re-authenticate Docker on the dev VM:
   ```bash
   TOKEN=$(gh auth token)
   ssh ${REMOTE_HOST} "echo '${TOKEN}' | docker login ghcr.io -u ${GHCR_OWNER} --password-stdin"
   ```

### 2. Ensure Dev Image Is Pullable

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

### 3. Deploy to AKS

The `build-dev-image.sh` output prints the exact commands. For a fresh install:

```bash
SHA=$(git rev-parse --short HEAD)
az account set --subscription "${AZURE_SUBSCRIPTION}"
az aks get-credentials --resource-group "${AKS_RESOURCE_GROUP}" --name "${AKS_CLUSTER_NAME}"

helm install cloudhv-installer \
  oci://ghcr.io/${GHCR_OWNER}/charts/cloudhv-installer \
  --version 0.5.3 --namespace kube-system \
  --set image.repository=ghcr.io/${GHCR_OWNER}/cloudhv-installer-dev \
  --set image.tag=${SHA} \
  --set "imagePullSecrets[0].name=ghcr-secret"
```

For updating an existing install:

```bash
kubectl set image daemonset/cloudhv-installer -n kube-system \
  installer=ghcr.io/${GHCR_OWNER}/cloudhv-installer-dev:${SHA}
```

Wait for the installer to roll out and verify:

```bash
kubectl rollout status daemonset/cloudhv-installer -n kube-system --timeout=120s
kubectl logs -n kube-system -l app.kubernetes.io/name=cloudhv-installer --tail=15
```

The logs should end with:
```
[cloudhv] Installation complete on <node>
[cloudhv] Installer idle. Shim is active.
```

### 4. Validate

Run whatever test is appropriate. For lifecycle validation, the Agent
Sandbox test is the standard:

```bash
kubectl port-forward svc/sandbox-router-svc 8080:8080 &
python3 example/agent-sandbox/run_fibonacci.py
# Run multiple times to verify consecutive runs work
```

### 5. Cleanup

Delete the AKS cluster when done:

```bash
az group delete --name "${AKS_RESOURCE_GROUP}" --yes --no-wait
```

## Important Notes

- **Always use `build-dev-image.sh`** — never build components separately.
  This prevents version skew between shim, agent, kernel, and rootfs.
- **Always use SHA tags** for dev images. Mutable tags like `latest` or
  `pr82-test` cause stale image issues on AKS nodes.
- **Static linking is required.** The dev VM (Ubuntu) has a newer glibc
  than AKS nodes (AzureLinux). A dynamically linked shim will fail with
  `GLIBC_2.39 not found`.
- **Never patch the DaemonSet after install.** If you need different
  settings, re-run `helm install` with the correct `--set` values.
- The Makefile `sync` target excludes build artifacts (shim binary,
  vmlinux, rootfs.erofs) to prevent overwriting dev VM builds with
  local macOS ARM64 binaries.
- For ARM64 testing, pass `--arch arm64` to the build scripts.
