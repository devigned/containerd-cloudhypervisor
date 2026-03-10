//! Golden snapshot manager for fast VM restore.
//!
//! Instead of cold-booting a VM (~460ms), a golden snapshot captures a
//! fully-booted VM state (kernel up, agent running) and allows restoring
//! copies in ~50ms. The snapshot excludes virtiofs to avoid vhost-user
//! protocol reconnection issues — container operations that need the
//! shared directory fall back to full VM boot.
//!
//! ## Lifecycle
//!
//! 1. **Create** (lazy, on first use): boot a minimal VM (disk + vsock),
//!    wait for agent health, pause, snapshot to disk, shut down.
//! 2. **Restore**: start fresh CH process, call `vm.restore`, resume.
//!    The agent is immediately available — no kernel boot or init wait.
//! 3. The snapshot is reused across all pool VMs and pod creations.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use log::{debug, info};
use tokio::process::Command;
use tokio::time::Duration;

use cloudhv_common::types::RuntimeConfig;
use cloudhv_common::RUNTIME_STATE_DIR;

use crate::vm::VmManager;
use crate::vsock::VsockClient;

/// Default directory for the golden snapshot files.
const SNAPSHOT_SUBDIR: &str = "golden-snapshot";

/// Manages creation and restoration of golden VM snapshots.
pub struct SnapshotManager {
    config: RuntimeConfig,
    /// Directory where the golden snapshot is stored.
    snapshot_dir: PathBuf,
    /// Whether a golden snapshot is available.
    ready: bool,
}

impl SnapshotManager {
    /// Create a new snapshot manager. Does not create the snapshot yet.
    pub fn new(config: RuntimeConfig) -> Self {
        let snapshot_dir = PathBuf::from(RUNTIME_STATE_DIR).join(SNAPSHOT_SUBDIR);
        let ready = snapshot_dir.join("state.json").exists();
        if ready {
            info!("golden snapshot found at {}", snapshot_dir.display());
        }
        Self {
            config,
            snapshot_dir,
            ready,
        }
    }

    /// Whether a golden snapshot is available for restore.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Ensure a golden snapshot exists, creating one if needed.
    ///
    /// This boots a minimal VM (no virtiofs, no network), waits for the
    /// agent to be healthy, then pauses and snapshots. The golden VM is
    /// shut down afterwards — only the snapshot files remain.
    pub async fn ensure_golden_snapshot(&mut self) -> Result<()> {
        if self.ready {
            debug!("golden snapshot already exists");
            return Ok(());
        }

        info!("creating golden snapshot...");
        let start = std::time::Instant::now();

        // Clean any partial snapshot from a previous failed attempt
        if self.snapshot_dir.exists() {
            tokio::fs::remove_dir_all(&self.snapshot_dir).await.ok();
        }
        tokio::fs::create_dir_all(&self.snapshot_dir)
            .await
            .context("create snapshot dir")?;

        // Boot a golden VM — minimal config, no virtiofs, no network
        let golden_id = "golden-snapshot-vm".to_string();
        let mut vm =
            VmManager::new(golden_id, self.config.clone()).context("create golden VmManager")?;

        vm.prepare().await.context("prepare golden VM")?;
        vm.spawn_vmm().context("spawn golden VMM")?;
        vm.wait_vmm_ready().await.context("golden VMM not ready")?;
        vm.create_and_boot_vm_for_snapshot()
            .await
            .context("boot golden VM")?;
        vm.wait_for_agent()
            .await
            .context("golden agent not ready")?;

        // Verify agent health before snapshotting
        let vsock_client = VsockClient::new(vm.vsock_socket());
        let (_agent, health) = vsock_client
            .connect_ttrpc()
            .await
            .context("golden ttrpc connect")?;
        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let resp = health
            .check(ctx, &cloudhv_proto::CheckRequest::new())
            .await
            .context("golden health check")?;
        if !resp.ready {
            anyhow::bail!("golden VM agent not ready");
        }
        drop(_agent);
        drop(health);

        // Snapshot the golden VM
        vm.snapshot(&self.snapshot_dir)
            .await
            .context("snapshot golden VM")?;

        // Shut down the golden VM — we only need the snapshot files
        vm.cleanup().await.context("cleanup golden VM")?;

        self.ready = true;
        info!(
            "golden snapshot created in {:?} at {}",
            start.elapsed(),
            self.snapshot_dir.display()
        );
        Ok(())
    }

