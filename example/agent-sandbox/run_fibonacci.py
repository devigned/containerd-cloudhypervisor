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
            'python3 -c "'
            "import sys, platform; "
            "print(f'Hello from a Cloud Hypervisor VM sandbox!'); "
            "print(f'Python {sys.version.split()[0]} | {platform.system()} {platform.release()}')"
            '"'
        )
        print(result.stdout)
        if result.stderr:
            print(f"  (stderr: {result.stderr})")

        # --- 2. Fibonacci sequence ---
        print("--- Running: Fibonacci sequence ---")
        result = sandbox.run(
            "python3 -c 'a,b=0,1\n"
            "for n in range(31):\n"
            " if n in(0,5,10,20,30):print(f\"fib({n})={a}\")\n"
            " a,b=b,a+b'"
        )
        print(result.stdout)
        if result.stderr:
            print(f"  (stderr: {result.stderr})")

        # --- 3. File I/O (persistent within the sandbox session) ---
        print("--- Running: File I/O in sandbox ---")
        result = sandbox.run(
            'python3 -c "'
            "path = '/tmp/hello.txt'; "
            "open(path, 'w').write('Hello from CloudHV Agent Sandbox!'); "
            "print(f'Wrote greeting to {path}'); "
            "print(f'Read back: {open(path).read()}')"
            '"'
        )
        print(result.stdout)

        # --- 4. System info (shows we're inside a VM) ---
        print("--- Running: System info ---")
        result = sandbox.run(
            'python3 -c "'
            "import socket; "
            "print(f'Hostname: {socket.gethostname()}'); "
            "lines = open('/proc/meminfo').readlines(); "
            "total = int(lines[0].split()[1]) // 1024; "
            "avail = int(lines[2].split()[1]) // 1024; "
            "print(f'Memory: {total} MB total, {avail} MB available'); "
            "uptime = float(open('/proc/uptime').read().split()[0]); "
            "print(f'VM uptime: {uptime:.2f} seconds')"
            '"'
        )
        print(result.stdout)

    print("Done! Sandbox cleaned up.")


if __name__ == "__main__":
    main()
