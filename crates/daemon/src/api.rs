//! ttrpc API server for the sandbox daemon.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;
use crate::pool::Pool;
use crate::shadow::SnapshotManager;

/// Per-VM agent connection, stored after first container start.
type AgentRegistry = Arc<Mutex<HashMap<String, cloudhv_proto::AgentServiceClient>>>;

/// The daemon API server. Listens on a Unix socket and serves
/// AcquireSandbox, ReleaseSandbox, and Status RPCs.
pub struct ApiServer {
    config: DaemonConfig,
    pool: Arc<Pool>,
    snapshots: Arc<SnapshotManager>,
    agents: AgentRegistry,
}

impl ApiServer {
    pub fn new(config: DaemonConfig, pool: Arc<Pool>, snapshots: Arc<SnapshotManager>) -> Self {
        Self {
            config,
            pool,
            snapshots,
            agents: Arc::new(Mutex::new(HashMap::new())),
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

        let listener = tokio::net::UnixListener::bind(socket_path)?;

        loop {
            let (stream, _) = listener.accept().await?;
            let pool = Arc::clone(&self.pool);
            let snapshots = Arc::clone(&self.snapshots);
            let config = self.config.clone();
            let agents = Arc::clone(&self.agents);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, &pool, &snapshots, &config, &agents).await
                {
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
    agents: &AgentRegistry,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    while buf_reader.read_line(&mut line).await? > 0 {
        let request: serde_json::Value = serde_json::from_str(line.trim())?;
        let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");

        log::debug!(
            "request: method={} vm_id={} container_id={}",
            method,
            request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("-"),
            request
                .get("container_id")
                .and_then(|v| v.as_str())
                .unwrap_or("-")
        );

        let response = match method {
            "AcquireSandbox" => handle_acquire(pool, snapshots, config, agents, &request).await,
            "ReleaseSandbox" => handle_release(pool, agents, &request).await,
            "AddContainer" => handle_add_container(pool, agents, &request).await,
            "KillContainer" => handle_kill_container(agents, &request).await,
            "WaitContainer" => handle_wait_container(agents, &request).await,
            "DeleteContainer" => handle_delete_container(agents, &request).await,
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
    config: &DaemonConfig,
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let tap_name = request
        .get("tap_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tap_mac = request
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
    let container_id = request
        .get("container_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let config_json_b64 = request
        .get("config_json")
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
        let state_dir = std::path::PathBuf::from(&config.state_dir).join(&vm_id);
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

                // Hot-plug TAP NIC into the restored VM so networking works

                if !tap_name.is_empty() && !tap_mac.is_empty() {
                    let net_json = serde_json::json!({
                        "tap": tap_name,
                        "mac": tap_mac,
                    });
                    if let Err(e) = crate::vm_lifecycle::api_request_with_body(
                        &api_socket,
                        "PUT",
                        "/api/v1/vm.add-net",
                        &net_json.to_string(),
                    )
                    .await
                    {
                        warn!("TAP hot-plug failed on warm restore: {e:#}");
                        return serde_json::json!({"error": format!("TAP hot-plug: {e:#}")});
                    }
                    info!("TAP {} hot-plugged into warm VM {}", tap_name, vm_id);
                }

                // Connect agent and configure network for warm restore
                let ip_cidr = _ip_cidr;
                let gateway = _gateway;
                if !ip_cidr.is_empty() && !gateway.is_empty() {
                    match crate::vm_lifecycle::connect_agent(&vsock_socket).await {
                        Ok((agent, _health)) => {
                            agents.lock().await.insert(vm_id.clone(), agent.clone());

                            let parts: Vec<&str> = ip_cidr.split('/').collect();
                            let ip = parts[0];
                            let prefix: u32 =
                                parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
                            let mut net_req = cloudhv_proto::ConfigureNetworkRequest::new();
                            net_req.ip_address = ip.to_string();
                            net_req.gateway = gateway.to_string();
                            net_req.prefix_len = prefix;
                            net_req.device = "eth0".to_string();
                            let ctx =
                                ttrpc::context::with_duration(std::time::Duration::from_secs(30));
                            if let Err(e) = agent.configure_network(ctx, &net_req).await {
                                warn!("configure_network failed on warm restore: {e:#}");
                            } else {
                                info!("guest network configured: ip={ip}/{prefix} gw={gateway}");
                            }
                        }
                        Err(e) => {
                            warn!("agent connect failed on warm restore: {e:#}");
                        }
                    }
                }

                // Register as active so release() works correctly
                pool.register_active(crate::pool::PoolVm {
                    vm_id: vm_id.clone(),
                    api_socket: api_socket.clone(),
                    vsock_socket: vsock_socket.clone(),
                    cid,
                    ch_pid,
                    created_at: std::time::Instant::now(),
                })
                .await;

                return serde_json::json!({
                    "vm_id": vm_id,
                    "api_socket": api_socket.to_string_lossy(),
                    "vsock_socket": vsock_socket.to_string_lossy(),
                    "cid": cid,
                    "ch_pid": ch_pid,
                    "from_snapshot": true,
                    "container_pid": 0,
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

            // Hot-plug rootfs and run container inside the VM
            let container_pid = if !erofs_path.is_empty() && !container_id.is_empty() {
                match run_container_in_vm(
                    &vm.api_socket,
                    &vm.vsock_socket,
                    &vm.vm_id,
                    agents,
                    tap_name,
                    tap_mac,
                    erofs_path,
                    container_id,
                    config_json_b64,
                )
                .await
                {
                    Ok(pid) => pid,
                    Err(e) => {
                        return serde_json::json!({"error": format!("run container: {e:#}")});
                    }
                }
            } else {
                0
            };

            serde_json::json!({
                "vm_id": vm.vm_id,
                "api_socket": vm.api_socket.to_string_lossy(),
                "vsock_socket": vm.vsock_socket.to_string_lossy(),
                "cid": vm.cid,
                "ch_pid": vm.ch_pid,
                "from_snapshot": false,
                "container_pid": container_pid,
            })
        }
        Err(e) => {
            serde_json::json!({"error": format!("acquire failed: {:#}", e)})
        }
    }
}

/// Hot-plug an erofs rootfs and run a container via the guest agent.
/// If `tap_name` and `tap_mac` are provided, hot-plugs a TAP NIC first.
#[allow(clippy::too_many_arguments)]
async fn run_container_in_vm(
    api_socket: &std::path::Path,
    vsock_socket: &std::path::Path,
    vm_id: &str,
    agents: &AgentRegistry,
    tap_name: &str,
    tap_mac: &str,
    erofs_path: &str,
    container_id: &str,
    config_json_b64: &str,
) -> anyhow::Result<u32> {
    use crate::vm_lifecycle;

    // Hot-plug TAP NIC (CH runs on the host and can see the TAP device
    // created by the shim via `ip link set <tap> netns 1`)
    if !tap_name.is_empty() && !tap_mac.is_empty() {
        let net_json = serde_json::json!({
            "tap": tap_name,
            "mac": tap_mac,
        });
        vm_lifecycle::api_request_with_body(
            api_socket,
            "PUT",
            "/api/v1/vm.add-net",
            &net_json.to_string(),
        )
        .await
        .context("hot-plug TAP NIC")?;
        info!("TAP {} hot-plugged into VM", tap_name);
    }

    // Hot-plug rootfs disk
    let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
    let disk_json = serde_json::json!({
        "path": erofs_path,
        "readonly": true,
        "id": disk_id,
    });
    vm_lifecycle::api_request_with_body(
        api_socket,
        "PUT",
        "/api/v1/vm.add-disk",
        &disk_json.to_string(),
    )
    .await
    .context("hot-plug rootfs")?;

    // Connect to agent (or reuse existing connection)
    let agent = {
        let existing = agents.lock().await.get(vm_id).cloned();
        if let Some(a) = existing {
            a
        } else {
            let (a, _health) = vm_lifecycle::connect_agent(vsock_socket)
                .await
                .context("agent connect for container")?;
            agents.lock().await.insert(vm_id.to_string(), a.clone());
            a
        }
    };

    // Decode OCI config
    use base64::Engine;
    let config_json = if config_json_b64.is_empty() {
        b"{}".to_vec()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(config_json_b64)
            .unwrap_or_else(|_| b"{}".to_vec())
    };

    // Run container
    let mut req = cloudhv_proto::CreateContainerRequest::new();
    req.container_id = container_id.to_string();
    req.config_json = config_json;
    req.rootfs_preattached = false;
    req.erofs_layers = 1;

    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
    let resp = agent
        .run_container(ctx, &req)
        .await
        .context("RunContainer")?;

    info!(
        "container {} started in VM (pid={})",
        container_id, resp.pid
    );
    Ok(resp.pid)
}

async fn handle_add_container(
    pool: &Arc<Pool>,
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");
    let erofs_path = request
        .get("erofs_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let container_id = request
        .get("container_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let config_json_b64 = request
        .get("config_json")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if vm_id.is_empty() || container_id.is_empty() {
        return serde_json::json!({"error": "vm_id and container_id required"});
    }

    let vm = match pool.get_active(vm_id).await {
        Some(vm) => vm,
        None => {
            return serde_json::json!({"error": format!("VM {} not found in active set", vm_id)});
        }
    };

    match run_container_in_vm(
        &vm.api_socket,
        &vm.vsock_socket,
        vm_id,
        agents,
        "", // no TAP hot-plug for additional containers
        "",
        erofs_path,
        container_id,
        config_json_b64,
    )
    .await
    {
        Ok(pid) => serde_json::json!({"container_pid": pid}),
        Err(e) => serde_json::json!({"error": format!("add container: {e:#}")}),
    }
}

async fn handle_release(
    pool: &Arc<Pool>,
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");

    if vm_id.is_empty() {
        return serde_json::json!({"error": "vm_id required"});
    }

    // Remove agent connection for this VM
    agents.lock().await.remove(vm_id);

    match pool.release(vm_id).await {
        Ok(()) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"error": format!("release failed: {:#}", e)}),
    }
}

async fn handle_kill_container(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");
    let container_id = request
        .get("container_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let signal = request.get("signal").and_then(|v| v.as_u64()).unwrap_or(15) as u32;

    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"error": format!("no agent for VM {vm_id}")}),
    };

    let mut req = cloudhv_proto::KillContainerRequest::new();
    req.container_id = container_id.to_string();
    req.signal = signal;
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));

    match agent.kill_container(ctx, &req).await {
        Ok(_) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"error": format!("kill_container: {e}")}),
    }
}

async fn handle_wait_container(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");
    let container_id = request
        .get("container_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"error": format!("no agent for VM {vm_id}")}),
    };

    let mut req = cloudhv_proto::WaitContainerRequest::new();
    req.container_id = container_id.to_string();
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(86400));

    match agent.wait_container(ctx, &req).await {
        Ok(resp) => serde_json::json!({
            "exit_status": resp.exit_status,
            "exited_at": resp.exited_at,
        }),
        Err(e) => serde_json::json!({
            "exit_status": 137,
            "exited_at": "",
            "error": format!("wait_container: {e}"),
        }),
    }
}

async fn handle_delete_container(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");
    let container_id = request
        .get("container_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"ok": true}), // VM already gone, nothing to delete
    };

    let mut req = cloudhv_proto::DeleteContainerRequest::new();
    req.container_id = container_id.to_string();
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
    let _ = agent.delete_container(ctx, &req).await;

    serde_json::json!({"ok": true})
}

async fn handle_status(pool: &Arc<Pool>, snapshots: &Arc<SnapshotManager>) -> serde_json::Value {
    serde_json::json!({
        "pool_ready": pool.ready_count().await,
        "active_vms": pool.active_count(),
        "shadow_vms_running": snapshots.active_shadow_count().await,
        "snapshot_keys": snapshots.snapshot_keys().await,
    })
}
