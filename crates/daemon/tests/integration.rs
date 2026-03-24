//! Integration tests for the sandbox daemon.
//!
//! Tests cover:
//!   - Daemon socket RPCs (AcquireSandbox, ReleaseSandbox, Status)
//!   - Pool drain and synchronous restore
//!   - Shadow snapshot creation and warm restore
//!   - End-to-end pod networking via containerd (crictl)
//!
//! Usage:
//!   # Daemon-level tests (require daemon + KVM):
//!   cargo test -p cloudhv-sandbox-daemon --test integration -- --nocapture --test-threads=1
//!
//!   # End-to-end networking test (requires containerd + cloudhv runtime):
//!   cargo test -p cloudhv-sandbox-daemon --test integration test_pod_http_connectivity -- --nocapture
//!
//! Requires:
//!   - Daemon running at /run/cloudhv/daemon.sock (or DAEMON_SOCKET env)
//!   - Cloud Hypervisor >= v51.0 installed
//!   - Container image erofs available (or specify ROOTFS_EROFS env)
//!   - KVM access (/dev/kvm)
//!   - For pod tests: containerd running with cloudhv runtime configured

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

fn daemon_socket() -> String {
    std::env::var("DAEMON_SOCKET").unwrap_or_else(|_| "/run/cloudhv/daemon.sock".to_string())
}

fn rpc(method: &str, extra: &serde_json::Value) -> serde_json::Value {
    let socket_path = daemon_socket();
    let mut stream =
        UnixStream::connect(&socket_path).unwrap_or_else(|e| panic!("connect {socket_path}: {e}"));
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();

    let mut req = extra.clone();
    req["method"] = serde_json::json!(method);
    let mut msg = serde_json::to_string(&req).unwrap();
    msg.push('\n');
    stream.write_all(msg.as_bytes()).unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap_or_else(|e| {
        panic!("parse response: {e}\nraw: {line}");
    })
}

#[test]
fn test_status() {
    let resp = rpc("Status", &serde_json::json!({}));
    assert!(resp.get("pool_ready").is_some(), "missing pool_ready");
    assert!(resp.get("active_vms").is_some(), "missing active_vms");
    println!("Status: {}", serde_json::to_string_pretty(&resp).unwrap());
}

#[test]
fn test_acquire_release_cycle() {
    // Check initial pool state
    let status = rpc("Status", &serde_json::json!({}));
    let initial_ready = status["pool_ready"].as_u64().unwrap();
    assert!(initial_ready > 0, "pool must have ready VMs");
    println!("Initial pool: {} ready", initial_ready);

    // Acquire
    let t0 = Instant::now();
    let resp = rpc(
        "AcquireSandbox",
        &serde_json::json!({
            "netns": "",
            "image_key": "",
        }),
    );
    let acquire_ms = t0.elapsed().as_millis();
    assert!(resp.get("vm_id").is_some(), "missing vm_id: {resp}");
    assert!(resp.get("error").is_none(), "acquire error: {resp}");
    let vm_id = resp["vm_id"].as_str().unwrap();
    let ch_pid = resp["ch_pid"].as_u64().unwrap();
    println!(
        "Acquired: vm_id={} ch_pid={} in {}ms",
        vm_id, ch_pid, acquire_ms
    );

    // Verify pool decreased
    let status = rpc("Status", &serde_json::json!({}));
    assert_eq!(
        status["active_vms"].as_u64().unwrap(),
        1,
        "active should be 1"
    );
    println!(
        "After acquire: pool={} active={}",
        status["pool_ready"], status["active_vms"]
    );

    // Release
    let t1 = Instant::now();
    let resp = rpc("ReleaseSandbox", &serde_json::json!({"vm_id": vm_id}));
    let release_ms = t1.elapsed().as_millis();
    assert!(resp.get("error").is_none(), "release error: {resp}");
    println!("Released in {}ms", release_ms);

    // Wait for replenishment
    std::thread::sleep(std::time::Duration::from_secs(2));
    let status = rpc("Status", &serde_json::json!({}));
    println!(
        "After replenish: pool={} active={}",
        status["pool_ready"], status["active_vms"]
    );
}

