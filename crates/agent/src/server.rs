use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use log::{debug, error, info};
use tokio::signal::unix::{signal, SignalKind};

use cloudhv_common::AGENT_VSOCK_PORT;

use crate::container::ContainerManager;

/// ttrpc server that listens on vsock and handles container lifecycle RPCs.
pub struct AgentServer {
    container_manager: Arc<Mutex<ContainerManager>>,
}

impl AgentServer {
    pub fn new() -> Self {
        Self {
            container_manager: Arc::new(Mutex::new(ContainerManager::new())),
        }
    }

    /// Start the ttrpc server and listen on vsock.
    pub async fn run(&self) -> Result<()> {
        info!("starting agent ttrpc server on vsock port {}", AGENT_VSOCK_PORT);

        // Bind to vsock
        // The guest-side vsock address is: AF_VSOCK, CID=VMADDR_CID_ANY(u32::MAX), port=10789
        //
        // For ttrpc, we create a listener and register our service implementations.
        //
        // TODO: Full ttrpc server implementation with AgentService and HealthService.
        // For Phase 1, we'll use a simplified approach:
        // 1. Create a vsock listener
        // 2. Accept connections
        // 3. Handle ttrpc-encoded requests
        //
        // The ttrpc crate supports async server via:
        //   ttrpc::asynchronous::Server::new()
        //       .register_service(agent_service)
        //       .register_service(health_service)
        //       .start_on_listener(listener)

        let vsock_fd = create_vsock_listener(AGENT_VSOCK_PORT)?;
        info!("vsock listener created on port {}", AGENT_VSOCK_PORT);

        // TODO: Register ttrpc services and start server
        // For now, keep the agent alive waiting for signals
        info!("agent ready, waiting for connections...");

        // Wait for SIGTERM or SIGINT
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
            }
        }

        info!("agent server stopped");
        Ok(())
    }
}

/// Create a vsock listener socket bound to the given port.
/// Only available on Linux (AF_VSOCK).
#[cfg(target_os = "linux")]
fn create_vsock_listener(port: u32) -> Result<i32> {
    use libc::{
        bind, listen, socket, sockaddr_vm, AF_VSOCK, SOCK_STREAM, VMADDR_CID_ANY,
    };
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
fn create_vsock_listener(port: u32) -> Result<i32> {
    anyhow::bail!("vsock is only supported on Linux (AF_VSOCK)")
}
