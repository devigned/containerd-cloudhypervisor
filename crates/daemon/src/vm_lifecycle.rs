//! Low-level VM lifecycle operations: boot, snapshot, restore, destroy.
//!
//! These functions interact with Cloud Hypervisor's API socket directly.
//! They are used by the pool manager and shadow VM logic.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::info;
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::config::DaemonConfig;

/// Boot a VM from scratch (cold boot). Returns the CH process PID.
pub async fn boot_vm(
    config: &DaemonConfig,
    vm_id: &str,
    state_dir: &Path,
    cid: u64,
    tap_name: Option<&str>,
    tap_mac: Option<&str>,
    extra_disks: &[(String, String, bool)], // (path, id, readonly)
) -> Result<u32> {
    let ch_pid = spawn_ch(config, state_dir).await?;
    let api_socket = state_dir.join("api.sock");
    wait_ch_ready(&api_socket).await?;

    let vsock_socket = state_dir.join("vsock.sock");
    let console_log = state_dir.join("console.log");
    std::fs::File::create(&console_log)?;

    // Build VM config
    let mut disks = vec![serde_json::json!({
        "path": config.rootfs_path,
        "readonly": true,
        "id": "_disk0",
    })];
    for (path, id, readonly) in extra_disks {
        disks.push(serde_json::json!({
            "path": path,
            "readonly": readonly,
            "id": id,
        }));
    }

    let mut net = Vec::new();
    if let (Some(tap), Some(mac)) = (tap_name, tap_mac) {
        net.push(serde_json::json!({
            "tap": tap,
            "mac": mac,
        }));
    }

    let vm_config = serde_json::json!({
        "payload": {
            "kernel": config.kernel_path,
            "cmdline": config.kernel_args,
        },
        "cpus": {
            "boot_vcpus": config.default_vcpus,
            "max_vcpus": config.default_vcpus,
        },
        "memory": {
            "size": config.default_memory_mb * 1024 * 1024,
            "shared": true,
        },
        "disks": disks,
        "net": net,
        "serial": { "mode": "File", "file": console_log.to_string_lossy() },
        "console": { "mode": "Off" },
        "vsock": { "cid": cid, "socket": vsock_socket.to_string_lossy() },
    });

    api_request(
        &api_socket,
        "PUT",
        "/api/v1/vm.create",
        Some(&vm_config.to_string()),
    )
    .await?;
    api_request(&api_socket, "PUT", "/api/v1/vm.boot", None).await?;

    // Save PID for cleanup
    std::fs::write(state_dir.join("ch.pid"), ch_pid.to_string())?;

    info!("VM {} booted (pid={}, cid={})", vm_id, ch_pid, cid);
    Ok(ch_pid)
}

/// Spawn a Cloud Hypervisor process. Returns PID.
pub async fn spawn_ch(config: &DaemonConfig, state_dir: &Path) -> Result<u32> {
    let api_socket = state_dir.join("api.sock");
    let child = Command::new(&config.cloud_hypervisor_binary)
        .arg("--api-socket")
        .arg(&api_socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn cloud-hypervisor")?;

    let pid = child.id().context("get CH pid")?;
    // Detach — we don't want to wait on it
    std::mem::forget(child);

    std::fs::write(state_dir.join("ch.pid"), pid.to_string())?;
    Ok(pid)
}

/// Wait for the CH API socket to become ready.
pub async fn wait_ch_ready(api_socket: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if api_socket.exists() {
            if UnixStream::connect(api_socket).await.is_ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("CH API socket not ready: {}", api_socket.display())
}

/// Connect to the guest agent via vsock and verify it responds.
pub async fn wait_for_agent(vsock_socket: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if vsock_socket.exists() {
            match try_agent_health(vsock_socket).await {
                Ok(()) => return Ok(()),
                Err(_) => {}
            }
        }
        tokio::task::yield_now().await;
    }
    anyhow::bail!("agent not responding on vsock: {}", vsock_socket.display())
}

/// Try to connect to the agent and check health.
async fn try_agent_health(vsock_socket: &Path) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stream = UnixStream::connect(vsock_socket).await?;
    let (reader, mut writer) = stream.into_split();

    let connect_cmd = format!("CONNECT {}\n", cloudhv_common::AGENT_VSOCK_PORT);
    writer.write_all(connect_cmd.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        buf_reader.read_line(&mut response),
    )
    .await??;

    if response.starts_with("OK") {
        Ok(())
    } else {
        anyhow::bail!("vsock CONNECT failed: {}", response.trim())
    }
}

