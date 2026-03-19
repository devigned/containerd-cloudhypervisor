---
name: aks-agent-sandbox
description: |
  Deploy an Agent Sandbox (k8s-sigs/agent-sandbox) environment on AKS with
  the CloudHV runtime, then run the Python SDK example to validate end-to-end
  sandbox creation, code execution, and cleanup. Use this when asked to test
  Agent Sandbox on AKS or benchmark sandbox startup times.
---

# AKS Agent Sandbox Deployment and Testing

## When to Use

Use this skill when asked to:
- Deploy Agent Sandbox with CloudHV on AKS
- Run the Agent Sandbox Python SDK example
- Benchmark sandbox startup/execution times
- Validate consecutive sandbox runs (lifecycle testing)

## Inputs

The user must provide:

- **AZURE_SUBSCRIPTION**: Azure subscription name
- **SHIM_VERSION**: Release tag (e.g. `v0.9.0`) or dev image tag (e.g. `40ca686`)
- **GHCR_OWNER**: GitHub Container Registry owner (e.g. `devigned`)

Optional:
- **REGION**: Azure region (default: `westus3`)
- **VM_SIZE**: Node VM size (default: `Standard_D4s_v5`)
- **RUN_COUNT**: Number of consecutive benchmark runs (default: `5`)

## Procedure

### 1. Create AKS Cluster

```bash
REGION="${REGION:-westus3}"
VM_SIZE="${VM_SIZE:-Standard_D4s_v5}"
RG="rg-cloudhv-sandbox"

az account set --subscription "${AZURE_SUBSCRIPTION}"
az group create --name "$RG" --location "$REGION"

az aks create --resource-group "$RG" --name cloudhv-sandbox \
  --location "$REGION" --node-count 1 --node-vm-size Standard_D2s_v5 \
  --nodepool-name system --generate-ssh-keys --network-plugin azure \
  --os-sku AzureLinux

az aks nodepool add --resource-group "$RG" --cluster-name cloudhv-sandbox \
  --name cloudhv --node-count 1 --node-vm-size "$VM_SIZE" \
  --max-pods 30 --labels workload=cloudhv --os-sku AzureLinux \
  --node-taints workload=cloudhv:NoSchedule

az aks get-credentials --resource-group "$RG" --name cloudhv-sandbox --overwrite-existing
```

### 2. Install CloudHV Shim

For a **released version**:

```bash
cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: cloudhv-installer
  namespace: kube-system
spec:
  selector:
    matchLabels:
      app: cloudhv-installer
  template:
    metadata:
      labels:
        app: cloudhv-installer
    spec:
      hostNetwork: true
      hostPID: true
      nodeSelector:
        workload: cloudhv
      tolerations:
      - operator: Exists
      containers:
      - name: installer
        image: ghcr.io/${GHCR_OWNER}/cloudhv-installer:${SHIM_VERSION}
        securityContext:
          privileged: true
        resources:
          requests:
            cpu: 100m
            memory: 128Mi
          limits:
            cpu: "1"
            memory: 1Gi
        volumeMounts:
        - name: host-root
          mountPath: /host
      volumes:
      - name: host-root
        hostPath:
          path: /
EOF
```

For a **dev image** (pre-release testing), also add an imagePullSecret.
See the `pre-release-testing` skill for details.

Wait for installation:

```bash
kubectl -n kube-system rollout status daemonset/cloudhv-installer --timeout=180s
kubectl logs -n kube-system -l app=cloudhv-installer --tail=15
```

### 3. Deploy Agent Sandbox Components

```bash
# Install the agent-sandbox-controller CRD and controller
kubectl apply -f https://raw.githubusercontent.com/k8s-sigs/agent-sandbox/main/deploy/install.yaml

# Wait for controller
kubectl -n agent-sandbox-system rollout status deployment/agent-sandbox-controller --timeout=120s
```

### 4. Deploy Sandbox Template and Router

```bash
kubectl apply -f example/agent-sandbox/python-sandbox-template.yaml
kubectl apply -f example/agent-sandbox/sandbox-router.yaml

# Wait for router
kubectl rollout status deployment/sandbox-router-deployment --timeout=120s
```

