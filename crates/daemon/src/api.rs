//! ttrpc API server for the sandbox daemon.

use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};

use crate::config::DaemonConfig;
use crate::pool::Pool;
use crate::shadow::SnapshotManager;

/// The daemon API server. Listens on a Unix socket and serves
/// AcquireSandbox, ReleaseSandbox, and Status RPCs.
pub struct ApiServer {
    config: DaemonConfig,
    pool: Arc<Pool>,
    snapshots: Arc<SnapshotManager>,
}

impl ApiServer {
    pub fn new(config: DaemonConfig, pool: Arc<Pool>, snapshots: Arc<SnapshotManager>) -> Self {
        Self {
            config,
            pool,
            snapshots,
        }
    }

    /// Start the ttrpc server. Blocks until shutdown.
    pub async fn serve(&self) -> Result<()> {
        let socket_path = &self.config.socket_path;

        // Remove stale socket
        let _ = std::fs::remove_file(socket_path);

        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        info!("daemon API listening on {}", socket_path);

        // For now, use a simple JSON-over-Unix-socket protocol.
        // This will be replaced with ttrpc once the proto codegen is wired up.
        let listener = tokio::net::UnixListener::bind(socket_path)?;

        loop {
            let (stream, _) = listener.accept().await?;
            let pool = Arc::clone(&self.pool);
            let snapshots = Arc::clone(&self.snapshots);
            let config = self.config.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, &pool, &snapshots, &config).await {
                    log::warn!("client error: {:#}", e);
                }
            });
        }
    }
}

/// Handle a single client connection.
/// Simple JSON request/response protocol (will be replaced with ttrpc).
async fn handle_connection(
    stream: tokio::net::UnixStream,
    pool: &Arc<Pool>,
    snapshots: &Arc<SnapshotManager>,
    config: &DaemonConfig,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    log::info!("client connected");

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    while buf_reader.read_line(&mut line).await? > 0 {
        log::info!("received: {}", line.trim());
        let request: serde_json::Value = serde_json::from_str(line.trim())?;
        let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");

        let response = match method {
            "AcquireSandbox" => handle_acquire(pool, snapshots, config, &request).await,
            "ReleaseSandbox" => handle_release(pool, &request).await,
            "Status" => handle_status(pool, snapshots).await,
            _ => serde_json::json!({"error": format!("unknown method: {}", method)}),
        };

        let mut resp_str = serde_json::to_string(&response)?;
        resp_str.push('\n');
        writer.write_all(resp_str.as_bytes()).await?;
        writer.flush().await?;
        log::info!("responded: {}", resp_str.trim());
        line.clear();
    }

    Ok(())
}

async fn handle_acquire(
    pool: &Arc<Pool>,
    snapshots: &Arc<SnapshotManager>,
    _config: &DaemonConfig,
    request: &serde_json::Value,
) -> serde_json::Value {
    let _tap_name = request
        .get("tap_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let _tap_mac = request
        .get("tap_mac")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let _ip_cidr = request
        .get("ip_cidr")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let _gateway = request
        .get("gateway")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let image_key = request
        .get("image_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let erofs_path = request
        .get("erofs_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Check for warm workload snapshot
    if !image_key.is_empty()
        && snapshots
            .has_snapshot(
                image_key,
                if erofs_path.is_empty() {
                    None
                } else {
                    Some(erofs_path)
                },
            )
            .await
    {
        let vm_id = format!("warm-{}-{}", image_key, std::process::id());
        let state_dir = std::path::PathBuf::from("/run/cloudhv/daemon").join(&vm_id);
        if let Err(e) = std::fs::create_dir_all(&state_dir) {
            return serde_json::json!({"error": format!("create state dir: {e}")});
        }

        let cid = pool.next_cid_pub();
        match snapshots
            .restore_from_snapshot(image_key, &vm_id, cid, &state_dir)
            .await
        {
            Ok((ch_pid, api_socket, vsock_socket)) => {
                info!("warm restore for image_key={}", image_key);
                return serde_json::json!({
                    "vm_id": vm_id,
                    "api_socket": api_socket.to_string_lossy(),
                    "vsock_socket": vsock_socket.to_string_lossy(),
                    "cid": cid,
                    "ch_pid": ch_pid,
                    "from_snapshot": true,
                });
            }
            Err(e) => {
                warn!("warm restore failed, falling back to pool: {:#}", e);
                // Fall through to pool acquire
            }
        }
    }

    match pool.acquire().await {
        Ok(vm) => {
            // Trigger async pool replenishment
            let pool_clone = Arc::clone(pool);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                pool_clone.replenish_one().await;
            });

            // Trigger shadow VM for this image (if not already snapshotted)
            if !image_key.is_empty() && !erofs_path.is_empty() {
                snapshots.trigger_shadow(image_key, erofs_path).await;
            }

            serde_json::json!({
                "vm_id": vm.vm_id,
                "api_socket": vm.api_socket.to_string_lossy(),
                "vsock_socket": vm.vsock_socket.to_string_lossy(),
                "cid": vm.cid,
                "ch_pid": vm.ch_pid,
                "from_snapshot": false,
            })
        }
        Err(e) => {
            serde_json::json!({"error": format!("acquire failed: {:#}", e)})
        }
    }
}

async fn handle_release(pool: &Arc<Pool>, request: &serde_json::Value) -> serde_json::Value {
    let vm_id = request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");

    if vm_id.is_empty() {
        return serde_json::json!({"error": "vm_id required"});
    }

    match pool.release(vm_id).await {
        Ok(()) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"error": format!("release failed: {:#}", e)}),
    }
}

async fn handle_status(pool: &Arc<Pool>, snapshots: &Arc<SnapshotManager>) -> serde_json::Value {
    serde_json::json!({
        "pool_ready": pool.ready_count().await,
        "active_vms": pool.active_count(),
        "shadow_vms_running": snapshots.active_shadow_count().await,
        "snapshot_keys": snapshots.snapshot_keys().await,
    })
}
