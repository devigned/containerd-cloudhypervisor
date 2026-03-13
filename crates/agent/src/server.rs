use std::sync::Arc;

use crate::proto::agent::*;
use crate::proto::agent_ttrpc::{self, AgentService, HealthService};
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info};
use tokio::signal::unix::{signal, SignalKind};
use ttrpc::r#async::TtrpcContext;

use cloudhv_common::AGENT_VSOCK_PORT;

use crate::container::ContainerManager;

// --- AgentService implementation ---

struct AgentServiceHandler {
    container_manager: Arc<tokio::sync::Mutex<ContainerManager>>,
}

#[async_trait]
impl AgentService for AgentServiceHandler {
    async fn create_container(
        &self,
        _ctx: &TtrpcContext,
        req: CreateContainerRequest,
    ) -> ttrpc::Result<CreateContainerResponse> {
        info!("RPC: create_container id={}", req.container_id);
        let mut mgr = self.container_manager.lock().await;
        let volumes: Vec<crate::container::VolumeInfo> = req
            .volumes
            .iter()
            .map(|v| crate::container::VolumeInfo {
                destination: v.destination.clone(),
                source: v.source.clone(),
                readonly: v.readonly,
                is_block: v.volume_type == crate::proto::agent::VolumeType::BLOCK.into(),
                fs_type: v.fs_type.clone(),
            })
            .collect();
        let pid = mgr
            .create(&req.container_id, &req.bundle_path, &volumes)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("create_container failed: {e:#}")))?;
        let mut resp = CreateContainerResponse::new();
        resp.pid = pid;
        Ok(resp)
    }

    async fn start_container(
        &self,
        _ctx: &TtrpcContext,
        req: StartContainerRequest,
    ) -> ttrpc::Result<StartContainerResponse> {
        info!("RPC: start_container id={}", req.container_id);
        let mut mgr = self.container_manager.lock().await;
        let pid = mgr
            .start(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("start_container failed: {e}")))?;
        let mut resp = StartContainerResponse::new();
        resp.pid = pid;
        Ok(resp)
    }

    async fn kill_container(
        &self,
        _ctx: &TtrpcContext,
        req: KillContainerRequest,
    ) -> ttrpc::Result<KillContainerResponse> {
        info!(
            "RPC: kill_container id={} signal={}",
            req.container_id, req.signal
        );
        let mgr = self.container_manager.lock().await;
        mgr.kill(&req.container_id, req.signal)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("kill_container failed: {e}")))?;
        Ok(KillContainerResponse::new())
    }

    async fn delete_container(
        &self,
        _ctx: &TtrpcContext,
        req: DeleteContainerRequest,
    ) -> ttrpc::Result<DeleteContainerResponse> {
        info!("RPC: delete_container id={}", req.container_id);
        let mut mgr = self.container_manager.lock().await;
        let (pid, exit_code) = mgr
            .delete(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("delete_container failed: {e}")))?;
        let mut resp = DeleteContainerResponse::new();
        resp.pid = pid;
        resp.exit_status = exit_code as u32;
        Ok(resp)
    }

    async fn wait_container(
        &self,
        _ctx: &TtrpcContext,
        req: WaitContainerRequest,
    ) -> ttrpc::Result<WaitContainerResponse> {
        info!("RPC: wait_container id={}", req.container_id);

        // Briefly lock to get the exit receiver, then drop the lock.
        let mut rx = {
            let mgr = self.container_manager.lock().await;
            mgr.get_exit_receiver(&req.container_id).ok_or_else(|| {
                ttrpc::Error::Others(format!(
                    "wait_container: unknown container {}",
                    req.container_id
                ))
            })?
        };

        // Clone data out of the Ref guard so we never hold a non-Send borrow across .await.
        let already_exited = { rx.borrow().clone() };

        let (exit_code, exited_at) = if let Some(status) = already_exited {
            (status.code, status.exited_at)
        } else {
            // Wait for the container to exit without holding the container_manager lock.
            let timeout = tokio::time::Duration::from_secs(600);
            let result = tokio::select! {
                changed = rx.changed() => {
                    match changed {
                        Ok(()) => {
                            let status = { rx.borrow().clone() };
                            match status {
                                Some(s) => (s.code, s.exited_at),
                                None => (137, chrono::Utc::now().to_rfc3339()),
                            }
                        }
                        Err(_) => (137, chrono::Utc::now().to_rfc3339()),
                    }
                }
                _ = tokio::time::sleep(timeout) => {
                    info!("wait_container: timeout after 600s for {}", req.container_id);
                    (137, chrono::Utc::now().to_rfc3339())
                }
            };
            result
        };

        // Briefly re-lock to mark the container as stopped.
        {
            let mut mgr = self.container_manager.lock().await;
            mgr.mark_stopped(&req.container_id, exit_code);
        }

        let mut resp = WaitContainerResponse::new();
        resp.exit_status = exit_code as u32;
        resp.exited_at = exited_at;
        Ok(resp)
    }

    async fn exec_process(
        &self,
        _ctx: &TtrpcContext,
        req: ExecProcessRequest,
    ) -> ttrpc::Result<ExecProcessResponse> {
        info!(
            "RPC: exec_process container={} exec_id={}",
            req.container_id, req.exec_id
        );
        Err(ttrpc::Error::RpcStatus(ttrpc::get_status(
            ttrpc::Code::UNIMPLEMENTED,
            "exec_process not yet implemented".to_string(),
        )))
    }

    async fn state_container(
        &self,
        _ctx: &TtrpcContext,
        req: StateContainerRequest,
    ) -> ttrpc::Result<StateContainerResponse> {
        debug!("RPC: state_container id={}", req.container_id);
        let mgr = self.container_manager.lock().await;
        mgr.state(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("state_container failed: {e}")))
    }

    async fn get_mem_info(
        &self,
        _ctx: &TtrpcContext,
        _req: GetMemInfoRequest,
    ) -> ttrpc::Result<GetMemInfoResponse> {
        debug!("RPC: get_mem_info");
        let content = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| ttrpc::Error::Others(format!("read /proc/meminfo: {e}")))?;

        let mut resp = GetMemInfoResponse::new();
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let val: u64 = parts[1].parse().unwrap_or(0);
            match parts[0] {
                "MemTotal:" => resp.mem_total_kb = val,
                "MemFree:" => resp.mem_free_kb = val,
                "MemAvailable:" => resp.mem_available_kb = val,
                "Buffers:" => resp.buffers_kb = val,
                "Cached:" => resp.cached_kb = val,
                "SwapTotal:" => resp.swap_total_kb = val,
                "SwapFree:" => resp.swap_free_kb = val,
                _ => {}
            }
        }
        Ok(resp)
    }

    async fn get_container_logs(
        &self,
        _ctx: &TtrpcContext,
        req: GetContainerLogsRequest,
    ) -> ttrpc::Result<GetContainerLogsResponse> {
        let id = &req.container_id;
        let offset = req.offset as usize;

        let log_buf = {
            let mgr = self.container_manager.lock().await;
            mgr.get_log_buffer(id)
        };

        let mut resp = GetContainerLogsResponse::new();
        if let Some(buf) = log_buf {
            let logs = buf.lock().await;
            // Return data from offset onwards
            if offset < logs.stdout.len() {
                resp.stdout = logs.stdout[offset..].to_vec();
            }
            if offset < logs.stderr.len() {
                resp.stderr = logs.stderr[offset..].to_vec();
            }
            resp.offset = std::cmp::max(logs.stdout.len(), logs.stderr.len()) as u64;
            resp.eof = logs.eof;
        } else {
            resp.eof = true;
        }
        Ok(resp)
    }
}