/// Pause a running VM.
pub async fn pause_vm(api_socket: &Path) -> Result<()> {
    api_request(api_socket, "PUT", "/api/v1/vm.pause", None).await
}

/// Resume a paused VM.
pub async fn resume_vm(api_socket: &Path) -> Result<()> {
    api_request(api_socket, "PUT", "/api/v1/vm.resume", None).await
}

/// Take a snapshot of a paused VM.
pub async fn snapshot_vm(api_socket: &Path, dest_dir: &Path) -> Result<()> {
    let body = serde_json::json!({
        "destination_url": format!("file://{}", dest_dir.display()),
    });
    api_request(
        api_socket,
        "PUT",
        "/api/v1/vm.snapshot",
        Some(&body.to_string()),
    )
    .await
}

/// Restore a VM from a snapshot directory.
pub async fn restore_vm(api_socket: &Path, source_dir: &Path) -> Result<()> {
    let body = serde_json::json!({
        "source_url": format!("file://{}", source_dir.display()),
        "prefault": false,
    });
    api_request(
        api_socket,
        "PUT",
        "/api/v1/vm.restore",
        Some(&body.to_string()),
    )
    .await
}

/// Prepare a snapshot directory for a specific VM: patch CID, vsock socket,
/// serial console path. Hardlinks large files (memory-ranges) for CoW.
pub fn prepare_snapshot(
    base_dir: &Path,
    vm_state_dir: &Path,
    new_cid: u64,
    new_vsock_socket: &Path,
) -> Result<PathBuf> {
    let work_dir = vm_state_dir.join("snapshot");
    std::fs::create_dir_all(&work_dir)?;

    // Hardlink large files
    for name in &["memory-ranges", "state.json"] {
        let src = base_dir.join(name);
        let dst = work_dir.join(name);
        std::fs::hard_link(&src, &dst)
            .or_else(|e| {
                if e.raw_os_error() == Some(18) {
                    std::fs::copy(&src, &dst).map(|_| ())
                } else {
                    Err(e)
                }
            })
            .with_context(|| format!("link {name} from base snapshot"))?;
    }

    // Read, patch, and write config.json
    let config_str = std::fs::read_to_string(base_dir.join("config.json"))?;
    let mut config: serde_json::Value = serde_json::from_str(&config_str)?;

    // Patch vsock CID and socket
    if let Some(vsock) = config.pointer_mut("/vsock") {
        if let Some(obj) = vsock.as_object_mut() {
            obj.insert("cid".to_string(), serde_json::json!(new_cid));
            obj.insert(
                "socket".to_string(),
                serde_json::json!(new_vsock_socket.to_string_lossy()),
            );
        }
    }

    // Patch serial console path
    let console_path = vm_state_dir.join("console.log");
    std::fs::File::create(&console_path)?;
    if let Some(serial) = config.pointer_mut("/serial") {
        if let Some(obj) = serial.as_object_mut() {
            if obj.get("mode").and_then(|v| v.as_str()) == Some("File") {
                obj.insert(
                    "file".to_string(),
                    serde_json::json!(console_path.to_string_lossy()),
                );
            }
        }
    }

    std::fs::write(
        work_dir.join("config.json"),
        serde_json::to_string_pretty(&config)?,
    )?;

    Ok(work_dir)
}

/// Shutdown and destroy a VM.
pub async fn shutdown_and_destroy(api_socket: &Path, ch_pid: u32, state_dir: &Path) {
    // Try graceful shutdown
    let _ = api_request(api_socket, "PUT", "/api/v1/vm.shutdown", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Force kill if still running
    unsafe {
        libc::kill(ch_pid as i32, libc::SIGKILL);
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Clean up state directory
    let _ = std::fs::remove_dir_all(state_dir);
}

/// Send an HTTP request to the CH API socket.
async fn api_request(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to CH API: {}", socket_path.display()))?;

    let body_bytes = body.unwrap_or("");
    let request = if body_bytes.is_empty() {
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
    } else {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body_bytes}",
            body_bytes.len()
        )
    };

    stream.write_all(request.as_bytes()).await?;

    let mut response = vec![0u8; 4096];
    let n = stream.read(&mut response).await?;
    let response_str = String::from_utf8_lossy(&response[..n]);

    if !response_str.contains("200 OK") && !response_str.contains("204 No Content") {
        anyhow::bail!(
            "CH API {method} {path}: {}",
            response_str.lines().next().unwrap_or("empty")
        );
    }

    Ok(())
}
