"""
Agent Sandbox + CloudHV: Execute Python code in a VM-isolated sandbox on AKS.

This script uses the k8s-agent-sandbox Python SDK to create a sandbox pod
running under the CloudHV runtime (VM isolation), then executes Python code
inside it — including computing Fibonacci numbers, file I/O, and system info.

Usage:
    python3 run_fibonacci.py
"""

from k8s_agent_sandbox import SandboxClient


def main():
    print("Connecting to sandbox (cloudhv VM-isolated)...")

    # Developer mode: uses kubectl port-forward to reach the sandbox router.
    # For production, pass gateway_name="your-gateway" instead.
    with SandboxClient(
        template_name="python-cloudhv-template",
        namespace="default",
    ) as sandbox:
        print("Sandbox ready!\n")

        # --- 1. Hello from the VM ---
        print("--- Running: Hello from CloudHV ---")
        result = sandbox.run(
            "import sys, platform; "
            "print(f'Hello from a Cloud Hypervisor VM sandbox!'); "
            "print(f'Python {sys.version.split()[0]} | {platform.system()} {platform.release()}')"
        )
        print(result.stdout)

        # --- 2. Fibonacci sequence ---
        print("--- Running: Fibonacci sequence ---")
        fib_code = """
def fibonacci(n):
    if n <= 1:
        return n
    a, b = 0, 1
    for _ in range(2, n + 1):
        a, b = b, a + b
    return b

for n in [0, 5, 10, 20, 30]:
    print(f"fib({n:<3}) = {fibonacci(n)}")
"""
        result = sandbox.run(fib_code)
        print(result.stdout)

        # --- 3. File I/O (persistent within the sandbox session) ---
        print("--- Running: File I/O in sandbox ---")
        result = sandbox.run(
            "path = '/tmp/hello.txt'\n"
            "with open(path, 'w') as f:\n"
            "    f.write('Hello from CloudHV Agent Sandbox!')\n"
            "print(f'Wrote greeting to {path}')\n"
            "with open(path) as f:\n"
            "    print(f'Read back: {f.read()}')\n"
        )
        print(result.stdout)

        # --- 4. System info (shows we're inside a VM) ---
        print("--- Running: System info ---")
        result = sandbox.run(
            "import socket, os\n"
            "print(f'Hostname: {socket.gethostname()}')\n"
            "with open('/proc/uptime') as f:\n"
            "    uptime = float(f.read().split()[0])\n"
            "    print(f'Uptime: {uptime:.2f} seconds')\n"
            "with open('/proc/meminfo') as f:\n"
            "    lines = f.readlines()\n"
            "    total = int(lines[0].split()[1]) // 1024\n"
            "    avail = int(lines[2].split()[1]) // 1024\n"
            "    print(f'Memory: {total} MB total, {avail} MB available')\n"
        )
        print(result.stdout)

    print("Done! Sandbox cleaned up.")


if __name__ == "__main__":
    main()
