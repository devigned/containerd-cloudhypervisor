# Agent Sandbox Timing Breakdown: Kata on AKS

## Test Configuration

| Parameter | Value |
|-----------|-------|
| **Runtime** | Kata (AKS pod sandboxing, `kata-vm-isolation`) |
| **Node** | Standard_D4s_v5 (4 vCPU, 16 GiB RAM) |
| **Sandbox image** | python-runtime-sandbox (agent-sandbox project) |
| **Pod resources** | 100m CPU req, 64Mi mem req, 256Mi mem limit |
| **K8s** | AKS, AzureLinux, westus3 |
| **Guest kernel** | 6.6.96.mshv1-3.azl3 |
| **Runs** | 5 consecutive (no manual cleanup between runs) |

## Results

| Run | Connect (s) | Execute (s) | Total (s) |
|-----|------------|------------|----------|
| 1 | 19.8 | 0.9 | 20.6 |
| 2 | 20.9 | 0.7 | 21.6 |
| 3 | 20.1 | 0.3 | 20.4 |
| 4 | 19.5 | 1.0 | 20.5 |
| 5 | 19.7 | 0.5 | 20.3 |
| **Avg** | **20.0** | **0.7** | **20.7** |

All 5 runs succeeded consecutively with no manual intervention.

## Time Breakdown

The ~20s connect time includes everything from SDK `SandboxClient.__enter__`
to the sandbox pod passing its readiness probe. Breaking this down:

| Phase | Est. Time | Notes |
|-------|----------|-------|
| SandboxClaim → Sandbox CR | ~0.5s | Controller reconciliation |
| Sandbox CR → Pod created | ~0.5s | Controller creates pod |
| Pod scheduled | ~0.5s | Scheduler assigns to node |
| Image pull (cached) | ~1s | Already on node after run 1 |
| Sandbox creation (pause container) | ~2s | Kata boots VM for pause |
| Container creation | ~3s | Kata creates container in VM via virtiofsd |
| Python server startup (uvicorn) | ~10s | Python imports + Flask/uvicorn init on 1 vCPU |
| Readiness probe passes | ~2s | HTTP GET / returns 200 |
| SDK tunnel established | ~0.5s | kubectl port-forward to router |

The dominant cost is **Python server startup (~10s)** inside the VM. The
uvicorn server imports Flask, sets up routes, and binds to port 8888. On a
single vCPU VM, Python's import machinery is slow.

The second largest cost is **VM boot + container setup (~5s)**, which includes
Cloud Hypervisor boot, Kata agent init, and virtiofsd rootfs sharing.

## Code Execution

Once the sandbox is ready, code execution is fast:

| Operation | Time |
|-----------|------|
| `sandbox.run()` with Fibonacci (fib(30)=832040) | 0.3–1.0s |
| Round-trip: SDK → router → sandbox pod → python → response | ~0.5s |

The variance (0.3–1.0s) is mostly network latency through the port-forward
tunnel and the router proxy, not computation time.

## Memory Utilization

Per-pod RSS measured via `/proc/PID/status` on the AKS Kata node while the
sandbox was running (pod status `1/1 Running`):

| Process | VmRSS | RssShmem |
|---------|-------|----------|
| Cloud Hypervisor VMM | 273 MB | 262 MB (= 256 MiB guest mmap) |
| Kata shim (containerd-shim-kata-v2) | 52 MB | — |
| virtiofsd (instance 1) | 4 MB | — |
| virtiofsd (instance 2) | 36 MB | — |
| **Total per pod** | **~365 MB** | — |

Guest `/proc/meminfo` (inside the VM):

| Metric | Value |
|--------|-------|
| MemTotal | 230 MB |
| MemFree | 103 MB |
| MemAvailable | 164 MB |
| Cached | 65 MB |
| Active | 15 MB |

Kata sizes the VM to the pod's `limits.memory` (256Mi), but the guest sees
230 MB total (26 MB reserved for kernel/firmware). RssShmem = 262 MB confirms
the full 256 MiB mmap is touched. The second virtiofsd instance (36 MB) holds
the rootfs page cache. The 600Mi AKS RuntimeClass overhead is ~1.6× the
actual per-pod RSS (~365 MB).

Node-level: 1081 MiB used (7% of 16 GiB) with one sandbox pod running,
59m CPU (1%).

## Observations

1. **Consecutive runs work cleanly.** Kata handles sandbox lifecycle
   (create → run → delete → create) without stale state. All 5 runs
   succeeded back-to-back.

2. **VM uptime at first execution: ~20s.** This matches the connect time,
   confirming the VM boots early in the pod lifecycle and the ~10s of
   Python startup happens inside the already-booted VM.

3. **The bottleneck is Python, not the VM.** A lighter sandbox runtime
   (e.g., Node.js or a compiled binary) would reduce the ~10s startup
   to <1s, making the total connect time ~10s instead of ~20s.

4. **Memory: 256 MiB guest.** Kata sizes the VM to `limits.memory` (256Mi).
   The VM uptime of ~20s means guest memory is fully allocated by the time
   execution starts (virtiofsd page cache fills available memory).

---

*Benchmark performed on AKS, westus3, extremis subscription.*
*Kata runtime: kata-vm-isolation, AzureLinux, MSHV hypervisor.*
*Agent Sandbox: k8s-sigs/agent-sandbox v0.2.1, python-runtime-sandbox image.*
