//! Thin containerd shim instance that delegates VM lifecycle to the
//! sandbox daemon. The shim handles networking (TAP/tc), erofs rootfs
//! conversion, and container lifecycle RPCs. The daemon handles VM
//! boot, pooling, and snapshot management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

use chrono::{DateTime, Utc};
use containerd_shimkit::sandbox::instance::{Instance, InstanceConfig};
use containerd_shimkit::sandbox::sync::WaitableCell;
use containerd_shimkit::sandbox::Error;
use log::info;
use tokio::sync::OnceCell;

use crate::config::load_config;
use crate::daemon_client::DaemonClient;

/// Milliseconds since Unix epoch for TIMING logs.
pub fn epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

const EROFS_CACHE_DIR: &str = "/run/cloudhv/erofs-cache";
const CRI_CONTAINER_TYPE: &str = "/annotations/io.kubernetes.cri.container-type";
const CRI_SANDBOX_ID: &str = "/annotations/io.kubernetes.cri.sandbox-id";

trait ResultExt<T> {
    fn ctx(self, msg: &str) -> Result<T, Error>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for Result<T, E> {
    fn ctx(self, msg: &str) -> Result<T, Error> {
        self.map_err(|e| Error::Any(anyhow::anyhow!("{msg}: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Static shared state
// ---------------------------------------------------------------------------

static VMS: LazyLock<RwLock<HashMap<String, Arc<SharedVmState>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_vm(sandbox_id: &str) -> Option<Arc<SharedVmState>> {
    VMS.read()
        .unwrap_or_else(|e| e.into_inner())
        .get(sandbox_id)
        .cloned()
}

// ---------------------------------------------------------------------------
// Shared VM state (thin — daemon owns the VM)
// ---------------------------------------------------------------------------

struct SharedVmState {
    /// Agent ttrpc client (connected after daemon acquire).
    agent: OnceCell<cloudhv_proto::AgentServiceClient>,
    /// vsock proxy socket path (from daemon).
    vsock_socket: PathBuf,
    /// Container count (sandbox counts as 1).
    container_count: AtomicUsize,
    /// TAP device name (created by shim in pod netns).
    tap_name: Option<String>,
    tap_mac: Option<String>,
    ip_cidr: Option<String>,
    gateway: Option<String>,
    netns: Option<String>,
    cgroups_path: Option<String>,
    /// VM ID assigned by the daemon (for release).
    daemon_vm_id: String,
    /// Daemon socket path.
    daemon_socket: String,
}

/// Connect to the guest agent over vsock.
async fn get_or_connect_agent(
    vm_state: &SharedVmState,
) -> Result<cloudhv_proto::AgentServiceClient, Error> {
    vm_state
        .agent
        .get_or_try_init(|| async {
            let vsock_client = crate::vsock::VsockClient::new(&vm_state.vsock_socket);

            // Tight-poll for first 500ms (fast for warm-restored VMs
            // where the agent is already running)
            let tight_poll_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(500);
            while tokio::time::Instant::now() < tight_poll_deadline {
                match vsock_client.connect_ttrpc().await {
                    Ok((agent, _health)) => return Ok(agent),
                    Err(_) => tokio::task::yield_now().await,
                }
            }

            // Fixed 100ms retry for next 5s (vsock proxy may need time)
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut last_err = String::new();
            while tokio::time::Instant::now() < deadline {
                match vsock_client.connect_ttrpc().await {
                    Ok((agent, _health)) => return Ok(agent),
                    Err(e) => {
                        last_err = format!("{e:#}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }

            // Last resort: longer backoff for cold boot
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
            for attempt in 0..8u32 {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                match vsock_client.connect_ttrpc().await {
                    Ok((agent, _health)) => return Ok(agent),
                    Err(e) => {
                        last_err = format!("{e:#}");
                        let delay = (500u64 * 2u64.pow(attempt)).min(3000);
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                }
            }

            Err(Error::Any(anyhow::anyhow!("agent connect: {last_err}")))
        })
        .await
        .cloned()
}

// ---------------------------------------------------------------------------
// CloudHvInstance — the shimkit Instance implementation
// ---------------------------------------------------------------------------

pub struct CloudHvInstance {
    id: String,
    sandbox_id: String,
    bundle: PathBuf,
    exit: Arc<WaitableCell<(u32, DateTime<Utc>)>>,
}

impl Instance for CloudHvInstance {
    async fn new(id: String, cfg: &InstanceConfig) -> Result<Self, Error> {
        let spec_path = cfg.bundle.join("config.json");
        let spec_str = std::fs::read_to_string(&spec_path)
            .map_err(|e| Error::Any(anyhow::anyhow!("read spec: {e}")))?;
        let spec: serde_json::Value = serde_json::from_str(&spec_str)
            .map_err(|e| Error::Any(anyhow::anyhow!("parse spec: {e}")))?;

        let sandbox_id = spec
            .pointer(CRI_SANDBOX_ID)
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();

        Ok(Self {
            id: id.clone(),
            sandbox_id,
            bundle: cfg.bundle.clone(),
            exit: Arc::new(WaitableCell::new()),
        })
    }

    async fn start(&self) -> Result<u32, Error> {
        let spec = std::fs::read_to_string(self.bundle.join("config.json"))
            .map_err(|e| Error::Any(anyhow::anyhow!("read spec: {e}")))?;
        let spec_json: serde_json::Value = serde_json::from_str(&spec)
            .map_err(|e| Error::Any(anyhow::anyhow!("parse spec: {e}")))?;

        let container_type = spec_json
            .pointer(CRI_CONTAINER_TYPE)
            .and_then(|v| v.as_str())
            .unwrap_or("container");

        match container_type {
            "sandbox" => self.start_sandbox().await,
            _ => self.start_container().await,
        }
    }

    async fn kill(&self, signal: u32) -> Result<(), Error> {
        info!("CloudHvInstance::kill id={} signal={}", self.id, signal);

        if let Some(vm_state) = get_vm(&self.sandbox_id) {
            if let Ok(agent) = get_or_connect_agent(&vm_state).await {
                let mut req = cloudhv_proto::KillContainerRequest::new();
                req.container_id = self.id.clone();
                req.signal = signal;
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
                let _ = agent.kill_container(ctx, &req).await;
            }
        }

        // Don't set exit here — the exit watcher (from start_container) will
        // set it when the container actually exits. Setting it eagerly causes
        // shimkit's TaskState to transition to Exited prematurely, making
        // subsequent kill() calls fail with "Exited => Killing" and bricking
        // the node in a StopPodSandbox retry loop.

        Ok(())
    }

    async fn delete(&self) -> Result<(), Error> {
        info!("CloudHvInstance::delete id={}", self.id);
        let t_total = std::time::Instant::now();

        if let Some(vm_state) = get_vm(&self.sandbox_id) {
            let prev = vm_state.container_count.fetch_sub(1, Ordering::SeqCst);

            if prev <= 1 {
                // Last container — release VM to daemon
                let client = DaemonClient::new(&vm_state.daemon_socket);
                if let Err(e) = client.release_sandbox(&vm_state.daemon_vm_id) {
                    info!("daemon release failed (non-fatal): {e:#}");
                }

                // Clean up TAP
                if let (Some(ref netns), Some(ref tap)) = (&vm_state.netns, &vm_state.tap_name) {
                    crate::netns::cleanup_tap(netns, tap).await;
                    info!("cleaned up TAP {tap}");
                }

                VMS.write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.sandbox_id);

                info!(
                    "TIMING delete {} @{}: total={}ms (sandbox released)",
                    self.id,
                    epoch_ms(),
                    t_total.elapsed().as_millis()
                );
            } else {
                // Container delete — just notify agent
                if let Ok(agent) = get_or_connect_agent(&vm_state).await {
                    let mut req = cloudhv_proto::DeleteContainerRequest::new();
                    req.container_id = self.id.clone();
                    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
                    let _ = agent.delete_container(ctx, &req).await;
                }

                info!(
                    "TIMING delete {} @{}: total={}ms (container only, {} remaining)",
                    self.id,
                    epoch_ms(),
                    t_total.elapsed().as_millis(),
                    prev - 1
                );
            }
        }

        let _ = self.exit.set((0, Utc::now()));
        Ok(())
    }

    async fn wait(&self) -> (u32, DateTime<Utc>) {
        *self.exit.wait().await
    }
}

// ---------------------------------------------------------------------------
// Sandbox and container lifecycle
// ---------------------------------------------------------------------------

impl CloudHvInstance {
    /// Set up networking for the sandbox. No VM interaction — the daemon
    /// will provide the VM when the first container starts.
    async fn start_sandbox(&self) -> Result<u32, Error> {
        let sandbox_id = self.id.clone();
        let t_total = std::time::Instant::now();

        let spec_path = self.bundle.join("config.json");
        let sandbox_spec = parse_sandbox_spec(&spec_path);

        let config = load_config(None).ctx("config error")?;

        // Set up TAP device in the pod's network namespace
        let t_tap = std::time::Instant::now();
        let (tap_name, tap_mac, ip_config) = if let Some(ref netns) = sandbox_spec.netns {
            let mut result = None;
            for attempt in 0..5 {
                match crate::netns::setup_tap(netns, &sandbox_id).await {
                    Ok(tap_info) => {
                        if attempt > 0 {
                            info!("TAP setup succeeded after {attempt} retries");
                        }
                        result = Some(tap_info);
                        break;
                    }
                    Err(e) => {
                        if attempt < 4 {
                            info!("TAP setup attempt {attempt} failed ({e:#}), retrying...");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }
            match result {
                Some(tap_info) => {
                    info!(
                        "TAP created: dev={} mac={} ip={} gw={}",
                        tap_info.tap_name, tap_info.mac, tap_info.ip_cidr, tap_info.gateway
                    );
                    (
                        Some(tap_info.tap_name),
                        Some(tap_info.mac),
                        Some((tap_info.ip_cidr, tap_info.gateway)),
                    )
                }
                None => (None, None, None),
            }
        } else {
            (None, None, None)
        };
        let tap_ms = t_tap.elapsed().as_millis();

        let (ip_cidr, gateway) = match &ip_config {
            Some((cidr, gw)) => (Some(cidr.clone()), Some(gw.clone())),
            None => (None, None),
        };

        // Store sandbox state — no VM yet (daemon provides it in start_container)
        let vm_state = Arc::new(SharedVmState {
            agent: OnceCell::new(),
            vsock_socket: PathBuf::new(),
            container_count: AtomicUsize::new(1),
            tap_name,
            tap_mac,
            ip_cidr,
            gateway,
            netns: sandbox_spec.netns.clone(),
            cgroups_path: sandbox_spec.cgroups_path.clone(),
            daemon_vm_id: String::new(),
            daemon_socket: config.daemon_socket.clone(),
        });

        VMS.write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sandbox_id.clone(), vm_state);

        info!(
            "TIMING start_sandbox {} @{}: tap={}ms total={}ms",
            sandbox_id,
            epoch_ms(),
            tap_ms,
            t_total.elapsed().as_millis()
        );

        Ok(std::process::id())
    }

    /// Acquire a VM from the daemon and start the container.
    async fn start_container(&self) -> Result<u32, Error> {
        let container_id = &self.id;
        let t_total = std::time::Instant::now();
        info!(
            "TIMING start_container_enter {} @{}",
            container_id,
            epoch_ms()
        );

        let vm_state = get_vm(&self.sandbox_id).ok_or_else(|| {
            Error::Any(anyhow::anyhow!("sandbox VM not found: {}", self.sandbox_id))
        })?;

        let rootfs_path = self.bundle.join("rootfs");

        // Resolve image digest for stable cache key + warm snapshot lookup.
        // This must happen before erofs conversion so the same image always
        // maps to the same erofs file (regardless of container snapshot path).
        let image_key = image_key_from_spec(&self.bundle)
            .await
            .unwrap_or_else(|| stable_hash_hex(&rootfs_path.to_string_lossy()));

        // Convert rootfs to erofs (cached by image key). The shim does this
        // because it has access to the bundle rootfs that containerd prepared.
        let t_erofs = std::time::Instant::now();
        let erofs_path = prepare_erofs(&rootfs_path, &image_key)?;
        let erofs_ms = t_erofs.elapsed().as_millis();

        // Read OCI config for the container
        let spec_path = self.bundle.join("config.json");
        let config_json = std::fs::read(&spec_path).unwrap_or_default();

        // Check if a VM is already acquired for this sandbox (multi-container pod).
        // Only the first container triggers daemon acquire + warm snapshot lookup.
        let already_acquired = !vm_state.daemon_vm_id.is_empty();

        let (active_state, from_snapshot, acquire_ms, container_pid) = if already_acquired {
            info!(
                "VM already acquired for sandbox {}, adding container {}",
                self.sandbox_id, container_id
            );
            // Additional container — daemon does hot-plug + RunContainer
            let client = DaemonClient::new(&vm_state.daemon_socket);
            let t_acquire = std::time::Instant::now();
            let resp = client
                .add_container(
                    &vm_state.daemon_vm_id,
                    &erofs_path.to_string_lossy(),
                    container_id,
                    &config_json,
                )
                .ctx("daemon add_container")?;
            let acq_ms = t_acquire.elapsed().as_millis();
            (vm_state.clone(), false, acq_ms, resp)
        } else {
            // First container — acquire VM from daemon
            let t_acquire = std::time::Instant::now();
            let client = DaemonClient::new(&vm_state.daemon_socket);
            let acquired = client
                .acquire_sandbox(
                    vm_state.tap_name.as_deref().unwrap_or(""),
                    vm_state.tap_mac.as_deref().unwrap_or(""),
                    vm_state.ip_cidr.as_deref().unwrap_or(""),
                    vm_state.gateway.as_deref().unwrap_or(""),
                    &image_key,
                    &erofs_path.to_string_lossy(),
                    container_id,
                    &config_json,
                )
                .ctx("daemon acquire")?;
            let acq_ms = t_acquire.elapsed().as_millis();

            info!(
                "daemon acquired: vm_id={} ch_pid={} from_snapshot={} container_pid={} in {}ms",
                acquired.vm_id,
                acquired.ch_pid,
                acquired.from_snapshot,
                acquired.container_pid,
                acq_ms
            );

            // Update shared state with daemon's VM info
            let new_state = Arc::new(SharedVmState {
                agent: OnceCell::new(),
                vsock_socket: acquired.vsock_socket.clone(),
                container_count: AtomicUsize::new(vm_state.container_count.load(Ordering::SeqCst)),
                tap_name: vm_state.tap_name.clone(),
                tap_mac: vm_state.tap_mac.clone(),
                ip_cidr: vm_state.ip_cidr.clone(),
                gateway: vm_state.gateway.clone(),
                netns: vm_state.netns.clone(),
                cgroups_path: vm_state.cgroups_path.clone(),
                daemon_vm_id: acquired.vm_id.clone(),
                daemon_socket: vm_state.daemon_socket.clone(),
            });

            VMS.write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(self.sandbox_id.clone(), new_state.clone());

            // Place CH in pod cgroup
            if let Some(ref cg) = new_state.cgroups_path {
                if let Err(e) = place_in_pod_cgroup(acquired.ch_pid, cg) {
                    info!("cgroup placement failed (non-fatal): {e}");
                }
            }

            // Configure network on warm restore
            if acquired.from_snapshot {
                let agent = get_or_connect_agent(&new_state).await?;
                if let (Some(ref ip_cidr), Some(ref gw)) = (&new_state.ip_cidr, &new_state.gateway)
                {
                    let parts: Vec<&str> = ip_cidr.split('/').collect();
                    let ip = parts[0];
                    let prefix: u32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
                    let mut req = cloudhv_proto::ConfigureNetworkRequest::new();
                    req.ip_address = ip.to_string();
                    req.gateway = gw.clone();
                    req.prefix_len = prefix;
                    req.device = "eth0".to_string();
                    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
                    agent
                        .configure_network(ctx, &req)
                        .await
                        .ctx("configure network after warm restore")?;
                    info!("guest network configured: ip={ip}/{prefix} gw={gw}");
                }
            }

            (
                new_state,
                acquired.from_snapshot,
                acq_ms,
                acquired.container_pid,
            )
        };

        // Set up exit watcher
        if !from_snapshot {
            let agent = get_or_connect_agent(&active_state).await?;
            let exit = self.exit.clone();
            let cid = container_id.to_string();
            tokio::spawn(async move {
                let mut wait_req = cloudhv_proto::WaitContainerRequest::new();
                wait_req.container_id = cid.clone();
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(86400));
                let exit_code = match agent.wait_container(ctx, &wait_req).await {
                    Ok(resp) => resp.exit_status,
                    Err(_) => 137,
                };
                let _ = exit.set((exit_code, Utc::now()));
            });
        }

        active_state.container_count.fetch_add(1, Ordering::SeqCst);

        info!(
            "TIMING start_container {} @{}: erofs={}ms acquire={}ms total={}ms from_snapshot={} additional={}",
            container_id,
            epoch_ms(),
            erofs_ms,
            acquire_ms,
            t_total.elapsed().as_millis(),
            from_snapshot,
            already_acquired
        );

        Ok(container_pid)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct SandboxSpec {
    netns: Option<String>,
    cgroups_path: Option<String>,
}

fn parse_sandbox_spec(spec_path: &std::path::Path) -> SandboxSpec {
    let spec_str = std::fs::read_to_string(spec_path).unwrap_or_default();
    let spec: serde_json::Value = serde_json::from_str(&spec_str).unwrap_or_default();

    let netns = spec
        .pointer("/linux/namespaces")
        .and_then(|ns| ns.as_array())
        .and_then(|ns| {
            ns.iter().find_map(|n| {
                if n.get("type").and_then(|t| t.as_str()) == Some("network") {
                    n.get("path").and_then(|p| p.as_str()).map(String::from)
                } else {
                    None
                }
            })
        });

    let cgroups_path = spec
        .pointer("/linux/cgroupsPath")
        .and_then(|v| v.as_str())
        .map(String::from);

    SandboxSpec {
        netns,
        cgroups_path,
    }
}

/// Extract a stable image key by resolving the CRI image-name annotation
/// to a content-addressed digest via containerd's image store.
///
/// Falls back to hashing the image tag if digest resolution fails, and
/// to the rootfs path if no image annotation is present.
async fn image_key_from_spec(bundle: &std::path::Path) -> Option<String> {
    let spec_str = std::fs::read_to_string(bundle.join("config.json")).ok()?;
    let spec: serde_json::Value = serde_json::from_str(&spec_str).ok()?;
    let image_name = spec
        .pointer("/annotations/io.kubernetes.cri.image-name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;

    // Resolve tag → digest via containerd's gRPC image service.
    // This is content-addressed and immune to tag mutation (e.g. "latest").
    if let Some(digest) = resolve_image_digest(image_name).await {
        info!("resolved image {} → {}", image_name, digest);
        return Some(stable_hash_hex(&digest));
    }

    // Fallback: hash the image reference (tag-based, not ideal but functional)
    info!(
        "digest resolution failed for {}, using tag hash",
        image_name
    );
    Some(stable_hash_hex(image_name))
}

/// Query containerd's gRPC image service to resolve an image reference
/// to its content digest. Returns the digest (e.g. "sha256:b3255e7d...").
async fn resolve_image_digest(image_ref: &str) -> Option<String> {
    use containerd_client::services::v1::images_client::ImagesClient;
    use containerd_client::services::v1::GetImageRequest;
    use containerd_client::tonic::Request;
    use containerd_client::with_namespace;

    let channel = containerd_client::connect("/run/containerd/containerd.sock")
        .await
        .ok()?;

    let req = GetImageRequest {
        name: image_ref.to_string(),
    };
    let req = with_namespace!(req, "k8s.io");

    let image = ImagesClient::new(channel)
        .get(req)
        .await
        .ok()?
        .into_inner()
        .image?;

    image.target.map(|t| t.digest)
}

/// Prepare erofs image from rootfs (cached by image key).
fn prepare_erofs(rootfs_path: &std::path::Path, cache_key: &str) -> Result<PathBuf, Error> {
    let cache_path = PathBuf::from(EROFS_CACHE_DIR).join(format!("{cache_key}.erofs"));

    if cache_path.exists() {
        return Ok(cache_path);
    }

    std::fs::create_dir_all(EROFS_CACHE_DIR)
        .map_err(|e| Error::Any(anyhow::anyhow!("create erofs cache dir: {e}")))?;

    let tmp_path =
        PathBuf::from(EROFS_CACHE_DIR).join(format!("{cache_key}.{}.tmp", std::process::id()));

    let status = std::process::Command::new("mkfs.erofs")
        .arg(&tmp_path)
        .arg(rootfs_path)
        .output()
        .map_err(|e| Error::Any(anyhow::anyhow!("mkfs.erofs: {e}")))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(Error::Any(anyhow::anyhow!("mkfs.erofs failed: {stderr}")));
    }

    std::fs::rename(&tmp_path, &cache_path)
        .map_err(|e| Error::Any(anyhow::anyhow!("rename erofs: {e}")))?;

    Ok(cache_path)
}

/// Stable FNV-1a 128-bit hash → hex string.
pub fn stable_hash_hex(input: &str) -> String {
    let mut hash: u128 = 0x6c62272e07bb0142_62b821756295c58d;
    for byte in input.bytes() {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(0x0000000001000000_000000000000013b);
    }
    format!("{hash:032x}")
}

/// Place a process in a pod cgroup.
fn place_in_pod_cgroup(pid: u32, cgroups_path: &str) -> Result<(), String> {
    let cgroup_base = "/sys/fs/cgroup";
    let full_path = format!("{cgroup_base}/{cgroups_path}/cgroup.procs");
    if std::path::Path::new(&full_path).exists() {
        std::fs::write(&full_path, pid.to_string()).map_err(|e| format!("{e}"))?;
        return Ok(());
    }

    // Try systemd-style path
    let parts: Vec<&str> = cgroups_path.split(':').collect();
    if parts.len() == 3 {
        let path = format!("{cgroup_base}/{}/{}/cgroup.procs", parts[1], parts[2]);
        if std::path::Path::new(&path).exists() {
            std::fs::write(&path, pid.to_string()).map_err(|e| format!("{e}"))?;
            return Ok(());
        }
    }

    Err(format!(
        "no cgroup found for path {cgroups_path} (tried v2, v2-systemd)"
    ))
}