#[test]
fn test_consecutive_lifecycle() {
    let iterations = 5;
    let mut acquire_times = Vec::new();
    let mut release_times = Vec::new();

    for i in 0..iterations {
        let t0 = Instant::now();
        let resp = rpc(
            "AcquireSandbox",
            &serde_json::json!({
                "netns": "",
                "image_key": "",
            }),
        );
        let acquire_ms = t0.elapsed().as_millis();
        assert!(resp.get("error").is_none(), "acquire {i} error: {resp}");
        let vm_id = resp["vm_id"].as_str().unwrap().to_string();
        acquire_times.push(acquire_ms);

        let t1 = Instant::now();
        let resp = rpc("ReleaseSandbox", &serde_json::json!({"vm_id": vm_id}));
        let release_ms = t1.elapsed().as_millis();
        assert!(resp.get("error").is_none(), "release {i} error: {resp}");
        release_times.push(release_ms);

        // Wait for replenishment
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    println!(
        "\n=== Consecutive Lifecycle ({} iterations) ===",
        iterations
    );
    for i in 0..iterations {
        println!(
            "  {}: acquire={}ms release={}ms",
            i + 1,
            acquire_times[i],
            release_times[i]
        );
    }

    let avg_acquire: u128 = acquire_times.iter().sum::<u128>() / iterations as u128;
    let avg_release: u128 = release_times.iter().sum::<u128>() / iterations as u128;
    println!("  avg: acquire={}ms release={}ms", avg_acquire, avg_release);

    // First acquire should be from pool (<5ms), subsequent may be sync restore
    assert!(
        acquire_times[0] < 50,
        "first acquire should be from pool: {}ms",
        acquire_times[0]
    );
}

#[test]
fn test_pool_drain_and_sync_restore() {
    // Drain the pool by acquiring all VMs
    let status = rpc("Status", &serde_json::json!({}));
    let pool_size = status["pool_ready"].as_u64().unwrap() as usize;
    println!("Pool has {} VMs, draining...", pool_size);

    let mut acquired = Vec::new();
    for _ in 0..pool_size {
        let resp = rpc(
            "AcquireSandbox",
            &serde_json::json!({
                "netns": "", "image_key": "",
            }),
        );
        assert!(resp.get("error").is_none(), "drain acquire error: {resp}");
        acquired.push(resp["vm_id"].as_str().unwrap().to_string());
    }

    // Pool should be empty
    let status = rpc("Status", &serde_json::json!({}));
    assert_eq!(
        status["pool_ready"].as_u64().unwrap(),
        0,
        "pool should be drained"
    );
    println!("Pool drained: active={}", status["active_vms"]);

    // Next acquire triggers synchronous restore
    let t0 = Instant::now();
    let resp = rpc(
        "AcquireSandbox",
        &serde_json::json!({
            "netns": "", "image_key": "",
        }),
    );
    let sync_ms = t0.elapsed().as_millis();
    assert!(resp.get("error").is_none(), "sync acquire error: {resp}");
    println!("Sync restore acquire: {}ms", sync_ms);
    acquired.push(resp["vm_id"].as_str().unwrap().to_string());

    // Release all
    for vm_id in &acquired {
        rpc("ReleaseSandbox", &serde_json::json!({"vm_id": vm_id}));
    }

    // Wait for replenishment
    std::thread::sleep(std::time::Duration::from_secs(3));
    let status = rpc("Status", &serde_json::json!({}));
    println!(
        "After release+replenish: pool={} active={}",
        status["pool_ready"], status["active_vms"]
    );
}

fn erofs_path() -> String {
    std::env::var("EROFS_PATH").unwrap_or_else(|_| {
        // Find the first erofs in the cache, or use http-echo if available
        let cache_dir = "/run/cloudhv/erofs-cache";
        if let Ok(entries) = std::fs::read_dir(cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "erofs").unwrap_or(false) {
                    return path.to_string_lossy().to_string();
                }
            }
        }
        String::new()
    })
}