struct HealthServiceHandler;

#[async_trait]
impl HealthService for HealthServiceHandler {
    async fn check(&self, _ctx: &TtrpcContext, _req: CheckRequest) -> ttrpc::Result<CheckResponse> {
        let mut resp = CheckResponse::new();
        resp.ready = true;
        resp.version = env!("CARGO_PKG_VERSION").to_string();
        Ok(resp)
    }
}

// --- Server ---

/// ttrpc server that listens on vsock and handles container lifecycle RPCs.
pub struct AgentServer {
    container_manager: Arc<tokio::sync::Mutex<ContainerManager>>,
}

impl Default for AgentServer {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentServer {
    pub fn new() -> Self {
        Self {
            container_manager: Arc::new(tokio::sync::Mutex::new(ContainerManager::new())),
        }
    }

    /// Start the ttrpc server and listen on vsock.
    pub async fn run(&self) -> Result<()> {
        info!(
            "starting agent ttrpc server on vsock port {}",
            AGENT_VSOCK_PORT
        );

        let vsock_fd = create_vsock_listener(AGENT_VSOCK_PORT)?;
        info!("vsock listener created on port {}", AGENT_VSOCK_PORT);

        let agent_service = Arc::new(AgentServiceHandler {
            container_manager: self.container_manager.clone(),
        });
        let health_service = Arc::new(HealthServiceHandler);

        let agent_svc = agent_ttrpc::create_agent_service(agent_service);
        let health_svc = agent_ttrpc::create_health_service(health_service);

        #[cfg(target_os = "linux")]
        let mut server = {
            unsafe {
                ttrpc::asynchronous::Server::new()
                    .add_vsock_listener(vsock_fd)
                    .expect("failed to add vsock listener")
            }
            .register_service(agent_svc)
            .register_service(health_svc)
        };

        #[cfg(not(target_os = "linux"))]
        let mut server = {
            let _ = vsock_fd;
            ttrpc::asynchronous::Server::new()
                .register_service(agent_svc)
                .register_service(health_svc)
        };

        server
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("failed to start ttrpc server: {e}"))?;

