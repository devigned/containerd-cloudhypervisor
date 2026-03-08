use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use log::{error, info};
use tokio::process::Command;

use cloudhv_common::VIRTIOFS_GUEST_MOUNT;

/// Tracks the state of a container managed by the agent.
#[derive(Debug)]
struct Container {
    _id: String,
    _bundle_path: PathBuf,
    pid: Option<u32>,
    exit_code: Option<i32>,
    state: ContainerState,
    _stdout_path: Option<PathBuf>,
    _stderr_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContainerState {
    Created,
    Running,
    Stopped,
}

/// Manages container lifecycle via runc.
pub struct ContainerManager {
    containers: HashMap<String, Container>,
}

impl Default for ContainerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerManager {
    pub fn new() -> Self {
        Self {
            containers: HashMap::new(),
        }
    }

    /// Create a new container from an OCI bundle.
    ///
    /// The bundle is expected at: /containers/<container_id>/
    /// which is the virtio-fs mount from the host.
    /// If stdout/stderr paths are provided, runc output is redirected there.
    pub async fn create(
        &mut self,
        container_id: &str,
        bundle_path: &str,
        stdout_path: Option<&str>,
        stderr_path: Option<&str>,
    ) -> Result<u32> {
        info!("creating container: id={}", container_id);

        let bundle = if bundle_path.is_empty() {
            PathBuf::from(VIRTIOFS_GUEST_MOUNT).join(container_id)
        } else {
            PathBuf::from(bundle_path)
        };

        if !bundle.exists() {
            anyhow::bail!("bundle path does not exist: {}", bundle.display());
        }

        // Set up stdout/stderr redirection
        let stdout_file = if let Some(p) = stdout_path {
            if !p.is_empty() {
                Some(
                    std::fs::File::create(p)
                        .with_context(|| format!("failed to create stdout file: {p}"))?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let stderr_file = if let Some(p) = stderr_path {
            if !p.is_empty() {
                Some(
                    std::fs::File::create(p)
                        .with_context(|| format!("failed to create stderr file: {p}"))?,
                )
            } else {
                None
            }
        } else {
            None
        };

        let stdout_stdio = stdout_file.map(Stdio::from).unwrap_or_else(Stdio::null);
        let stderr_stdio = stderr_file.map(Stdio::from).unwrap_or_else(Stdio::null);

        let output = Command::new("runc")
            .arg("create")
            .arg("--bundle")
            .arg(&bundle)
            .arg(container_id)
            .stdin(Stdio::null())
            .stdout(stdout_stdio)
            .stderr(stderr_stdio)
            .output()
            .await
            .context("failed to execute runc create")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("runc create failed: {}", stderr);
        }

        let pid = self.get_container_pid(container_id).await?;

        let container = Container {
            _id: container_id.to_string(),
            _bundle_path: bundle,
            pid: Some(pid),
            exit_code: None,
            state: ContainerState::Created,
            _stdout_path: stdout_path.map(PathBuf::from),
            _stderr_path: stderr_path.map(PathBuf::from),
        };

        self.containers.insert(container_id.to_string(), container);
        info!("container created: id={}, pid={}", container_id, pid);
        Ok(pid)
    }

    /// Start a previously created container.
    pub async fn start(&mut self, container_id: &str) -> Result<u32> {
        info!("starting container: {}", container_id);

        let output = Command::new("runc")
            .arg("start")
            .arg(container_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute runc start")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("runc start failed: {}", stderr);
        }

        if let Some(container) = self.containers.get_mut(container_id) {
            container.state = ContainerState::Running;
            let pid = container.pid.unwrap_or(0);
            info!("container started: id={}, pid={}", container_id, pid);
            Ok(pid)
        } else {
            anyhow::bail!("container not found: {}", container_id);
        }
    }

    /// Send a signal to a container.
    pub async fn kill(&self, container_id: &str, signal: u32) -> Result<()> {
        info!("killing container: {} signal={}", container_id, signal);

        let output = Command::new("runc")
            .arg("kill")
            .arg(container_id)
            .arg(signal.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute runc kill")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("runc kill failed: {}", stderr);
        }

        Ok(())
    }

    /// Delete a stopped container.
    pub async fn delete(&mut self, container_id: &str) -> Result<(u32, i32)> {
        info!("deleting container: {}", container_id);

        let output = Command::new("runc")
            .arg("delete")
            .arg("--force")
            .arg(container_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute runc delete")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("runc delete failed (may be ok): {}", stderr);
        }

        let (pid, exit_code) = if let Some(container) = self.containers.remove(container_id) {
            (container.pid.unwrap_or(0), container.exit_code.unwrap_or(0))
        } else {
            (0, 0)
        };

        info!(
            "container deleted: id={}, pid={}, exit_code={}",
            container_id, pid, exit_code
        );
        Ok((pid, exit_code))
    }

    /// Wait for a container to exit, returning (exit_code, exited_at).
    pub async fn wait(&mut self, container_id: &str) -> Result<(i32, String)> {
        info!("waiting for container: {}", container_id);

        // Poll runc state until the container is stopped
        loop {
            let state = self.get_runc_state(container_id).await?;

            if let Some(status) = state.get("status").and_then(|s| s.as_str()) {
                if status == "stopped" {
                    // Container has exited
                    let exit_code = 0; // runc state doesn't always include exit code
                    let exited_at = chrono::Utc::now().to_rfc3339();

                    if let Some(container) = self.containers.get_mut(container_id) {
                        container.state = ContainerState::Stopped;
                        container.exit_code = Some(exit_code);
                    }

                    return Ok((exit_code, exited_at));
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
    }

    /// Get the PID of a container from runc state.
    async fn get_container_pid(&self, container_id: &str) -> Result<u32> {
        let state = self.get_runc_state(container_id).await?;
        state
            .get("pid")
            .and_then(|p| p.as_u64())
            .map(|p| p as u32)
            .context("container pid not found in runc state")
    }

    /// Query runc for container state (JSON).
    async fn get_runc_state(
        &self,
        container_id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let output = Command::new("runc")
            .arg("state")
            .arg(container_id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute runc state")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("runc state failed: {}", stderr);
        }

        let state: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("failed to parse runc state output")?;

        state
            .as_object()
            .cloned()
            .context("runc state is not a JSON object")
    }

    /// Get the state of a container as a proto response.
    pub async fn state(
        &self,
        container_id: &str,
    ) -> Result<cloudhv_proto::generated::agent::StateContainerResponse> {
        use ::protobuf::EnumOrUnknown;
        use cloudhv_proto::generated::agent::{
            ContainerState as ProtoState, StateContainerResponse,
        };

        let mut resp = StateContainerResponse::new();
        resp.container_id = container_id.to_string();

        if let Some(container) = self.containers.get(container_id) {
            resp.pid = container.pid.unwrap_or(0);
            resp.status = match container.state {
                ContainerState::Created => EnumOrUnknown::new(ProtoState::CREATED),
                ContainerState::Running => EnumOrUnknown::new(ProtoState::RUNNING),
                ContainerState::Stopped => EnumOrUnknown::new(ProtoState::STOPPED),
            };
            resp.exit_status = container.exit_code.unwrap_or(0) as u32;
        } else {
            resp.status = EnumOrUnknown::new(ProtoState::UNKNOWN);
        }

        Ok(resp)
    }
}
