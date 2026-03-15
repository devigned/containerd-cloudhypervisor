# Agent Sandbox with CloudHV on AKS

This example demonstrates running the [Kubernetes Agent Sandbox](https://agent-sandbox.sigs.k8s.io/)
with Cloud Hypervisor VM isolation on Azure Kubernetes Service (AKS). It uses
the Python SDK to execute Python code inside a VM-isolated sandbox pod.

## Overview

Agent Sandbox provides a Kubernetes-native API for managing isolated, stateful
workloads — ideal for executing untrusted, LLM-generated code. By combining it
with CloudHV's VM isolation, each sandbox runs inside its own lightweight
microVM, providing hardware-level isolation with ~59 MB per-pod RSS overhead.

**Architecture:**

```
Your Python Code
    │
    ▼
k8s-agent-sandbox (Python SDK)
    │
    ▼
sandbox-router (ClusterIP Service)
    │  kubectl port-forward (dev mode)
    ▼
Sandbox Pod (runtimeClassName: cloudhv)
    │
    ▼
Cloud Hypervisor VM → Guest Agent → crun → python-runtime container
```

## Prerequisites

- Azure CLI authenticated with a subscription
- `kubectl` and `helm` installed
- Python 3.10+

## Setup

### 1. Create AKS Cluster with CloudHV

```bash
REGION="westus3"
RG="rg-agent-sandbox"
CLUSTER="agent-sandbox-demo"

az group create --name "$RG" --location "$REGION"

az aks create --resource-group "$RG" --name "$CLUSTER" --location "$REGION" \
  --node-count 1 --node-vm-size Standard_D2s_v5 --nodepool-name system \
  --generate-ssh-keys --network-plugin azure --os-sku AzureLinux

az aks nodepool add --resource-group "$RG" --cluster-name "$CLUSTER" \
  --name cloudhv --node-count 1 --node-vm-size Standard_D4s_v5 \
  --max-pods 30 --labels workload=cloudhv --os-sku AzureLinux

az aks get-credentials --resource-group "$RG" --name "$CLUSTER"
```

### 2. Install CloudHV Shim

```bash
helm install cloudhv-installer oci://ghcr.io/devigned/charts/cloudhv-installer \
  --version 0.7.0 --namespace kube-system
```

Wait for the installer to complete and verify the runtime is registered:

```bash
kubectl -n kube-system rollout status daemonset/cloudhv-installer --timeout=180s
kubectl get runtimeclass cloudhv
```

> **Note:** The installer's `dmsetup create` on loopback may hang. Check
> installer logs and manually complete setup if needed (see main project README).
> This is a known issue being tracked for a fix.

> **Note:** The `python-runtime-sandbox` image has been validated running inside
> CloudHV VMs on both hl-dev (crictl) and AKS. The `etc/hostname` mount issue
> was fixed in v0.7.1.

### 3. Install Agent Sandbox Controller

```bash
export VERSION="v0.2.1"

# Core controller + CRDs
kubectl apply -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/${VERSION}/manifest.yaml

# Extensions (SandboxTemplate, SandboxClaim, SandboxWarmPool)
kubectl apply -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/${VERSION}/extensions.yaml
```

### 4. Deploy the Sandbox Router

The router proxies requests from the Python SDK to individual sandbox pods.

```bash
kubectl apply -f sandbox-router.yaml
```

### 5. Deploy the SandboxTemplate

The template defines what each sandbox pod looks like — including the CloudHV
runtime class:

```bash
kubectl apply -f python-sandbox-template.yaml
```

### 6. Install the Python SDK

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install k8s-agent-sandbox
```

## Run the Example

```bash
python3 run_fibonacci.py
```

This script:
1. Creates a VM-isolated sandbox on AKS via the `cloudhv` RuntimeClass
2. Executes Python code that computes the Fibonacci sequence inside the VM
3. Writes a file and reads it back to demonstrate persistent sandbox state
4. Cleans up the sandbox on exit

**Expected output:**

```
Connecting to sandbox (cloudhv VM-isolated)...
Sandbox ready!

--- Running: Hello from CloudHV ---
Hello from a Cloud Hypervisor VM sandbox!
Python 3.13.x | Linux 6.12.8

--- Running: Fibonacci sequence ---
fib(0)  = 0
fib(5)  = 5
fib(10) = 55
fib(20) = 6765
fib(30) = 832040

--- Running: File I/O in sandbox ---
Wrote greeting to /tmp/hello.txt
Read back: Hello from CloudHV Agent Sandbox!

--- Running: System info ---
Hostname: sb-cloudhv-xxxx
Uptime: 0.42 seconds
Memory: 502 MB total, 461 MB available

Done! Sandbox cleaned up.
```

## How It Works

The `python-sandbox-template.yaml` specifies `runtimeClassName: cloudhv`, which
tells containerd to use the CloudHV shim. When the Agent Sandbox controller
creates a pod from this template:

1. containerd invokes the CloudHV shim
2. The shim spawns a Cloud Hypervisor VM with a minimal guest kernel
3. The container image (python-runtime-sandbox) runs inside the VM
4. The Python SDK communicates with the container via the sandbox router
5. Code execution happens inside the VM — isolated from the host kernel

Each sandbox pod consumes ~59 MB of host RSS (vs ~330 MB with Kata), enabling
high-density deployment of agent sandboxes.

## Cleanup

```bash
az group delete --name "$RG" --yes --no-wait
```
