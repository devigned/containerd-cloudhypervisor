use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};

use cloudhv_common::types::*;
use cloudhv_common::{GUEST_CID_START, RUNTIME_STATE_DIR, VIRTIOFS_TAG};

/// Global CID counter for allocating unique vsock CIDs to each VM.
static NEXT_CID: AtomicU64 = AtomicU64::new(GUEST_CID_START);

fn allocate_cid() -> u64 {
    NEXT_CID.fetch_add(1, Ordering::Relaxed)
}

/// Manages the lifecycle of a single Cloud Hypervisor VM instance.
pub struct VmManager {
    /// Unique identifier for this VM (matches containerd container ID).
    vm_id: String,
    /// Allocated vsock CID for this VM.
    cid: u64,
    /// Runtime directory for this VM: /run/cloudhv/<vm_id>/
    state_dir: PathBuf,
    /// Path to the Cloud Hypervisor API socket.
    api_socket: PathBuf,
    /// Path to the vsock socket (host-side).
    vsock_socket: PathBuf,
    /// Path to the virtiofsd socket.
    virtiofsd_socket: PathBuf,
    /// Shared directory for virtio-fs.
    shared_dir: PathBuf,
    /// Cloud Hypervisor child process.
    ch_process: Option<Child>,
    /// virtiofsd child process.
    virtiofsd_process: Option<Child>,
    /// Runtime configuration.
    config: RuntimeConfig,
}

impl VmManager {
    /// Create a new VM manager. Does not start the VM.
    pub fn new(vm_id: String, config: RuntimeConfig) -> Result<Self> {
        let cid = allocate_cid();
        let state_dir = PathBuf::from(RUNTIME_STATE_DIR).join(&vm_id);
        let api_socket = state_dir.join("api.sock");
        let vsock_socket = state_dir.join("vsock.sock");
        let virtiofsd_socket = state_dir.join("virtiofsd.sock");
        let shared_dir = state_dir.join("shared");

        info!(
            "VmManager created: vm_id={}, cid={}, state_dir={}",
            vm_id,
            cid,
            state_dir.display()
        );

        Ok(Self {
            vm_id,
            cid,
            state_dir,
            api_socket,
            vsock_socket,
            virtiofsd_socket,
            shared_dir,
            ch_process: None,
            virtiofsd_process: None,
            config,
        })
    }

