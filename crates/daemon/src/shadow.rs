//! Shadow VM management for warm workload snapshots.
//!
//! A shadow VM is a throwaway instance that exists solely to produce a
//! warm workload snapshot. It runs outside Kubernetes, has no networking,
//! and is destroyed after the snapshot is captured.
//!
//! Flow:
//! 1. Restore from base snapshot (clean VM, agent ready)
//! 2. Hot-plug container rootfs disk
//! 3. RunContainer via agent RPC (workload starts)
//! 4. Wait for warmup (configurable, default 30s)
//! 5. Pause → snapshot → destroy
//! 6. Cache workload snapshot under image_key

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;
use crate::pool::Pool;

/// Metadata for a cached workload snapshot.
struct SnapshotEntry {
    /// Path to the snapshot directory.
    dir: PathBuf,
    /// Last time this snapshot was used (for LRU eviction).
    last_used: std::time::Instant,
    /// mtime of the erofs image when the snapshot was created.
    erofs_mtime: Option<std::time::SystemTime>,
}

/// Manages workload snapshots created by shadow VMs.
pub struct SnapshotManager {
    config: DaemonConfig,
    pool: Arc<Pool>,
    /// Cached workload snapshots: image_key → entry
    snapshots: Mutex<HashMap<String, SnapshotEntry>>,
    /// Shadow VMs currently running (image_key → task handle)
    active_shadows: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Maximum number of cached snapshots.
    max_snapshots: usize,
}

impl SnapshotManager {
    pub fn new(config: DaemonConfig, pool: Arc<Pool>) -> Arc<Self> {
        let max = config.max_snapshots;
        Arc::new(Self {
            config,
            pool,
            snapshots: Mutex::new(HashMap::new()),
            active_shadows: Mutex::new(HashMap::new()),
            max_snapshots: max,
        })
    }

    /// Check if a valid workload snapshot exists for the given image key.
    /// Invalidates the snapshot if the erofs image has been modified since
    /// the snapshot was created (e.g. image upgrade).
    pub async fn has_snapshot(&self, image_key: &str, erofs_path: Option<&str>) -> bool {
        let mut snapshots = self.snapshots.lock().await;
        if let Some(entry) = snapshots.get(image_key) {
            // Validate erofs mtime — if the image changed, snapshot is stale
            if let (Some(erofs), Some(snap_mtime)) = (erofs_path, entry.erofs_mtime) {
                if let Ok(meta) = std::fs::metadata(erofs) {
                    if let Ok(current_mtime) = meta.modified() {
                        if current_mtime != snap_mtime {
                            info!("snapshot {} invalidated: erofs mtime changed", image_key);
                            let dir = entry.dir.clone();
                            snapshots.remove(image_key);
                            let _ = std::fs::remove_dir_all(&dir);
                            return false;
                        }
                    }
                }
            }
            true
        } else {
            false
        }
    }

    /// Get the snapshot directory and update last-used time.
    pub async fn get_snapshot_dir(&self, image_key: &str) -> Option<PathBuf> {
        let mut snapshots = self.snapshots.lock().await;
        if let Some(entry) = snapshots.get_mut(image_key) {
            entry.last_used = std::time::Instant::now();
            Some(entry.dir.clone())
        } else {
            None
        }
    }

    /// Restore a VM from a workload snapshot. Returns the VM details.
    /// The caller is responsible for configuring networking after restore.
    pub async fn restore_from_snapshot(
        &self,
        image_key: &str,
        _vm_id: &str,
        cid: u64,
        state_dir: &Path,
    ) -> Result<(u32, PathBuf, PathBuf)> {
        use crate::vm_lifecycle;

        let snap_dir = self
            .get_snapshot_dir(image_key)
            .await
            .context("snapshot not found")?;

        let vsock_socket = state_dir.join("vsock.sock");
        let snapshot_work_dir =
            vm_lifecycle::prepare_snapshot(&snap_dir, state_dir, cid, &vsock_socket)?;

        let ch_pid = vm_lifecycle::spawn_ch(&self.config, state_dir).await?;
        let api_socket = state_dir.join("api.sock");
        vm_lifecycle::wait_ch_ready(&api_socket).await?;
        vm_lifecycle::restore_vm(&api_socket, &snapshot_work_dir).await?;
        vm_lifecycle::resume_vm(&api_socket).await?;

        // Skip agent health check for warm restores — the agent was verified
        // when the snapshot was created. The shim will connect directly.
        // Connecting here would hold the vsock proxy and delay the shim.

        info!("restored workload snapshot for image_key={}", image_key);
        Ok((ch_pid, api_socket, vsock_socket))
    }

    /// Trigger shadow VM creation for an image key.
    /// Does nothing if a shadow is already running or a snapshot exists.
    pub async fn trigger_shadow(self: &Arc<Self>, image_key: &str, erofs_path: &str) {
        // Skip if snapshot already exists and is valid
        if self.has_snapshot(image_key, Some(erofs_path)).await {
            return;
        }

        // Skip if shadow already running for this image
        {
            let shadows = self.active_shadows.lock().await;
            if shadows.contains_key(image_key) {
                return;
            }
        }

        let mgr = Arc::clone(self);
        let key = image_key.to_string();
        let erofs = erofs_path.to_string();

        let handle = tokio::spawn(async move {
            if let Err(e) = mgr.run_shadow(&key, &erofs).await {
                warn!("shadow VM failed for image_key={}: {:#}", key, e);
            }
            mgr.active_shadows.lock().await.remove(&key);
        });

        self.active_shadows
            .lock()
            .await
            .insert(image_key.to_string(), handle);
        info!("shadow VM triggered for image_key={}", image_key);
    }

