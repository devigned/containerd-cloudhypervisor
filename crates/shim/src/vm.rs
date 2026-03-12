use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::Duration;

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
    /// virtiofsd child process (used when embedded-virtiofsd feature is disabled).
    virtiofsd_process: Option<Child>,
    /// In-process virtiofsd backend (used when embedded-virtiofsd feature is enabled).
    #[cfg(all(target_os = "linux", feature = "embedded-virtiofsd"))]
    virtiofsd_backend: Option<crate::virtfs::VirtiofsBackend>,
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
            #[cfg(all(target_os = "linux", feature = "embedded-virtiofsd"))]
            virtiofsd_backend: None,
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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
    ///
    /// When the `embedded-virtiofsd` feature is enabled, runs virtiofsd
    /// in-process as a thread (no child process, ~5MB RSS saved per VM).
    /// Otherwise, spawns the virtiofsd binary as a child process.
    ///
    /// Returns immediately — use `wait_virtiofsd_ready()` to wait for the socket.
    pub fn spawn_virtiofsd(&mut self) -> Result<()> {
        #[cfg(all(target_os = "linux", feature = "embedded-virtiofsd"))]
        {
            let backend =
                crate::virtfs::VirtiofsBackend::start(&self.virtiofsd_socket, &self.shared_dir)
                    .context("failed to start embedded virtiofsd")?;
            self.virtiofsd_backend = Some(backend);
            Ok(())
        }

        #[cfg(not(all(target_os = "linux", feature = "embedded-virtiofsd")))]
        {
            let child = Command::new(&self.config.virtiofsd_binary)
                .arg(format!("--socket-path={}", self.virtiofsd_socket.display()))
                .arg(format!("--shared-dir={}", self.shared_dir.display()))
                .arg("--cache=never")
                .arg("--sandbox=none")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to spawn virtiofsd")?;
            self.virtiofsd_process = Some(child);
            Ok(())
        }
    }

    /// Wait for virtiofsd socket to appear.
    pub async fn wait_virtiofsd_ready(&self) -> Result<()> {
        for _ in 0..200 {
            if self.virtiofsd_socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        anyhow::bail!("virtiofsd socket did not appear");
    }

    /// Start the Cloud Hypervisor VMM process.
    /// If `netns` is provided, CH runs inside that network namespace
    /// so it can access TAP devices created there.
    pub fn spawn_vmm_in_netns(&mut self, netns: Option<&str>) -> Result<()> {
        let ch_binary = &self.config.cloud_hypervisor_binary;
        let child = if let Some(ns) = netns {
            let netns_arg = format!("--net={ns}");
            Command::new("nsenter")
                .arg(netns_arg)
                .arg("--")
                .arg(ch_binary)
                .arg("--api-socket")
                .arg(&self.api_socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("nsenter + cloud-hypervisor in {ns}"))?
        } else {
            Command::new(ch_binary)
                .arg("--api-socket")
                .arg(&self.api_socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("failed to spawn cloud-hypervisor at {ch_binary}"))?
        };
        self.ch_process = Some(child);
        Ok(())
    }

    /// Wait for CH API socket to appear.
    pub async fn wait_vmm_ready(&self) -> Result<()> {
        for _ in 0..500 {
            if self.api_socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        anyhow::bail!("cloud-hypervisor API socket did not appear");
    }

    /// Create and boot the VM via the Cloud Hypervisor HTTP API.
    ///
    /// If `tap_device` is provided, a virtio-net device is attached to the
    /// VM using the named TAP device. The kernel cmdline should include
    /// `ip=...` parameters for network configuration.
    pub async fn create_and_boot_vm(
        &self,
        tap_device: Option<&str>,
        tap_mac: Option<&str>,
    ) -> Result<()> {
        let net = match tap_device {
            Some(tap) => vec![VmNet {
                tap: tap.to_string(),
                mac: tap_mac.map(|m| m.to_string()),
            }],
            None => vec![],
        };

        let vm_config = VmConfig {
            payload: VmPayload {
                kernel: self.config.kernel_path.clone(),
                cmdline: Some(self.config.kernel_args.clone()),
                initramfs: None,
            },
            cpus: VmCpus {
                boot_vcpus: self.config.default_vcpus,
                // Allow hotplug up to host CPU count (or at least boot_vcpus)
                max_vcpus: std::cmp::max(
                    self.config.default_vcpus,
                    std::thread::available_parallelism()
                        .map(|n| n.get() as u32)
                        .unwrap_or(self.config.default_vcpus),
                ),
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
                id: None,
            }],
            net,
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
            serial: Some(VmConsoleConfig::file(
                &self.state_dir.join("console.log").to_string_lossy(),
            )),
            console: Some(VmConsoleConfig::off()),
            balloon: if self.config.hotplug_memory_mb > 0
                && self.config.hotplug_method != "virtio-mem"
            {
                Some(VmBalloon {
                    size: 0,
                    free_page_reporting: true,
                })
            } else {
                None
            },
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
        self.api_request("PUT", "/api/v1/vm.create", Some(&config_json))
            .await
            .context("failed to create VM")?;

        // PUT /api/v1/vm.boot — no delay needed, CH is ready immediately
        self.api_request("PUT", "/api/v1/vm.boot", None)
            .await
            .context("failed to boot VM")?;

        info!("VM {} created and booted (cid={})", self.vm_id, self.cid);
        Ok(())
    }

    /// Wait for the guest agent to become responsive.
    pub async fn wait_for_agent(&self) -> Result<()> {
        info!(
            "waiting for guest agent on vsock (cid={}, timeout={}s)",
            self.cid, self.config.agent_startup_timeout_secs
        );

        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.config.agent_startup_timeout_secs);

        // Poll aggressively — the guest kernel boots in ~200ms and the
        // agent starts immediately after. Each probe uses a blocking
        // CONNECT handshake that returns instantly when the agent is
        // listening, or returns 0 bytes / error when it's not.
        while tokio::time::Instant::now() < deadline {
            if self.check_agent_health().await.unwrap_or(false) {
                info!("guest agent is ready (cid={})", self.cid);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        anyhow::bail!(
            "timed out waiting for guest agent after {}s",
            self.config.agent_startup_timeout_secs
        )
    }

    /// Check if the guest agent is responding on vsock.
    ///
    /// Sends a CONNECT handshake to CH's vsock socket and checks
    /// for an "OK" response from the guest agent.
    async fn check_agent_health(&self) -> Result<bool> {
        if !self.vsock_socket.exists() {
            return Ok(false);
        }

        let stream = match UnixStream::connect(&self.vsock_socket).await {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };

        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut reader, mut writer) = stream.into_split();
        let cmd = format!("CONNECT {}\n", cloudhv_common::AGENT_VSOCK_PORT);
        if writer.write_all(cmd.as_bytes()).await.is_err() {
            return Ok(false);
        }

        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_secs(2), reader.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                let response = String::from_utf8_lossy(&buf[..n]);
                Ok(response.starts_with("OK"))
            }
            _ => Ok(false),
        }
    }

    /// Send an HTTP request to a Cloud Hypervisor API socket.
    /// Static version for use when the VmManager is behind a Mutex.
    pub async fn api_request_to_socket(
        api_socket: &Path,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String> {
        let mut stream = UnixStream::connect(api_socket)
            .await
            .with_context(|| format!("connect to CH API: {}", api_socket.display()))?;

        let request = match body {
            Some(b) if !b.is_empty() => format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                b.len()
            ),
            _ => format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\r\n"
            ),
        };

        stream.write_all(request.as_bytes()).await?;

        let mut response = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let mut buf = [0u8; 4096];
            let read_result = tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                stream.read(&mut buf),
            )
            .await;
            match read_result {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    response.extend_from_slice(&buf[..n]);
                    if find_subsequence(&response, b"\r\n\r\n").is_some() {
                        let headers = String::from_utf8_lossy(&response);
                        if let Some(cl) = parse_content_length(&headers) {
                            let header_end = find_subsequence(&response, b"\r\n\r\n").unwrap() + 4;
                            if response.len() >= header_end + cl {
                                break;
                            }
                        } else if !headers.contains("Content-Length") {
                            break;
                        }
                    }
                }
                Ok(Err(e)) => anyhow::bail!("read error: {e}"),
                Err(_) => anyhow::bail!("API request timed out"),
            }
        }

        let resp_str = String::from_utf8_lossy(&response);
        if let Some(status_line) = resp_str.lines().next() {
            if !status_line.contains("200")
                && !status_line.contains("204")
                && !status_line.contains("201")
            {
                anyhow::bail!("API error: {}", resp_str.trim());
            }
        }

        if let Some(body_start) = find_subsequence(&response, b"\r\n\r\n") {
            Ok(String::from_utf8_lossy(&response[body_start + 4..]).to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Send an HTTP request to the Cloud Hypervisor API over Unix socket.
    async fn api_request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        Self::api_request_to_socket(&self.api_socket, method, path, body).await
    }

    /// Shutdown the VM gracefully.
    pub async fn shutdown(&mut self) -> Result<()> {
        info!("shutting down VM {}", self.vm_id);

        // Try graceful shutdown via API (short timeout — if CH doesn't respond
        // quickly, we'll SIGKILL it below)
        if self.api_socket.exists() {
            match tokio::time::timeout(
                Duration::from_secs(2),
                self.api_request("PUT", "/api/v1/vm.shutdown", None),
            )
            .await
            {
                Ok(Ok(_)) => {
                    info!("VM {} shutdown requested via API", self.vm_id);
                }
                Ok(Err(e)) => {
                    warn!("VM {} API shutdown failed: {e}", self.vm_id);
                }
                Err(_) => {
                    warn!("VM {} API shutdown timed out (2s)", self.vm_id);
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

        // Stop embedded virtiofsd backend (thread exits when CH disconnects,
        // which we ensured above by killing the CH process).
        #[cfg(all(target_os = "linux", feature = "embedded-virtiofsd"))]
        {
            self.virtiofsd_backend.take();
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

    pub fn vsock_socket(&self) -> &Path {
        &self.vsock_socket
    }

    pub fn shared_dir(&self) -> &Path {
        &self.shared_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn api_socket_path(&self) -> &Path {
        &self.api_socket
    }

    pub fn cid(&self) -> u64 {
        self.cid
    }

    /// Append extra parameters to the kernel command line.
    pub fn append_kernel_args(&mut self, args: &str) {
        self.config.kernel_args.push_str(args);
    }

    /// Get the Cloud Hypervisor process PID.
    pub fn ch_pid(&self) -> Option<u32> {
        self.ch_process.as_ref().and_then(|c| c.id())
    }

    /// Hot-plug a block device into the VM.
    pub async fn add_disk(&self, path: &str, disk_id: &str, readonly: bool) -> Result<()> {
        let disk = VmDisk {
            path: path.to_string(),
            readonly,
            id: Some(disk_id.to_string()),
        };
        let body = serde_json::to_string(&disk)?;
        info!(
            "hot-plugging disk to VM {}: id={} path={}",
            self.vm_id, disk_id, path
        );
        self.api_request("PUT", "/api/v1/vm.add-disk", Some(&body))
            .await
            .context("failed to hot-plug disk")?;
        info!("disk {} hot-plugged to VM {}", disk_id, self.vm_id);
        Ok(())
    }

    /// Resize the VM's vCPUs and/or memory.
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
}

/// Synchronous cleanup on drop — kills child processes and removes state.
/// This ensures VM resources are released even if the shim panics or
/// the async cleanup path is never reached.
impl Drop for VmManager {
    fn drop(&mut self) {
        // Kill child processes aggressively — SIGKILL to prevent orphans
        for (name, proc) in [
            ("cloud-hypervisor", &mut self.ch_process),
            ("virtiofsd", &mut self.virtiofsd_process),
            ("swtpm", &mut self.swtpm_process),
        ] {
            if let Some(child) = proc.take() {
                if let Some(pid) = child.id() {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                    // Non-blocking reap — don't hang if already reaped by tokio
                    unsafe {
                        libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
                    }
                    info!("killed {} (pid={})", name, pid);
                }
            }
        }

        // Remove state directory
        if self.state_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
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
