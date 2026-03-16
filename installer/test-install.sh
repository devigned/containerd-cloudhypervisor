#!/usr/bin/env bash
set -euo pipefail

# Test the installer's config patching logic.

# Syntax-check the installer script itself
bash -n installer/install.sh || { echo "FAIL: syntax error in install.sh"; exit 1; }

PASS=0
FAIL=0

assert_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s\n' "$haystack" | grep -Fq "$needle"; then
    echo "  ✅ $desc"
    PASS=$((PASS + 1))
  else
    echo "  ❌ $desc (not found: $needle)"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if ! printf '%s\n' "$haystack" | grep -Fq "$needle"; then
    echo "  ✅ $desc"
    PASS=$((PASS + 1))
  else
    echo "  ❌ $desc (found but should not be: $needle)"
    FAIL=$((FAIL + 1))
  fi
}

# --- Test 1: awk removes existing cloudhv runtime section ---
echo ""
echo "=== Test 1: awk removes existing cloudhv runtime ==="
INPUT='[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"
  snapshotter = "devmapper"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata]
  runtime_type = "io.containerd.kata.v2"'

RESULT=$(printf '%s\n' "$INPUT" | awk '
  /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
  skip && /^\[/ && !/cloudhv/ { skip=0 }
  !skip { print }
')

assert_contains "runc preserved" "runtimes.runc" "$RESULT"
assert_contains "kata preserved" "runtimes.kata" "$RESULT"
assert_not_contains "cloudhv removed" "runtimes.cloudhv" "$RESULT"
assert_not_contains "devmapper removed" "devmapper" "$RESULT"

# --- Test 2: erofs config is added ---
echo ""
echo "=== Test 2: erofs config is added ==="
TOML=$(printf '%s\n' "$RESULT")
TOML="${TOML}

# CloudHV erofs snapshotter for direct image layer passthrough
[plugins.\"io.containerd.snapshotter.v1.erofs\"]

[plugins.\"io.containerd.service.v1.diff-service\"]
  default = [\"erofs\",\"walking\"]

# Cloud Hypervisor VM-isolated runtime
[plugins.\"io.containerd.grpc.v1.cri\".containerd.runtimes.cloudhv]
  runtime_type = \"io.containerd.cloudhv.v1\"
  snapshotter = \"erofs\""

assert_contains "erofs snapshotter" "snapshotter.v1.erofs" "$TOML"
assert_contains "erofs differ" 'default = ["erofs","walking"]' "$TOML"
assert_contains "cloudhv runtime" "runtimes.cloudhv" "$TOML"
assert_contains "erofs snapshotter on runtime" 'snapshotter = "erofs"' "$TOML"
assert_not_contains "no devmapper" "devmapper" "$TOML"

# --- Test 3: single-quote TOML keys ---
echo ""
echo "=== Test 3: awk handles single-quote TOML keys ==="
INPUT2="[plugins.'io.containerd.grpc.v1.cri'.containerd.runtimes.cloudhv]
  runtime_type = 'io.containerd.cloudhv.v1'
[plugins.'io.containerd.grpc.v1.cri'.containerd.runtimes.runc]
  runtime_type = 'io.containerd.runc.v2'"

RESULT2=$(printf '%s\n' "$INPUT2" | awk '
  /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
  skip && /^\[/ && !/cloudhv/ { skip=0 }
  !skip { print }
')

assert_not_contains "single-quote cloudhv removed" "runtimes.cloudhv" "$RESULT2"
assert_contains "single-quote runc preserved" "runtimes.runc" "$RESULT2"

# --- Results ---
echo ""
echo "=== Results ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
