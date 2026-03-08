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
    /// Path to the swtpm socket (if TPM enabled).
    tpm_socket: PathBuf,
    /// Cloud Hypervisor child process.
    ch_process: Option<Child>,
    /// virtiofsd child process.
    virtiofsd_process: Option<Child>,
    /// swtpm child process (if TPM enabled).
    swtpm_process: Option<Child>,
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
        let tpm_socket = state_dir.join("tpm.sock");

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
            tpm_socket,
            ch_process: None,
            virtiofsd_process: None,
            swtpm_process: None,
            config,
        })
    }

    /// Prepare the state directory and shared filesystem.
    pub async fn prepare(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.shared_dir)
            .await
            .with_context(|| {
                format!("failed to create shared dir: {}", self.shared_dir.display())
            })?;
        debug!("state directory prepared: {}", self.state_dir.display());
        Ok(())
    }

    /// Start swtpm for TPM 2.0 support (if enabled in config).
    pub async fn start_swtpm(&mut self) -> Result<()> {
        if !self.config.tpm_enabled {
            return Ok(());
        }

        info!("starting swtpm: socket={}", self.tpm_socket.display());

        let tpm_state_dir = self.state_dir.join("tpm-state");
        tokio::fs::create_dir_all(&tpm_state_dir).await?;

        let child = Command::new("swtpm")
            .arg("socket")
            .arg("--tpmstate")
            .arg(format!("dir={}", tpm_state_dir.display()))
            .arg("--ctrl")
            .arg(format!("type=unixio,path={}", self.tpm_socket.display()))
            .arg("--tpm2")
            .arg("--log")
            .arg("level=1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn swtpm")?;

        self.swtpm_process = Some(child);

        // Wait for socket to appear
        for _ in 0..20 {
            if self.tpm_socket.exists() {
                debug!("swtpm socket ready");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        anyhow::bail!(
            "swtpm socket did not appear at {}",
            self.tpm_socket.display()
        );
    }

    /// Start virtiofsd to serve the shared directory.
    pub async fn start_virtiofsd(&mut self) -> Result<()> {
        info!(
            "starting virtiofsd: socket={}, shared_dir={}",
            self.virtiofsd_socket.display(),
            self.shared_dir.display()
        );

        let child = Command::new(&self.config.virtiofsd_binary)
            .arg(format!("--socket-path={}", self.virtiofsd_socket.display()))
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
                max_vcpus: std::cmp::max(self.config.default_vcpus, 4),
            },
            memory: VmMemory {
                size: self.config.default_memory_mb * 1024 * 1024,
                shared: true,
                hotplug_size: if self.config.hotplug_memory_mb > 0 {
                    Some(self.config.hotplug_memory_mb * 1024 * 1024)
                } else {
                    None
                },
                hotplug_method: if self.config.hotplug_method == "virtio-mem" {
                    Some("VirtioMem".to_string())
                } else {
                    None
                },
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
            tpm: if self.config.tpm_enabled {
                Some(VmTpm {
                    socket: self.tpm_socket.to_string_lossy().to_string(),
                })
            } else {
                None
            },
        };

        let config_json = serde_json::to_string(&vm_config)?;
        debug!("VM config: {}", config_json);

        // PUT /api/v1/vm.create
        let create_resp = self
            .api_request("PUT", "/api/v1/vm.create", Some(&config_json))
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
    async fn api_request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
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
        debug!(
            "CH API response ({} bytes): {}",
            response.len(),
            &response_str[..std::cmp::min(response_str.len(), 200)]
        );

        // Parse HTTP status line
        let status_line = response_str.lines().next().unwrap_or("");
        let status_code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if (200..300).contains(&status_code) {
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

    /// Resize VM resources (vCPUs and/or memory) via the CH API.
    ///
    /// Uses PUT /api/v1/vm.resize to dynamically adjust resources.
    /// Only works if the VM was created with hotplug_size > 0.
    #[allow(dead_code)]
    pub async fn resize(
        &self,
        desired_vcpus: Option<u32>,
        desired_memory_bytes: Option<u64>,
    ) -> Result<()> {
        let mut resize_body = serde_json::Map::new();
        if let Some(vcpus) = desired_vcpus {
            resize_body.insert(
                "desired_vcpus".to_string(),
                serde_json::Value::Number(vcpus.into()),
            );
        }
        if let Some(mem) = desired_memory_bytes {
            resize_body.insert(
                "desired_ram".to_string(),
                serde_json::Value::Number(mem.into()),
            );
        }

        if resize_body.is_empty() {
            return Ok(());
        }

        let body = serde_json::to_string(&serde_json::Value::Object(resize_body))?;
        info!("resizing VM {}: {}", self.vm_id, body);

        self.api_request("PUT", "/api/v1/vm.resize", Some(&body))
            .await
            .context("failed to resize VM")?;

        info!("VM {} resized successfully", self.vm_id);
        Ok(())
    }

    /// Snapshot the VM state to a directory.
    ///
    /// The VM must be paused first. Creates config.json, memory-ranges,
    /// and state.json in the destination directory.
    #[allow(dead_code)]
    pub async fn snapshot(&self, destination_dir: &Path) -> Result<()> {
        info!(
            "snapshotting VM {} to {}",
            self.vm_id,
            destination_dir.display()
        );

        // Pause the VM first
        self.api_request("PUT", "/api/v1/vm.pause", None)
            .await
            .context("failed to pause VM for snapshot")?;

        // Take snapshot
        let url = format!("file://{}", destination_dir.display());
        let body = serde_json::to_string(&serde_json::json!({
            "destination_url": url
        }))?;
        self.api_request("PUT", "/api/v1/vm.snapshot", Some(&body))
            .await
            .context("failed to snapshot VM")?;

        info!(
            "VM {} snapshot saved to {}",
            self.vm_id,
            destination_dir.display()
        );
        Ok(())
    }

    /// Restore a VM from a snapshot directory.
    ///
    /// Creates a new VM from the saved state. The VM starts in a paused
    /// state and must be resumed with resume().
    #[allow(dead_code)]
    pub async fn restore(api_socket: &Path, source_dir: &Path) -> Result<()> {
        info!("restoring VM from {}", source_dir.display());

        let url = format!("file://{}", source_dir.display());
        let body = serde_json::to_string(&serde_json::json!({
            "source_url": url
        }))?;

        // Connect to the (new) CH instance API socket and send restore
        let mut stream = UnixStream::connect(api_socket)
            .await
            .context("failed to connect to CH API for restore")?;

        let request = format!(
            "PUT /api/v1/vm.restore HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await?;

        let mut response = vec![0u8; 4096];
        let n = stream.read(&mut response).await?;
        let resp_str = String::from_utf8_lossy(&response[..n]);

        if resp_str.contains("200") || resp_str.contains("204") {
            info!("VM restored from {}", source_dir.display());
            Ok(())
        } else {
            anyhow::bail!("restore failed: {resp_str}")
        }
    }

    /// Resume a paused VM (used after snapshot or restore).
    #[allow(dead_code)]
    pub async fn resume(&self) -> Result<()> {
        self.api_request("PUT", "/api/v1/vm.resume", None)
            .await
            .context("failed to resume VM")?;
        info!("VM {} resumed", self.vm_id);
        Ok(())
    }

    /// Send this VM to another Cloud Hypervisor instance via live migration.
    ///
    /// The destination CH must be running and have called receive_migration.
    /// Transport: "unix:/path/to/socket" for same-host, "tcp:host:port" for remote.
    #[allow(dead_code)]
    pub async fn send_migration(&self, transport_uri: &str, local: bool) -> Result<()> {
        info!("sending VM {} migration to {}", self.vm_id, transport_uri);

        let mut body_map = serde_json::Map::new();
        body_map.insert(
            "destination_url".to_string(),
            serde_json::Value::String(transport_uri.to_string()),
        );
        if local {
            body_map.insert("local".to_string(), serde_json::Value::Bool(true));
        }
        let body = serde_json::to_string(&serde_json::Value::Object(body_map))?;

        self.api_request("PUT", "/api/v1/vm.send-migration", Some(&body))
            .await
            .context("failed to send migration")?;

        info!("VM {} migration sent to {}", self.vm_id, transport_uri);
        Ok(())
    }

    /// Prepare to receive a VM via live migration.
    #[allow(dead_code)]
    pub async fn receive_migration(api_socket: &Path, transport_uri: &str) -> Result<()> {
        info!("preparing to receive migration on {}", transport_uri);

        let body = serde_json::to_string(&serde_json::json!({
            "receiver_url": transport_uri
        }))?;

        let mut stream = UnixStream::connect(api_socket)
            .await
            .context("failed to connect to CH API for receive-migration")?;

        let request = format!(
            "PUT /api/v1/vm.receive-migration HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await?;

        let mut response = vec![0u8; 4096];
        let n = stream.read(&mut response).await?;
        let resp_str = String::from_utf8_lossy(&response[..n]);

        if resp_str.contains("200") || resp_str.contains("204") {
            info!("VM migration received on {}", transport_uri);
            Ok(())
        } else {
            anyhow::bail!("receive-migration failed: {resp_str}")
        }
    }

    /// Shutdown the VM gracefully.
    pub async fn shutdown(&mut self) -> Result<()> {
        info!("shutting down VM {}", self.vm_id);

        // Try graceful shutdown via API
        if self.api_socket.exists() {
            match self.api_request("PUT", "/api/v1/vm.shutdown", None).await {
                Ok(_) => {
                    info!("VM {} shutdown requested via API", self.vm_id);
                }
                Err(e) => {
                    warn!(
                        "VM {} API shutdown failed: {}, killing process",
                        self.vm_id, e
                    );
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

        // Clean up swtpm if running
        if let Some(ref mut child) = self.swtpm_process {
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

    #[allow(dead_code)]
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