    /// Run a shadow VM: restore base → hot-plug rootfs → run workload →
    /// warmup → pause → snapshot → destroy.
    async fn run_shadow(&self, image_key: &str, erofs_path: &str) -> Result<()> {
        use crate::vm_lifecycle;

        let t0 = std::time::Instant::now();
        let vm_id = format!("shadow-{}-{}", image_key, std::process::id());
        let state_dir = PathBuf::from(&self.config.state_dir).join(&vm_id);
        std::fs::create_dir_all(&state_dir)?;

        // 1. Acquire a clean VM from pool (or restore synchronously)
        let pool_vm = self.pool.acquire().await.context("acquire for shadow")?;
        let api_socket = pool_vm.api_socket.clone();
        let vsock_socket = pool_vm.vsock_socket.clone();
        let ch_pid = pool_vm.ch_pid;

        info!(
            "shadow VM {} using pool VM {} (pid={})",
            vm_id, pool_vm.vm_id, ch_pid
        );

        // 2. Hot-plug container rootfs
        let disk_json = serde_json::json!({
            "path": erofs_path,
            "readonly": true,
            "id": "ctr-rootfs",
        });
        vm_lifecycle::api_request_with_body(
            &api_socket,
            "PUT",
            "/api/v1/vm.add-disk",
            &disk_json.to_string(),
        )
        .await
        .context("hot-plug rootfs for shadow")?;

        let t_hotplug = t0.elapsed().as_millis();
        info!("shadow {}: rootfs hot-plugged ({}ms)", vm_id, t_hotplug);

        // 3. Run container via agent RPC
        // Connect to agent and send RunContainer
        let agent_connected = vm_lifecycle::connect_agent(&vsock_socket).await?;
        let (agent, _health) = agent_connected;

        let mut run_req = cloudhv_proto::CreateContainerRequest::new();
        run_req.container_id = format!("shadow-ctr-{}", image_key);
        run_req.rootfs_preattached = false;
        run_req.erofs_layers = 1;
        // Intentionally empty OCI config: the shadow container only exists to
        // warm up the rootfs and runtime (file caches, shared libraries, etc.).
        // The agent falls back to defaults for an empty config, which is
        // sufficient for warmup. The real pod's OCI config will be applied
        // when restoring from this snapshot.
        run_req.config_json = b"{}".to_vec();

        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
        let run_resp = agent
            .run_container(ctx, &run_req)
            .await
            .context("RunContainer in shadow VM")?;

        let t_run = t0.elapsed().as_millis();
        info!(
            "shadow {}: container started (pid={}, {}ms)",
            vm_id, run_resp.pid, t_run
        );

        // 4. Wait for warmup
        let warmup = self.config.warmup_duration_secs;
        info!("shadow {}: warming up for {}s...", vm_id, warmup);
        tokio::time::sleep(std::time::Duration::from_secs(warmup)).await;

        // 5. Pause + snapshot
        let snap_dir = self.config.snapshot_cache_dir().join(image_key);
        std::fs::create_dir_all(&snap_dir)?;

        vm_lifecycle::pause_vm(&api_socket).await?;
        vm_lifecycle::snapshot_vm(&api_socket, &snap_dir).await?;

        let t_snap = t0.elapsed().as_millis();
        info!("shadow {}: snapshot created ({}ms total)", vm_id, t_snap);

        // Strip kernel ip= from snapshot (networking is per-pod)
        strip_kernel_ip(&snap_dir)?;

        // 6. Cache the snapshot with metadata
        let erofs_mtime = std::fs::metadata(erofs_path)
            .ok()
            .and_then(|m| m.modified().ok());

        {
            let mut snapshots = self.snapshots.lock().await;

            // LRU eviction if at capacity
            while snapshots.len() >= self.max_snapshots {
                // Find the least recently used entry
                let lru_key = snapshots
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone());
                if let Some(key) = lru_key {
                    if let Some(entry) = snapshots.remove(&key) {
                        info!("evicting LRU snapshot: {}", key);
                        let _ = std::fs::remove_dir_all(&entry.dir);
                    }
                } else {
                    break;
                }
            }

            snapshots.insert(
                image_key.to_string(),
                SnapshotEntry {
                    dir: snap_dir,
                    last_used: std::time::Instant::now(),
                    erofs_mtime,
                },
            );
        }

        // 7. Destroy the shadow VM
        self.pool.release(&pool_vm.vm_id).await?;

        info!(
            "shadow {} complete: hotplug={}ms run={}ms warmup={}s snapshot={}ms",
            vm_id,
            t_hotplug,
            t_run - t_hotplug,
            warmup,
            t_snap - t_run
        );

        Ok(())
    }

    /// Number of active shadow VMs.
    pub async fn active_shadow_count(&self) -> usize {
        self.active_shadows.lock().await.len()
    }

    /// List cached snapshot keys.
    pub async fn snapshot_keys(&self) -> Vec<String> {
        self.snapshots.lock().await.keys().cloned().collect()
    }
}

/// Remove `ip=...` from the kernel cmdline in a snapshot's config.json.
fn strip_kernel_ip(snap_dir: &Path) -> Result<()> {
    let config_path = snap_dir.join("config.json");
    let config_str = std::fs::read_to_string(&config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&config_str)?;

    if let Some(cmdline) = config.pointer_mut("/payload/cmdline") {
        if let Some(s) = cmdline.as_str() {
            let stripped = s
                .split_whitespace()
                .filter(|part| !part.starts_with("ip="))
                .collect::<Vec<_>>()
                .join(" ");
            *cmdline = serde_json::json!(stripped);
        }
    }

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    Ok(())
}
