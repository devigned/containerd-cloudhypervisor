#!/usr/bin/env bash
set -euo pipefail

# Test the installer's config patching logic without requiring a real host.
# This runs in CI to catch syntax errors and config generation bugs.

PASS=0
FAIL=0

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "  ✅ $desc"
    PASS=$((PASS + 1))
  else
    echo "  ❌ $desc"
    echo "     expected: $expected"
    echo "     actual:   $actual"
    FAIL=$((FAIL + 1))
  fi
}

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
    echo "  ❌ $desc (found: $needle)"
    FAIL=$((FAIL + 1))
  fi
}

TMPDIR=$(mktemp -d)
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

echo "=== Test 1: install.sh has valid bash syntax ==="
if bash -n installer/install.sh; then
  assert_eq "syntax check" "0" "0"
else
  assert_eq "syntax check" "0" "1"
fi

echo ""
echo "=== Test 2: awk removes cloudhv section idempotently ==="
cat > "$TMPDIR/config.toml" << 'TOML'
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"
  snapshotter = "devmapper"

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata]
  runtime_type = "io.containerd.kata.v2"
TOML

awk '
  /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
  skip && /^\[/ && !/cloudhv/ { skip=0 }
  !skip { print }
' "$TMPDIR/config.toml" > "$TMPDIR/result.toml"

RESULT=$(cat "$TMPDIR/result.toml")
assert_contains "runc preserved" "runtimes.runc" "$RESULT"
assert_contains "kata preserved" "runtimes.kata" "$RESULT"
assert_not_contains "cloudhv removed" "runtimes.cloudhv" "$RESULT"
assert_not_contains "snapshotter removed" "snapshotter" "$RESULT"

echo ""
echo "=== Test 3: awk removes devmapper snapshotter section ==="
cat > "$TMPDIR/config2.toml" << 'TOML'
[plugins."io.containerd.snapshotter.v1.overlayfs"]
  root_path = "/var/lib/containerd/overlayfs"

[plugins."io.containerd.snapshotter.v1.devmapper"]
  root_path = "/var/lib/containerd/devmapper"
  pool_name = "cloudhv-pool"

[metrics]
  address = "0.0.0.0:1234"
TOML

awk '
  /^\[.*\.snapshotter.*devmapper/ { skip=1; next }
  skip && /^\[/ && !/devmapper/ { skip=0 }
  !skip { print }
' "$TMPDIR/config2.toml" > "$TMPDIR/result2.toml"

RESULT2=$(cat "$TMPDIR/result2.toml")
assert_contains "overlayfs preserved" "overlayfs" "$RESULT2"
assert_contains "metrics preserved" "metrics" "$RESULT2"
assert_not_contains "devmapper removed" "devmapper" "$RESULT2"

echo ""
echo "=== Test 4: awk is idempotent (no cloudhv to remove) ==="
cat > "$TMPDIR/config3.toml" << 'TOML'
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"
TOML

awk '
  /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
  skip && /^\[/ && !/cloudhv/ { skip=0 }
  !skip { print }
' "$TMPDIR/config3.toml" > "$TMPDIR/result3.toml"

RESULT3=$(cat "$TMPDIR/result3.toml")
assert_contains "runc still there" "runtimes.runc" "$RESULT3"

echo ""
echo "=== Test 5: awk handles single-quote TOML keys ==="
cat > "$TMPDIR/config4.toml" << 'TOML'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"
TOML

awk '
  /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
  skip && /^\[/ && !/cloudhv/ { skip=0 }
  !skip { print }
' "$TMPDIR/config4.toml" > "$TMPDIR/result4.toml"

RESULT4=$(cat "$TMPDIR/result4.toml")
assert_not_contains "single-quote cloudhv removed" "cloudhv" "$RESULT4"
assert_contains "single-quote runc preserved" "runtimes.runc" "$RESULT4"

echo ""
echo "=== Results ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
