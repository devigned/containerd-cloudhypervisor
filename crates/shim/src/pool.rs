use std::collections::VecDeque;

use anyhow::{Context, Result};
use log::{debug, info, warn};

use cloudhv_common::types::RuntimeConfig;

use crate::snapshot::SnapshotManager;
use crate::vm::VmManager;
use crate::vsock::VsockClient;

/// A ready-to-use VM with an established agent connection.
pub struct WarmVm {
    pub vm: VmManager,
    pub agent: cloudhv_proto::AgentServiceClient,
    pub _health: cloudhv_proto::HealthServiceClient,
}

/// Pool of pre-warmed Cloud Hypervisor VMs for instant container start.
///
/// When a golden snapshot exists, the pool restores VMs from the snapshot
/// (~60ms) instead of cold-booting them (~460ms). Falls back to full boot
/// if no snapshot is available.
pub struct VmPool {
    available: VecDeque<WarmVm>,
    config: RuntimeConfig,
    target_size: usize,
    snapshot_mgr: SnapshotManager,
}

impl VmPool {
    pub fn new(config: RuntimeConfig) -> Self {
        let target_size = config.pool_size;
        let snapshot_mgr = SnapshotManager::new(config.clone());
        Self {
            available: VecDeque::with_capacity(target_size),
            config,
            target_size,
            snapshot_mgr,
        }
    }

    /// Pre-warm the pool by restoring VMs from the golden snapshot.
    /// Creates the golden snapshot lazily if it doesn't exist.
    /// Falls back to full boot if snapshot creation/restore fails.
    #[allow(dead_code)]
    pub async fn warm(&mut self) -> Result<()> {
        if self.target_size == 0 {
            debug!("VM pool disabled (pool_size=0)");
            return Ok(());
        }

        // Ensure we have a golden snapshot for fast restores
        if let Err(e) = self.snapshot_mgr.ensure_golden_snapshot().await {
            warn!("pool: golden snapshot creation failed, using cold boot: {e}");
        }

        info!(
            "pre-warming VM pool: target={}, current={}, snapshot={}",
            self.target_size,
            self.available.len(),
            self.snapshot_mgr.is_ready()
        );

        while self.available.len() < self.target_size {
            match self.create_warm_vm().await {
                Ok(warm) => {
                    info!(
                        "pool: VM {} warmed (cid={})",
                        warm.vm.vm_id(),
                        warm.vm.cid()
                    );
                    self.available.push_back(warm);
                }
                Err(e) => {
                    warn!("pool: failed to warm VM: {e}");
                    break;
                }
            }
        }

        info!("VM pool ready: {} VMs available", self.available.len());
        Ok(())
    }

    /// Try to acquire a pre-warmed VM from the pool.
    /// Returns None if the pool is empty.
    pub fn try_acquire(&mut self) -> Option<WarmVm> {
        let warm = self.available.pop_front();
        if warm.is_some() {
            debug!("pool: acquired VM, {} remaining", self.available.len());
        }
        warm
    }

    /// Create a new warm VM. Uses snapshot restore if available, otherwise
    /// falls back to full cold boot.
    pub async fn create_warm_vm(&self) -> Result<WarmVm> {
        let vm_id = format!("pool-{}", uuid::Uuid::new_v4().as_simple());

        // Try snapshot restore first (much faster)
        if self.snapshot_mgr.is_ready() {
            match self.create_warm_vm_from_snapshot(&vm_id).await {
                Ok(warm) => return Ok(warm),
                Err(e) => {
                    warn!("pool: snapshot restore failed, falling back to cold boot: {e}");
                }
            }
        }

        // Cold boot fallback
        self.create_warm_vm_cold_boot(vm_id).await
    }