### 5. Run Agent Sandbox Example

```bash
# Port-forward the router service
kubectl port-forward svc/sandbox-router-svc 8080:8080 &
PF_PID=$!
sleep 3

# Install the Python SDK (if not already installed)
pip3 install k8s-agent-sandbox

# Run the example
python3 example/agent-sandbox/run_fibonacci.py
```

Expected output includes 4 test blocks:
1. Hello from CloudHV (Python version, kernel version)
2. Fibonacci sequence (fib(30)=832040)
3. File I/O (write and read back)
4. System info (hostname, memory, VM uptime)

### 6. Benchmark: Consecutive Runs with Timing

Run multiple consecutive sandbox create/execute/destroy cycles to
validate lifecycle correctness and measure timing:

```python
import time
from k8s_agent_sandbox import SandboxClient

RUN_COUNT = int("${RUN_COUNT:-5}")

for i in range(1, RUN_COUNT + 1):
    t0 = time.time()
    with SandboxClient(
        template_name="python-cloudhv-template",
        namespace="default",
    ) as sandbox:
        t_connect = time.time() - t0
        result = sandbox.run("python3 -c 'def fib(n):\\n a,b=0,1\\n for _ in range(n): a,b=b,a+b\\n return a\\nprint(fib(30))'")
        t_exec = time.time() - t0 - t_connect
        uptime = sandbox.run("cat /proc/uptime").stdout.strip().split()[0]
        kernel = sandbox.run("uname -r").stdout.strip()
        print(f"Run {i}: connect={t_connect:.1f}s exec={t_exec:.1f}s "
              f"total={time.time()-t0:.1f}s uptime={uptime}s "
              f"kernel={kernel} fib(30)={result.stdout.strip()}")
    time.sleep(5)
```

Key metrics to capture:
- **connect time**: Time from SandboxClient creation to sandbox ready
- **exec time**: Time to execute a Python snippet
- **total time**: End-to-end per-run time
- **VM uptime**: Guest kernel uptime at first exec (shows boot latency)
- **kernel version**: Verify expected guest kernel
- **consecutive success rate**: All N runs must succeed

### 7. Collect Memory Utilization

After a successful run (while a sandbox pod is still running), collect
per-process RSS from the node:

```bash
NODE=$(kubectl get nodes -l agentpool=cloudhv -o jsonpath='{.items[0].metadata.name}')
kubectl debug node/$NODE --profile=sysadmin -it --image=busybox -- chroot /host sh -c '
for pid in $(pgrep cloud-hyper); do
  echo "CH PID $pid:"; grep -E "VmRSS|RssShmem" /proc/$pid/status
done
for pid in $(pgrep containerd-shim-cloudhv); do
  rss=$(grep VmRSS /proc/$pid/status | awk "{print \$2}")
  [ "$rss" -gt 3000 ] && echo "Shim PID $pid: VmRSS ${rss} kB"
done
'
```

### 8. Generate Report

Save results to `reports/agent-sandbox-timing-cloudhv-aks.md` with:
- Test configuration (VM size, shim version, kernel version)
- Per-run timing table
- Aggregate statistics (mean, min, max for connect/exec/total)
- Memory utilization (per-pod RSS breakdown)
- Consecutive run success rate
- Comparison with Kata if data is available

### 9. Cleanup

```bash
az group delete --name "$RG" --yes --no-wait
```

## Important Notes

- **Consecutive runs are the key test.** The shim had a lifecycle bug where
  the second sandbox creation would fail. If any consecutive run fails,
  investigate containerd logs for "invalid state transition" errors.
- **Sandbox startup is ~20s** — dominated by the Python runtime server
  boot inside the VM, not VM boot itself (~2-3s).
- The SandboxTemplate sets `automountServiceAccountToken: false` for
  security (no k8s API access from inside the sandbox).
- The sandbox router runs as a regular pod (not under CloudHV runtime).
- Port 8080 is the router's service port, not 3000.
- Each sandbox gets its own VM with isolated kernel, network, and
  filesystem — true VM-level isolation, not just namespace isolation.
