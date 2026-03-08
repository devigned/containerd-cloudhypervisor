use std::os::unix::io::IntoRawFd;
use std::path::Path;

use anyhow::{Context, Result};
use cloudhv_proto::AgentServiceClient;
use cloudhv_proto::HealthServiceClient;
use log::{debug, info};
use tokio::net::UnixStream;

use cloudhv_common::AGENT_VSOCK_PORT;

/// Client for communicating with the guest agent over vsock (ttrpc).
///
/// Cloud Hypervisor exposes a Unix socket on the host that proxies to the
/// guest's vsock. The guest agent listens on AGENT_VSOCK_PORT (10789).
///
/// After the vsock CONNECT handshake, the raw socket is wrapped in a
/// ttrpc async client for typed RPC calls.
pub struct VsockClient {
    socket_path: std::path::PathBuf,
}

impl VsockClient {
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// Perform the Cloud Hypervisor vsock CONNECT handshake and return
    /// the raw connected UnixStream.
    async fn vsock_connect(&self) -> Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to vsock socket: {}",
                    self.socket_path.display()
                )
            })?;

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
        let stream = buf_reader.into_inner().reunite(writer)?;
        Ok(stream)
    }

    /// Connect and create a ttrpc client wrapping the vsock stream.
    /// Returns (AgentServiceClient, HealthServiceClient).
    pub async fn connect_ttrpc(&self) -> Result<(AgentServiceClient, HealthServiceClient)> {
        info!(
            "connecting ttrpc client via vsock: {} port {}",
            self.socket_path.display(),
            AGENT_VSOCK_PORT
        );

        let stream = self.vsock_connect().await?;

        // Wrap the tokio UnixStream into a ttrpc Socket, then create a ttrpc Client
        let socket = ttrpc::r#async::transport::Socket::new(stream);
        let ttrpc_client = ttrpc::r#async::Client::new(socket);
        let agent_client = AgentServiceClient::new(ttrpc_client.clone());
        let health_client = HealthServiceClient::new(ttrpc_client);

        info!("ttrpc client connected to guest agent");
        Ok((agent_client, health_client))
    }

    /// Send a health check to the guest agent via ttrpc.
    /// Returns true if the agent is healthy and responding.
    pub async fn health_check(&self) -> Result<bool> {
        match self.connect_ttrpc().await {
            Ok((_agent, health)) => {
                let ctx = ttrpc::context::with_timeout(5);
                let req = cloudhv_proto::CheckRequest::new();
                match health.check(ctx, &req).await {
                    Ok(resp) => {
                        info!(
                            "agent health check: ready={}, version={}",
                            resp.ready, resp.version
                        );
                        Ok(resp.ready)
                    }
                    Err(e) => {
                        debug!("health check RPC failed: {}", e);
                        Ok(false)
                    }
                }
            }
            Err(e) => {
                debug!("health check connection failed: {}", e);
                Ok(false)
            }
        }
    }
}
