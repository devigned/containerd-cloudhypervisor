//! Client for the sandbox daemon.
//!
//! When `daemon_socket` is set in the runtime config, the shim uses this
//! client to acquire pre-booted VMs from the daemon instead of spawning
//! Cloud Hypervisor directly.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};
use log::info;

/// Response from AcquireSandbox.
#[derive(Debug)]
pub struct AcquiredVm {
    pub vm_id: String,
    pub vsock_socket: PathBuf,
    pub cid: u64,
    pub ch_pid: u32,
    pub from_snapshot: bool,
    pub container_pid: u32,
}

/// Client for the sandbox daemon's Unix socket API.
pub struct DaemonClient {
    socket_path: String,
}

impl DaemonClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Check if the daemon is available.
    pub fn is_available(&self) -> bool {
        !self.socket_path.is_empty() && std::path::Path::new(&self.socket_path).exists()
    }

    /// Acquire a pre-booted VM from the daemon.
    /// The daemon handles hot-plugging the rootfs and starting the container.
    pub fn acquire_sandbox(
        &self,
        tap_name: &str,
        tap_mac: &str,
        ip_cidr: &str,
        gateway: &str,
        image_key: &str,
        erofs_path: &str,
        container_id: &str,
        config_json: &[u8],
    ) -> Result<AcquiredVm> {
        use base64::Engine;
        let config_b64 = base64::engine::general_purpose::STANDARD.encode(config_json);

        let req = serde_json::json!({
            "method": "AcquireSandbox",
            "tap_name": tap_name,
            "tap_mac": tap_mac,
            "ip_cidr": ip_cidr,
            "gateway": gateway,
            "image_key": image_key,
            "erofs_path": erofs_path,
            "container_id": container_id,
            "config_json": config_b64,
        });

        let resp = self.rpc(&req)?;

        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("daemon AcquireSandbox: {}", err);
        }

        Ok(AcquiredVm {
            vm_id: resp["vm_id"].as_str().context("missing vm_id")?.to_string(),
            vsock_socket: PathBuf::from(
                resp["vsock_socket"]
                    .as_str()
                    .context("missing vsock_socket")?,
            ),
            cid: resp["cid"].as_u64().context("missing cid")?,
            ch_pid: resp["ch_pid"].as_u64().context("missing ch_pid")? as u32,
            from_snapshot: resp["from_snapshot"].as_bool().unwrap_or(false),
            container_pid: resp["container_pid"].as_u64().unwrap_or(0) as u32,
        })
    }

    /// Release a VM back to the daemon for destruction.
    pub fn release_sandbox(&self, vm_id: &str) -> Result<()> {
        let req = serde_json::json!({
            "method": "ReleaseSandbox",
            "vm_id": vm_id,
        });

        let resp = self.rpc(&req)?;

        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("daemon ReleaseSandbox: {}", err);
        }

        info!("released VM {} to daemon", vm_id);
        Ok(())
    }

    /// Add a container to an existing VM (multi-container pod).
    /// Returns the container PID.
    pub fn add_container(
        &self,
        vm_id: &str,
        erofs_path: &str,
        container_id: &str,
        config_json: &[u8],
    ) -> Result<u32> {
        use base64::Engine;
        let config_b64 = base64::engine::general_purpose::STANDARD.encode(config_json);

        let req = serde_json::json!({
            "method": "AddContainer",
            "vm_id": vm_id,
            "erofs_path": erofs_path,
            "container_id": container_id,
            "config_json": config_b64,
        });

        let resp = self.rpc(&req)?;

        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("daemon AddContainer: {}", err);
        }

        Ok(resp["container_pid"].as_u64().unwrap_or(0) as u32)
    }

    /// Send a JSON-line RPC to the daemon.
    fn rpc(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .with_context(|| format!("connect to daemon: {}", self.socket_path))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;

        let mut msg = serde_json::to_string(request)?;
        msg.push('\n');
        stream.write_all(msg.as_bytes())?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("read daemon response")?;

        serde_json::from_str(line.trim()).context("parse daemon response")
    }
}
