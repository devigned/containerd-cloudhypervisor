//! Async client for the sandbox daemon.
//!
//! The shim communicates with the daemon over a Unix socket using a
//! JSON-line protocol. All methods are async and use tokio::net::UnixStream.

use anyhow::{Context, Result};
use log::info;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Parameters for acquiring a sandbox VM from the daemon.
pub struct AcquireRequest<'a> {
    pub tap_name: &'a str,
    pub tap_mac: &'a str,
    pub ip_cidr: &'a str,
    pub gateway: &'a str,
    pub image_key: &'a str,
    pub erofs_path: &'a str,
    pub container_id: &'a str,
    pub config_json: &'a [u8],
}

/// Response from AcquireSandbox.
#[derive(Debug)]
pub struct AcquiredVm {
    pub vm_id: String,
    pub ch_pid: u32,
    pub from_snapshot: bool,
    pub container_pid: u32,
}

/// Async client for the sandbox daemon's Unix socket API.
pub struct DaemonClient {
    socket_path: String,
}

impl DaemonClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Acquire a pre-booted VM from the daemon.
    pub async fn acquire_sandbox(&self, params: &AcquireRequest<'_>) -> Result<AcquiredVm> {
        use base64::Engine;
        let config_b64 = base64::engine::general_purpose::STANDARD.encode(params.config_json);

        let resp = self
            .rpc(&serde_json::json!({
                "method": "AcquireSandbox",
                "tap_name": params.tap_name,
                "tap_mac": params.tap_mac,
                "ip_cidr": params.ip_cidr,
                "gateway": params.gateway,
                "image_key": params.image_key,
                "erofs_path": params.erofs_path,
                "container_id": params.container_id,
                "config_json": config_b64,
            }))
            .await?;

        Self::check_error(&resp, "AcquireSandbox")?;

        Ok(AcquiredVm {
            vm_id: resp["vm_id"].as_str().context("missing vm_id")?.to_string(),
            ch_pid: resp["ch_pid"].as_u64().context("missing ch_pid")? as u32,
            from_snapshot: resp["from_snapshot"].as_bool().unwrap_or(false),
            container_pid: resp["container_pid"].as_u64().unwrap_or(0) as u32,
        })
    }

    /// Release a VM back to the daemon for destruction.
    pub async fn release_sandbox(&self, vm_id: &str) -> Result<()> {
        let resp = self
            .rpc(&serde_json::json!({
                "method": "ReleaseSandbox",
                "vm_id": vm_id,
            }))
            .await?;
        Self::check_error(&resp, "ReleaseSandbox")?;
        info!("released VM {} to daemon", vm_id);
        Ok(())
    }

    /// Add a container to an existing VM (multi-container pod).
    pub async fn add_container(
        &self,
        vm_id: &str,
        erofs_path: &str,
        container_id: &str,
        config_json: &[u8],
    ) -> Result<u32> {
        use base64::Engine;
        let config_b64 = base64::engine::general_purpose::STANDARD.encode(config_json);

        let resp = self
            .rpc(&serde_json::json!({
                "method": "AddContainer",
                "vm_id": vm_id,
                "erofs_path": erofs_path,
                "container_id": container_id,
                "config_json": config_b64,
            }))
            .await?;
        Self::check_error(&resp, "AddContainer")?;
        Ok(resp["container_pid"].as_u64().unwrap_or(0) as u32)
    }

    /// Send a signal to a container inside a VM.
    pub async fn kill_container(&self, vm_id: &str, container_id: &str, signal: u32) -> Result<()> {
        let resp = self
            .rpc(&serde_json::json!({
                "method": "KillContainer",
                "vm_id": vm_id,
                "container_id": container_id,
                "signal": signal,
            }))
            .await?;
        Self::check_error(&resp, "KillContainer")?;
        Ok(())
    }

    /// Wait for a container to exit. Blocks until the container terminates.
    /// Returns (exit_code, exited_at).
    pub async fn wait_container(&self, vm_id: &str, container_id: &str) -> Result<(u32, String)> {
        // WaitContainer is long-lived — no timeout on the read since the
        // container may run for hours. The daemon holds the connection open
        // and responds only when the container exits.
        let resp = self
            .rpc_no_timeout(&serde_json::json!({
                "method": "WaitContainer",
                "vm_id": vm_id,
                "container_id": container_id,
            }))
            .await?;
        Self::check_error(&resp, "WaitContainer")?;
        let exit_code = resp["exit_status"].as_u64().unwrap_or(137) as u32;
        let exited_at = resp["exited_at"].as_str().unwrap_or("").to_string();
        Ok((exit_code, exited_at))
    }

    /// Delete a stopped container inside a VM.
    pub async fn delete_container(&self, vm_id: &str, container_id: &str) -> Result<()> {
        let resp = self
            .rpc(&serde_json::json!({
                "method": "DeleteContainer",
                "vm_id": vm_id,
                "container_id": container_id,
            }))
            .await?;
        Self::check_error(&resp, "DeleteContainer")?;
        Ok(())
    }

    /// Send a JSON-line RPC with a 30s read timeout.
    async fn rpc(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connect to daemon: {}", self.socket_path))?;

        let mut msg = serde_json::to_string(request)?;
        msg.push('\n');

        let (reader, mut writer) = stream.into_split();
        writer.write_all(msg.as_bytes()).await?;

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            buf_reader.read_line(&mut line),
        )
        .await
        .map_err(|_| anyhow::anyhow!("daemon RPC timed out after 30s"))?
        .context("read daemon response")?;

        serde_json::from_str(line.trim()).context("parse daemon response")
    }

    /// Send a JSON-line RPC without a read timeout (for long-lived RPCs).
    async fn rpc_no_timeout(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connect to daemon: {}", self.socket_path))?;

        let mut msg = serde_json::to_string(request)?;
        msg.push('\n');

        let (reader, mut writer) = stream.into_split();
        writer.write_all(msg.as_bytes()).await?;

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader
            .read_line(&mut line)
            .await
            .context("read daemon response")?;

        serde_json::from_str(line.trim()).context("parse daemon response")
    }

    fn check_error(resp: &serde_json::Value, method: &str) -> Result<()> {
        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("daemon {method}: {err}");
        }
        Ok(())
    }
}
