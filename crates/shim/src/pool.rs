use std::collections::VecDeque;

use anyhow::{Context, Result};
use log::{debug, info, warn};

use cloudhv_common::types::RuntimeConfig;

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
/// The pool maintains a queue of booted VMs with active agent connections.
/// When a container is created, it claims a VM from the pool instead of
/// booting a new one. The pool refills in the background.
pub struct VmPool {
    available: VecDeque<WarmVm>,
    config: RuntimeConfig,
    target_size: usize,
}

impl VmPool {
    pub fn new(config: RuntimeConfig) -> Self {
        let target_size = config.pool_size;
        Self {
            available: VecDeque::with_capacity(target_size),
            config,
            target_size,
        }
    }

    /// Pre-warm the pool by booting VMs up to the target size.
    #[allow(dead_code)]
    pub async fn warm(&mut self) -> Result<()> {
        if self.target_size == 0 {
            debug!("VM pool disabled (pool_size=0)");
            return Ok(());
        }

        info!(
            "pre-warming VM pool: target={}, current={}",
            self.target_size,
            self.available.len()
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

    /// Create a new warm VM (boot + connect agent).
    /// Used both for pool pre-warming and on-demand creation.
    pub async fn create_warm_vm(&self) -> Result<WarmVm> {
        let vm_id = format!("pool-{}", uuid::Uuid::new_v4().as_simple());
        self.create_warm_vm_with_id(vm_id).await
    }

    /// Create a warm VM with a specific ID.
    pub async fn create_warm_vm_with_id(&self, vm_id: String) -> Result<WarmVm> {
        let mut vm = VmManager::new(vm_id.clone(), self.config.clone())
            .context("failed to create VmManager")?;

        vm.prepare().await.context("failed to prepare VM")?;
        vm.start_swtpm().await.context("failed to start swtpm")?;

        // Optimization: spawn virtiofsd and CH VMM in parallel
        vm.spawn_virtiofsd().context("failed to spawn virtiofsd")?;
        vm.spawn_vmm().context("failed to spawn VMM")?;

        // Wait for both sockets in parallel
        let (vfsd_result, vmm_result) =
            tokio::join!(vm.wait_virtiofsd_ready(), vm.wait_vmm_ready(),);
        vfsd_result.context("virtiofsd not ready")?;
        vmm_result.context("VMM not ready")?;

        vm.create_and_boot_vm(None, None)
            .await
            .context("failed to boot VM")?;
        vm.wait_for_agent().await.context("agent not ready")?;

        // Retry ttrpc connect — the agent may need a moment after vsock socket appears
        let vsock_client = VsockClient::new(vm.vsock_socket());
        let mut last_err = None;
        for attempt in 1..=5 {
            match vsock_client.connect_ttrpc().await {
                Ok((agent, health)) => {
                    info!("ttrpc connected on attempt {attempt}");
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

    /// Refill the pool back to target size (call periodically or after acquire).
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
}
