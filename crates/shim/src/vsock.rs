use std::path::Path;

use anyhow::{Context, Result};
use log::{debug, info};
use tokio::net::UnixStream;

use cloudhv_common::AGENT_VSOCK_PORT;

/// Client for communicating with the guest agent over vsock (ttrpc).
///
/// Cloud Hypervisor exposes a Unix socket on the host that proxies to the
/// guest's vsock. The guest agent listens on AGENT_VSOCK_PORT (10789).
///
/// For Phase 1, we connect to the vsock Unix socket directly and issue
/// ttrpc calls. The ttrpc client is created from the generated proto code.
pub struct VsockClient {
    /// Path to the vsock Unix socket on the host.
    socket_path: std::path::PathBuf,
}

impl VsockClient {
    /// Create a new vsock client targeting the given host-side socket.
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// Connect to the guest agent and return a ttrpc client.
    ///
    /// Cloud Hypervisor's vsock socket requires a connect command:
    /// write "CONNECT <port>\n" then the socket becomes a bidirectional
    /// stream to the guest on that port.
    pub async fn connect(&self) -> Result<UnixStream> {
        info!(
            "connecting to guest agent via vsock: {} port {}",
            self.socket_path.display(),
            AGENT_VSOCK_PORT
        );

        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to vsock socket: {}",
                    self.socket_path.display()
                )
            })?;

        // Cloud Hypervisor vsock requires a CONNECT handshake:
        // Send "CONNECT <port>\n" and wait for "OK <port>\n"
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (reader, mut writer) = stream.into_split();
        let connect_cmd = format!("CONNECT {AGENT_VSOCK_PORT}\n");
        writer.write_all(connect_cmd.as_bytes()).await?;

        let mut buf_reader = BufReader::new(reader);
        let mut response = String::new();
        buf_reader.read_line(&mut response).await?;

        if !response.starts_with("OK") {
            anyhow::bail!("vsock CONNECT failed: {}", response.trim());
        }

        debug!("vsock connected to guest agent port {}", AGENT_VSOCK_PORT);

        // Reunite the split stream
        let stream = buf_reader.into_inner().reunite(writer)?;
        Ok(stream)
    }

    /// Send a health check to the guest agent.
    /// Returns true if the agent is healthy and responding.
    pub async fn health_check(&self) -> Result<bool> {
        match self.connect().await {
            Ok(_stream) => {
                // TODO: Send ttrpc HealthService.Check RPC
                // For now, successful connection = healthy
                Ok(true)
            }
            Err(e) => {
                debug!("health check failed: {}", e);
                Ok(false)
            }
        }
    }
}
