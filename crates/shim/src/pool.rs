use std::collections::VecDeque;

use anyhow::{Context, Result};
use log::{debug, info, warn};

use cloudhv_common::types::RuntimeConfig;

use crate::snapshot::SnapshotManager;
use crate::vm::VmManager;
use crate::vsock::VsockClient;

/// A ready-to-use VM from the warm pool.
///
/// Pool VMs are minimal (rootfs disk + vsock only). virtiofsd and shared_dir
/// are set up when the VM is assigned to a sandbox. The ttrpc agent is
/// pre-connected during warmup so the first container start doesn't pay the
/// connection cost.
pub struct WarmVm {
    pub vm: VmManager,
    pub agent: cloudhv_proto::AgentServiceClient,
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

    /// Number of warm VMs ready in the pool.
    pub fn available_count(&self) -> usize {
        self.available.len()
    }

    /// Push a pre-created warm VM into the pool.
    pub fn push_warm(&mut self, vm: WarmVm) {
        self.available.push_back(vm);
    }

    /// Refill the pool back to target_size. Call after acquiring a VM.
    pub async fn refill(&mut self) {
        while self.available.len() < self.target_size {
            match self.create_warm_vm().await {
                Ok(warm) => {
                    info!(
                        "pool: refilled VM {} (cid={})",
                        warm.vm.vm_id(),
                        warm.vm.cid()
                    );
                    self.available.push_back(warm);
                }
                Err(e) => {
                    warn!("pool: refill failed: {e}");
                    break;
                }
            }
        }
    }

    /// Create a new warm VM. Uses snapshot restore if available, otherwise
    /// falls back to full cold boot.
    async fn create_warm_vm(&self) -> Result<WarmVm> {
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

    /// Restore a VM from the golden snapshot.
    ///
    /// Pool VMs are minimal: rootfs disk + vsock only. virtiofsd and shared_dir
    /// are set up later when the VM is assigned to a sandbox. The ttrpc agent
    /// is eagerly connected here so sandbox start doesn't pay that cost.
    async fn create_warm_vm_from_snapshot(&self, vm_id: &str) -> Result<WarmVm> {
        let restored = self
            .snapshot_mgr
            .restore_vm(vm_id)
            .await
            .context("snapshot restore")?;

        let vm = VmManager::from_restored(restored, self.config.clone());

        // Pre-connect the ttrpc agent (retry up to 5 times, 200ms apart)
        let agent = Self::connect_agent(&vm)
            .await
            .context("pool: agent pre-connect")?;

        info!(
            "pool: VM {} restored from snapshot (minimal, no virtiofsd)",
            vm.vm_id()
        );

        Ok(WarmVm { vm, agent })
    }

    /// Full cold boot a VM (fallback when no snapshot is available).
    ///
    /// Pool VMs are minimal: no virtiofsd or shared_dir. The ttrpc agent is
    /// eagerly connected after boot.
    async fn create_warm_vm_cold_boot(&self, vm_id: String) -> Result<WarmVm> {
        let mut vm = VmManager::new(vm_id.clone(), self.config.clone())
            .context("failed to create VmManager")?;

        vm.prepare().await.context("failed to prepare VM")?;
        vm.start_swtpm().await.context("failed to start swtpm")?;

        // No virtiofsd — pool VMs boot with rootfs disk + vsock only
        vm.spawn_vmm().context("failed to spawn VMM")?;
        vm.wait_vmm_ready().await.context("VMM not ready")?;

        vm.create_and_boot_vm_for_snapshot()
            .await
            .context("failed to boot VM")?;
        vm.wait_for_agent().await.context("agent not ready")?;

        // Pre-connect the ttrpc agent
        let agent = Self::connect_agent(&vm)
            .await
            .context("pool: agent pre-connect")?;

        info!("pool: VM {} cold-booted (minimal, no virtiofsd)", vm_id);

        Ok(WarmVm { vm, agent })
    }

    /// Connect the ttrpc agent client to a VM, retrying up to 5 times with
    /// 200ms between attempts.
    async fn connect_agent(vm: &VmManager) -> Result<cloudhv_proto::AgentServiceClient> {
        let vsock = VsockClient::new(vm.vsock_socket());
        let mut last_err = None;
        for attempt in 1..=5 {
            match vsock.connect_ttrpc().await {
                Ok((agent, _health)) => {
                    debug!("pool: agent connected on attempt {attempt}");
                    return Ok(agent);
                }
                Err(e) => {
                    debug!("pool: agent connect attempt {attempt}/5 failed: {e}");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("agent connect failed")))
    }

    /// Shut down and clean up all VMs in the pool.
    pub async fn drain(&mut self) {
        info!("draining VM pool ({} VMs)", self.available.len());
        while let Some(mut warm) = self.available.pop_front() {
            let _ = warm.vm.cleanup().await;
        }
    }
}