    /// Restore a VM from the golden snapshot and connect ttrpc.
    async fn create_warm_vm_from_snapshot(&self, vm_id: &str) -> Result<WarmVm> {
        let restored = self
            .snapshot_mgr
            .restore_vm(vm_id)
            .await
            .context("snapshot restore")?;

        let vsock_socket = restored.vsock_socket.clone();
        let vm = VmManager::from_restored(restored, self.config.clone());

        // Connect ttrpc — agent should be immediately available after restore
        let vsock_client = VsockClient::new(&vsock_socket);
        let mut last_err = None;
        for attempt in 1..=5 {
            match vsock_client.connect_ttrpc().await {
                Ok((agent, health)) => {
                    info!(
                        "pool: VM {} restored from snapshot, ttrpc connected (attempt {attempt})",
                        vm.vm_id()
                    );
                    return Ok(WarmVm {
                        vm,
                        agent,
                        _health: health,
                    });
                }
                Err(e) => {
                    debug!("pool: ttrpc connect attempt {attempt} failed: {e}");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("ttrpc connect failed"))
            .context("ttrpc connect after snapshot restore"))
    }

    /// Full cold boot a VM (fallback when no snapshot is available).
    async fn create_warm_vm_cold_boot(&self, vm_id: String) -> Result<WarmVm> {
        let mut vm = VmManager::new(vm_id.clone(), self.config.clone())
            .context("failed to create VmManager")?;

        vm.prepare().await.context("failed to prepare VM")?;
        vm.start_swtpm().await.context("failed to start swtpm")?;

        vm.spawn_virtiofsd().context("failed to spawn virtiofsd")?;
        vm.spawn_vmm().context("failed to spawn VMM")?;

        let (vfsd_result, vmm_result) =
            tokio::join!(vm.wait_virtiofsd_ready(), vm.wait_vmm_ready(),);
        vfsd_result.context("virtiofsd not ready")?;
        vmm_result.context("VMM not ready")?;

        vm.create_and_boot_vm(None, None)
            .await
            .context("failed to boot VM")?;
        vm.wait_for_agent().await.context("agent not ready")?;

        let vsock_client = VsockClient::new(vm.vsock_socket());
        let mut last_err = None;
        for attempt in 1..=5 {
            match vsock_client.connect_ttrpc().await {
                Ok((agent, health)) => {
                    info!(
                        "pool: VM {} cold-booted, ttrpc connected (attempt {attempt})",
                        vm_id
                    );
                    return Ok(WarmVm {
                        vm,
                        agent,
                        _health: health,
                    });
                }
                Err(e) => {
                    debug!("ttrpc connect attempt {attempt} failed: {e}");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("ttrpc connect failed"))
            .context("failed to connect ttrpc after 5 attempts"))
    }

    /// Create a warm VM with a specific ID (cold boot only, for backward compat).
    #[allow(dead_code)]
    pub async fn create_warm_vm_with_id(&self, vm_id: String) -> Result<WarmVm> {
        self.create_warm_vm_cold_boot(vm_id).await
    }

    /// Refill the pool back to target size.
    #[allow(dead_code)]
    pub async fn refill(&mut self) {
        while self.available.len() < self.target_size {
            match self.create_warm_vm().await {
                Ok(warm) => {
                    info!("pool: refilled VM {}", warm.vm.vm_id());
                    self.available.push_back(warm);
                }
                Err(e) => {
                    warn!("pool: refill failed: {e}");
                    break;
                }
            }
        }
    }

    /// Shut down and clean up all VMs in the pool.
    #[allow(dead_code)]
    pub async fn drain(&mut self) {
        info!("draining VM pool ({} VMs)", self.available.len());
        while let Some(mut warm) = self.available.pop_front() {
            let _ = warm.vm.cleanup().await;
        }
    }

    /// Number of available VMs in the pool.
    #[allow(dead_code)]
    pub fn available_count(&self) -> usize {
        self.available.len()
    }

    /// Whether pooling is enabled.
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.target_size > 0
    }

    /// Access the snapshot manager (e.g., for lazy golden snapshot creation).
    #[allow(dead_code)]
    pub fn snapshot_manager(&mut self) -> &mut SnapshotManager {
        &mut self.snapshot_mgr
    }
}