        info!("ttrpc server started, accepting connections");

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM, shutting down"),
            _ = sigint.recv() => info!("received SIGINT, shutting down"),
        }

        server
            .shutdown()
            .await
            .map_err(|e| anyhow::anyhow!("failed to shutdown ttrpc server: {e}"))?;
        info!("agent server stopped");
        Ok(())
    }
}

/// Create a vsock listener socket bound to the given port.
/// Only available on Linux (AF_VSOCK).
#[cfg(target_os = "linux")]
fn create_vsock_listener(port: u32) -> Result<i32> {
    use libc::{bind, listen, sockaddr_vm, socket, AF_VSOCK, SOCK_STREAM, VMADDR_CID_ANY};
    use std::mem;

    unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM, 0);
        if fd < 0 {
            anyhow::bail!(
                "failed to create vsock socket: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut addr: sockaddr_vm = mem::zeroed();
        addr.svm_family = AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = VMADDR_CID_ANY;
        addr.svm_port = port;

        let addr_ptr = &addr as *const sockaddr_vm as *const libc::sockaddr;
        let addr_len = mem::size_of::<sockaddr_vm>() as libc::socklen_t;

        if bind(fd, addr_ptr, addr_len) < 0 {
            libc::close(fd);
            anyhow::bail!(
                "failed to bind vsock port {}: {}",
                port,
                std::io::Error::last_os_error()
            );
        }

        if listen(fd, 128) < 0 {
            libc::close(fd);
            anyhow::bail!(
                "failed to listen on vsock port {}: {}",
                port,
                std::io::Error::last_os_error()
            );
        }

        debug!("vsock listener ready: fd={}, port={}", fd, port);
        Ok(fd)
    }
}

#[cfg(not(target_os = "linux"))]
fn create_vsock_listener(_port: u32) -> Result<i32> {
    anyhow::bail!("vsock is only supported on Linux (AF_VSOCK)")
}