    /// Restore a VM from the golden snapshot.
    ///
    /// Creates an instance-specific copy of the snapshot with rewritten
    /// socket paths (config.json), symlinking the large memory file to
    /// avoid copying 128MB+ per restore.
    pub async fn restore_vm(&self, vm_id: &str) -> Result<RestoredVm> {
        if !self.ready {
            anyhow::bail!("no golden snapshot available");
        }

        let start = std::time::Instant::now();
        let state_dir = PathBuf::from(RUNTIME_STATE_DIR).join(vm_id);
        let api_socket = state_dir.join("api.sock");
        let vsock_socket = state_dir.join("vsock.sock");
        let instance_snap_dir = state_dir.join("snapshot");

        tokio::fs::create_dir_all(&instance_snap_dir)
            .await
            .context("create instance snapshot dir")?;

        // Rewrite config.json with instance-specific socket paths.
        // The golden snapshot has the golden VM's paths; we replace them
        // so the restored CH creates sockets in our state_dir.
        let golden_config_path = self.snapshot_dir.join("config.json");
        let config_str = tokio::fs::read_to_string(&golden_config_path)
            .await
            .context("read golden config.json")?;

        let mut config_val: serde_json::Value =
            serde_json::from_str(&config_str).context("parse golden config.json")?;

        // Rewrite vsock socket path
        if let Some(vsock) = config_val.pointer_mut("/vsock/socket") {
            *vsock = serde_json::Value::String(vsock_socket.to_string_lossy().to_string());
        }

        tokio::fs::write(
            instance_snap_dir.join("config.json"),
            serde_json::to_string_pretty(&config_val)?,
        )
        .await
        .context("write instance config.json")?;

        // Symlink state.json and memory-ranges to avoid copying
        for file in ["state.json", "memory-ranges"] {
            let src = self.snapshot_dir.join(file);
            let dst = instance_snap_dir.join(file);
            if src.exists() {
                tokio::fs::symlink(&src, &dst)
                    .await
                    .with_context(|| format!("symlink {file}"))?;
            }
        }

        // Start a fresh CH process with its own API socket.
        // Wrapped in a guard that kills the process on error to prevent leaks.
        let ch_binary = &self.config.cloud_hypervisor_binary;
        let mut ch_process = Command::new(ch_binary)
            .arg("--api-socket")
            .arg(&api_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn CH for restore: {ch_binary}"))?;

        // Helper closure to kill CH on error
        let kill_ch = |proc: &mut tokio::process::Child| {
            let _ = proc.start_kill();
        };

        // Wait for API socket
        for _ in 0..500 {
            if api_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if !api_socket.exists() {
            kill_ch(&mut ch_process);
            anyhow::bail!("CH API socket did not appear for restore");
        }

        // Restore from the instance-specific snapshot.
        // Uses api_request_to_socket with a long timeout since restore
        // loads the entire VM memory from disk.
        let source_url = format!("file://{}", instance_snap_dir.display());
        let body = serde_json::to_string(&serde_json::json!({
            "source_url": source_url
        }))?;

        let restore_result = tokio::time::timeout(
            Duration::from_secs(120),
            VmManager::api_request_to_socket(&api_socket, "PUT", "/api/v1/vm.restore", Some(&body)),
        )
        .await;

        match restore_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                kill_ch(&mut ch_process);
                return Err(e.context("vm.restore API call failed"));
            }
            Err(_) => {
                kill_ch(&mut ch_process);
                anyhow::bail!("restore timed out (120s)");
            }
        }

        // Resume the VM
        if let Err(e) =
            VmManager::api_request_to_socket(&api_socket, "PUT", "/api/v1/vm.resume", None).await
        {
            kill_ch(&mut ch_process);
            return Err(e.context("resume after restore"));
        }

        info!(
            "VM {} restored from snapshot in {:?}",
            vm_id,
            start.elapsed()
        );

        Ok(RestoredVm {
            ch_process,
            state_dir,
            api_socket,
            vsock_socket,
        })
    }

    /// Remove the golden snapshot files.
    #[allow(dead_code)]
    pub async fn cleanup(&mut self) -> Result<()> {
        if self.snapshot_dir.exists() {
            tokio::fs::remove_dir_all(&self.snapshot_dir)
                .await
                .context("remove snapshot dir")?;
        }
        self.ready = false;
        info!("golden snapshot removed");
        Ok(())
    }
}

/// A VM restored from a golden snapshot.
///
/// The caller owns the CH process and is responsible for cleanup.
pub struct RestoredVm {
    /// The Cloud Hypervisor child process.
    pub ch_process: tokio::process::Child,
    /// State directory for this VM instance.
    pub state_dir: PathBuf,
    /// API socket path.
    pub api_socket: PathBuf,
    /// Vsock socket path (connect here for agent ttrpc).
    pub vsock_socket: PathBuf,
}