    /// Prepare the state directory and shared filesystem.
    pub async fn prepare(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.shared_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to create shared dir: {}",
                    self.shared_dir.display()
                )
            })?;
        debug!("state directory prepared: {}", self.state_dir.display());
        Ok(())
    }

    /// Start virtiofsd to serve the shared directory.
    pub async fn start_virtiofsd(&mut self) -> Result<()> {
        info!(
            "starting virtiofsd: socket={}, shared_dir={}",
            self.virtiofsd_socket.display(),
            self.shared_dir.display()
        );

        let child = Command::new(&self.config.virtiofsd_binary)
            .arg(format!(
                "--socket-path={}",
                self.virtiofsd_socket.display()
            ))
            .arg(format!("--shared-dir={}", self.shared_dir.display()))
            .arg("--cache=never")
            .arg("--sandbox=none")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn virtiofsd")?;

        self.virtiofsd_process = Some(child);

        // Wait briefly for the socket to appear
        for _ in 0..20 {
            if self.virtiofsd_socket.exists() {
                debug!("virtiofsd socket ready");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        anyhow::bail!(
            "virtiofsd socket did not appear at {}",
            self.virtiofsd_socket.display()
        );
    }

    /// Start the Cloud Hypervisor VMM process.
    pub async fn start_vmm(&mut self) -> Result<()> {
        info!(
            "starting cloud-hypervisor: api_socket={}",
            self.api_socket.display()
        );

        let ch_binary = &self.config.cloud_hypervisor_binary;
        let child = Command::new(ch_binary)
            .arg("--api-socket")
            .arg(&self.api_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn cloud-hypervisor at {ch_binary}"))?;

        self.ch_process = Some(child);

        // Wait for the API socket to appear
        for _ in 0..50 {
            if self.api_socket.exists() {
                debug!("cloud-hypervisor API socket ready");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        anyhow::bail!(
            "cloud-hypervisor API socket did not appear at {}",
            self.api_socket.display()
        );
    }

    /// Create and boot the VM via the Cloud Hypervisor HTTP API.
    pub async fn create_and_boot_vm(&self) -> Result<()> {
        let vm_config = VmConfig {
            payload: VmPayload {
                kernel: self.config.kernel_path.clone(),
                cmdline: Some(self.config.kernel_args.clone()),
                initramfs: None,
            },
            cpus: VmCpus {
                boot_vcpus: self.config.default_vcpus,
                max_vcpus: self.config.default_vcpus,
            },
            memory: VmMemory {
                size: self.config.default_memory_mb * 1024 * 1024,
                shared: true,
                hotplug_size: None,
            },
            disks: vec![VmDisk {
                path: self.config.rootfs_path.clone(),
                readonly: false,
            }],
            fs: vec![VmFs {
                tag: VIRTIOFS_TAG.to_string(),
                socket: self.virtiofsd_socket.to_string_lossy().to_string(),
                num_queues: 1,
                queue_size: 128,
            }],
            vsock: Some(VmVsock {
                cid: self.cid,
                socket: self.vsock_socket.to_string_lossy().to_string(),
            }),
            serial: Some(VmConsoleConfig::off()),
            console: Some(VmConsoleConfig::off()),
        };

        let config_json = serde_json::to_string(&vm_config)?;
        debug!("VM config: {}", config_json);

        // PUT /api/v1/vm.create
        let create_resp = self.api_request("PUT", "/api/v1/vm.create", Some(&config_json))
            .await
            .context("failed to create VM")?;
        info!("VM create response: {}", create_resp);

        // Small delay between create and boot
        tokio::time::sleep(Duration::from_millis(200)).await;

        // PUT /api/v1/vm.boot
        self.api_request("PUT", "/api/v1/vm.boot", None)
            .await
            .context("failed to boot VM")?;

        info!("VM {} created and booted (cid={})", self.vm_id, self.cid);
        Ok(())
    }

    /// Wait for the guest agent to become responsive.
    pub async fn wait_for_agent(&self) -> Result<()> {
        let timeout_duration = Duration::from_secs(self.config.agent_startup_timeout_secs);
        info!(
            "waiting for guest agent on vsock (cid={}, timeout={}s)",
            self.cid, self.config.agent_startup_timeout_secs
        );

        timeout(timeout_duration, async {
            loop {
                // Try connecting to the vsock socket and sending a health check
                match self.check_agent_health().await {
                    Ok(true) => {
                        info!("guest agent is ready");
                        return Ok(());
                    }
                    Ok(false) => {
                        debug!("agent not ready yet, retrying...");
                    }
                    Err(e) => {
                        debug!("agent check failed: {}, retrying...", e);
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out waiting for guest agent after {}s",
                self.config.agent_startup_timeout_secs
            )
        })?
    }

    /// Check if the guest agent is responding to health checks.
    async fn check_agent_health(&self) -> Result<bool> {
        // Connect to the vsock host-side socket
        // Cloud Hypervisor exposes vsock as a Unix socket at vsock_socket path
        // The guest agent listens on AGENT_VSOCK_PORT
        // TODO: implement ttrpc health check over vsock
        // For now, just check if the vsock socket exists and is connectable
        if !self.vsock_socket.exists() {
            return Ok(false);
        }
        match UnixStream::connect(&self.vsock_socket).await {
            Ok(_stream) => {
                // Connection succeeded — agent is likely up
                // Full ttrpc health check will be implemented in vsock.rs
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    /// Send an HTTP request to the Cloud Hypervisor API over Unix socket.
    async fn api_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String> {
        let mut stream = UnixStream::connect(&self.api_socket)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to CH API socket: {}",
                    self.api_socket.display()
                )
            })?;

        let request = match body {
            Some(b) if !b.is_empty() => format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                b.len()
            ),
            _ => format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Length: 0\r\n\r\n"
            ),
        };

        debug!("CH API request: {} {}", method, path);
        stream.write_all(request.as_bytes()).await?;

        // Give CH a moment to process before reading
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Read response with a timeout
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(
                std::cmp::min(remaining, Duration::from_secs(5)),
                stream.read(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => {
                    response.extend_from_slice(&buf[..n]);
                    // Check if we have a complete response (headers + body)
                    if let Some(pos) = find_subsequence(&response, b"\r\n\r\n") {
                        // Check for Content-Length to know if we have full body
                        let header_str = String::from_utf8_lossy(&response[..pos]);
                        if let Some(cl) = parse_content_length(&header_str) {
                            let body_start = pos + 4;
                            if response.len() >= body_start + cl {
                                break; // Full response received
                            }
                        } else {
                            // No Content-Length — for 204 No Content, headers are enough
                            break;
                        }
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break, // Timeout
            }
        }

        let response_str = String::from_utf8_lossy(&response).to_string();
        debug!("CH API response ({} bytes): {}", response.len(), &response_str[..std::cmp::min(response_str.len(), 200)]);

        // Parse HTTP status line
        let status_line = response_str.lines().next().unwrap_or("");
        let status_code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if status_code >= 200 && status_code < 300 {
            debug!("API {method} {path} -> {status_code}");
            let body = response_str
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or("")
                .to_string();
            Ok(body)
        } else {
            error!("API {method} {path} -> {status_code}");
            error!("Response body: {response_str}");
            anyhow::bail!("CH API error: {status_code} for {method} {path}: {response_str}")
        }
    }

    /// Shutdown the VM gracefully.
    pub async fn shutdown(&mut self) -> Result<()> {
        info!("shutting down VM {}", self.vm_id);

        // Try graceful shutdown via API
        if self.api_socket.exists() {
            match self
                .api_request("PUT", "/api/v1/vm.shutdown", None)
                .await
            {
                Ok(_) => {
                    info!("VM {} shutdown requested via API", self.vm_id);
                }
                Err(e) => {
                    warn!("VM {} API shutdown failed: {}, killing process", self.vm_id, e);
                }
            }
        }

        // Kill CH process if still running
        if let Some(ref mut child) = self.ch_process {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        // Kill virtiofsd if still running
        if let Some(ref mut child) = self.virtiofsd_process {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        Ok(())
    }

    /// Clean up all state for this VM.
    pub async fn cleanup(&mut self) -> Result<()> {
        self.shutdown().await?;

        // Remove state directory
        if self.state_dir.exists() {
            tokio::fs::remove_dir_all(&self.state_dir).await.ok();
            debug!("removed state directory: {}", self.state_dir.display());
        }

        info!("VM {} cleaned up", self.vm_id);
        Ok(())
    }

    // --- Accessors ---

    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    pub fn cid(&self) -> u64 {
        self.cid
    }

    pub fn vsock_socket(&self) -> &Path {
        &self.vsock_socket
    }

    pub fn shared_dir(&self) -> &Path {
        &self.shared_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        if let Some(val) = line.strip_prefix("Content-Length:") {
            return val.trim().parse().ok();
        }
        if let Some(val) = line.strip_prefix("content-length:") {
            return val.trim().parse().ok();
        }
    }
    None
}