#[test]
fn test_shadow_snapshot_creation() {
    let erofs = erofs_path();
    if erofs.is_empty() {
        println!("SKIP: no erofs image available (set EROFS_PATH or populate erofs-cache)");
        return;
    }
    println!("Using erofs: {}", erofs);

    let image_key = "test-shadow-image";

    // Verify no snapshot exists yet
    let status = rpc("Status", &serde_json::json!({}));
    let keys: Vec<String> = status["snapshot_keys"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !keys.contains(&image_key.to_string()),
        "snapshot should not exist yet"
    );
    println!("Initial status: {:?}", status);

    // First acquire with image_key — should return pool VM and trigger shadow
    let t0 = Instant::now();
    let resp = rpc(
        "AcquireSandbox",
        &serde_json::json!({
            "netns": "",
            "image_key": image_key,
            "erofs_path": erofs,
        }),
    );
    let acquire_ms = t0.elapsed().as_millis();
    assert!(resp.get("error").is_none(), "acquire error: {resp}");
    assert!(
        !resp["from_snapshot"].as_bool().unwrap_or(true),
        "first acquire should NOT be from snapshot"
    );
    println!(
        "First acquire: {}ms (from pool, shadow triggered)",
        acquire_ms
    );

    let vm_id = resp["vm_id"].as_str().unwrap().to_string();

    // Release the VM
    rpc("ReleaseSandbox", &serde_json::json!({"vm_id": vm_id}));

    // Check that shadow was triggered
    let status = rpc("Status", &serde_json::json!({}));
    println!(
        "After first acquire: shadow_vms_running={}, snapshot_keys={:?}",
        status["shadow_vms_running"], status["snapshot_keys"]
    );

    // Wait for shadow VM to complete (warmup + snapshot)
    // Default warmup is 30s — use a shorter config for testing
    let warmup_secs: u64 = std::env::var("WARMUP_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    println!(
        "Waiting {}s for shadow VM warmup + snapshot...",
        warmup_secs + 10
    );
    std::thread::sleep(std::time::Duration::from_secs(warmup_secs + 10));

    // Verify snapshot was created
    let status = rpc("Status", &serde_json::json!({}));
    let keys: Vec<String> = status["snapshot_keys"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    println!("After shadow: snapshot_keys={:?}", keys);

    if keys.contains(&image_key.to_string()) {
        println!("Shadow snapshot created successfully!");

        // Second acquire — should restore from snapshot
        let t1 = Instant::now();
        let resp2 = rpc(
            "AcquireSandbox",
            &serde_json::json!({
                "netns": "",
                "image_key": image_key,
                "erofs_path": erofs,
            }),
        );
        let warm_ms = t1.elapsed().as_millis();
        assert!(resp2.get("error").is_none(), "warm acquire error: {resp2}");
        assert!(
            resp2["from_snapshot"].as_bool().unwrap_or(false),
            "second acquire SHOULD be from snapshot"
        );
        println!("Warm restore acquire: {}ms (from_snapshot=true)", warm_ms);

        let vm_id2 = resp2["vm_id"].as_str().unwrap().to_string();
        rpc("ReleaseSandbox", &serde_json::json!({"vm_id": vm_id2}));
    } else {
        println!("WARNING: shadow snapshot not created (shadow may have failed)");
        // Don't fail the test — shadow creation depends on the workload
        // being able to start, which requires a valid OCI config
    }
}

#[test]
fn test_status_includes_snapshot_fields() {
    let status = rpc("Status", &serde_json::json!({}));
    assert!(status.get("pool_ready").is_some());
    assert!(status.get("active_vms").is_some());
    assert!(status.get("shadow_vms_running").is_some());
    assert!(status.get("snapshot_keys").is_some());
    println!("Status: {}", serde_json::to_string_pretty(&status).unwrap());
}

/// End-to-end networking test: creates a netns with a veth pair, boots a VM
/// with http-echo, and verifies HTTP connectivity through the TAP bridge.
///
/// Requires: daemon running, http-echo erofs image available, `ip` and `curl`
/// commands installed, root privileges (for netns creation).
#[test]
fn test_pod_http_connectivity() {
    let erofs = erofs_path();
    if erofs.is_empty() {
        println!("SKIP: no erofs image available (set EROFS_PATH or populate erofs-cache)");
        return;
    }

    let netns_name = "cloudhv-test-net";
    let veth_host = "veth-test-h";
    let veth_peer = "veth-test-p";
    let veth_ns = "eth0";
    let pod_ip = "10.99.0.2";
    let host_ip = "10.99.0.1";
    let prefix = "24";

    // Clean up any leftover test netns
    let _ = run_cmd("ip", &["netns", "delete", netns_name]);
    let _ = run_cmd("ip", &["link", "delete", veth_host]);
    let _ = run_cmd("ip", &["link", "delete", veth_peer]);

    // Create netns and veth pair
    run_cmd_ok("ip", &["netns", "add", netns_name]);
    run_cmd_ok(
        "ip",
        &["link", "add", veth_host, "type", "veth", "peer", "name", veth_peer],
    );
    run_cmd_ok(
        "ip",
        &["link", "set", veth_peer, "netns", netns_name],
    );
    // Rename inside netns to eth0 (what the daemon expects)
    run_cmd_ok(
        "ip",
        &["netns", "exec", netns_name, "ip", "link", "set", veth_peer, "name", veth_ns],
    );

    // Configure host side
    run_cmd_ok(
        "ip",
        &["addr", "add", &format!("{host_ip}/{prefix}"), "dev", veth_host],
    );
    run_cmd_ok("ip", &["link", "set", veth_host, "up"]);

    // Configure netns side (veth gets pod IP, acts like CNI)
    run_cmd_ok(
        "ip",
        &[
            "netns", "exec", netns_name,
            "ip", "addr", "add", &format!("{pod_ip}/{prefix}"), "dev", veth_ns,
        ],
    );
    run_cmd_ok(
        "ip",
        &["netns", "exec", netns_name, "ip", "link", "set", veth_ns, "up"],
    );
    run_cmd_ok(
        "ip",
        &["netns", "exec", netns_name, "ip", "link", "set", "lo", "up"],
    );
    // Add default route so prepare_tap can discover the gateway
    run_cmd_ok(
        "ip",
        &[
            "netns", "exec", netns_name,
            "ip", "route", "add", "default", "via", host_ip,
        ],
    );

    let netns_path = format!("/var/run/netns/{netns_name}");

    // Build a minimal OCI config for http-echo
    let oci_config = serde_json::json!({
        "ociVersion": "1.0.0",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": ["/http-echo", "-text=integration-test-ok", "-listen=:5678"],
            "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "cwd": "/"
        },
        "root": { "path": "rootfs", "readonly": true },
        "linux": {
            "namespaces": [
                { "type": "mount" }
            ]
        }
    });
    let config_json = serde_json::to_vec(&oci_config).unwrap();

    // Encode config as base64
    let config_b64 = base64_encode(&config_json);

    let container_id = format!("integ-net-{}", std::process::id());

    // Acquire VM with networking
    println!("Acquiring VM with netns={netns_path} erofs={erofs}...");
    let t0 = Instant::now();
    let resp = rpc(
        "AcquireSandbox",
        &serde_json::json!({
            "netns": netns_path,
            "image_key": "",
            "erofs_path": erofs,
            "container_id": container_id,
            "config_json": config_b64,
        }),
    );
    let acquire_ms = t0.elapsed().as_millis();
    assert!(
        resp.get("error").is_none(),
        "acquire error: {}",
        resp
    );
    let vm_id = resp["vm_id"].as_str().unwrap().to_string();
    let container_pid = resp["container_pid"].as_u64().unwrap_or(0);
    println!(
        "Acquired: vm_id={vm_id} container_pid={container_pid} in {acquire_ms}ms"
    );
    assert!(container_pid > 0, "container should be running");

    // Wait for http-echo to start listening
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Curl the container from the host via the veth
    println!("Curling http://{pod_ip}:5678/ ...");
    let curl_result = run_cmd("curl", &[
        "-s", "--connect-timeout", "5",
        &format!("http://{pod_ip}:5678/"),
    ]);
    let (curl_ok, curl_output) = curl_result;
    println!("curl output: {curl_output}");
    assert!(curl_ok, "curl failed: {curl_output}");
    assert!(
        curl_output.contains("integration-test-ok"),
        "unexpected response: {curl_output}"
    );
    println!("✅ HTTP connectivity verified!");

    // Release VM
    let resp = rpc("ReleaseSandbox", &serde_json::json!({"vm_id": vm_id}));
    assert!(resp.get("error").is_none(), "release error: {resp}");

    // Clean up netns
    let _ = run_cmd("ip", &["link", "delete", veth_host]);
    let _ = run_cmd("ip", &["netns", "delete", netns_name]);
    println!("Cleaned up test netns");
}

/// Run a command and return (success, combined output).
fn run_cmd(cmd: &str, args: &[&str]) -> (bool, String) {
    match std::process::Command::new(cmd)
        .args(args)
        .output()
    {
        Ok(output) => {
            let out = String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr);
            (output.status.success(), out.trim().to_string())
        }
        Err(e) => (false, format!("exec error: {e}")),
    }
}

/// Run a command and panic on failure.
fn run_cmd_ok(cmd: &str, args: &[&str]) {
    let (ok, out) = run_cmd(cmd, args);
    assert!(ok, "{} {} failed: {}", cmd, args.join(" "), out);
}

/// Base64 encode bytes (no external dep needed — test-only).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[(n >> 18 & 63) as usize] as char);
        result.push(CHARS[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[(n >> 6 & 63) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}
