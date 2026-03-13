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
use crate::vm::VmManager;

/// Extension trait to simplify error conversion to shimkit Error.
trait ResultExt<T> {
    fn ctx(self, msg: &str) -> Result<T, Error>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for Result<T, E> {
    fn ctx(self, msg: &str) -> Result<T, Error> {
        self.map_err(|e| Error::Any(anyhow::anyhow!("{msg}: {e}")))
    }
}

const CRI_CONTAINER_TYPE: &str = "/annotations/io.kubernetes.cri.container-type";
const CRI_SANDBOX_ID: &str = "/annotations/io.kubernetes.cri.sandbox-id";

// ---------------------------------------------------------------------------
// Static shared state
// ---------------------------------------------------------------------------

/// Active VMs keyed by sandbox ID. Shimkit creates one Instance per
/// container, but sandbox and app containers in the same pod share a VM.
static VMS: LazyLock<RwLock<HashMap<String, Arc<SharedVmState>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Look up a VM by sandbox ID (takes a brief read-lock on VMS).
fn get_vm(sandbox_id: &str) -> Option<Arc<SharedVmState>> {
    VMS.read()
        .unwrap_or_else(|e| e.into_inner())
        .get(sandbox_id)
        .cloned()
}

/// Shared state for a running VM (one per pod).
struct SharedVmState {
    vm: VmManager,
    agent: OnceCell<cloudhv_proto::AgentServiceClient>,
    vsock_socket: PathBuf,
    shared_dir: PathBuf,
    container_count: AtomicUsize,
    api_socket: PathBuf,
}

/// Lazily connect to the guest agent over vsock, caching the client in the
/// `OnceCell`. The first caller pays the connection cost; subsequent callers
/// get the cached client.
async fn get_or_connect_agent(
    vm_state: &SharedVmState,
) -> Result<cloudhv_proto::AgentServiceClient, Error> {
    vm_state
        .agent
        .get_or_try_init(|| async {
            let vsock_client = crate::vsock::VsockClient::new(&vm_state.vsock_socket);
            // Poll for agent readiness
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if vsock_client.health_check().await.unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let (agent, _health) = vsock_client
                .connect_ttrpc()
                .await
                .map_err(|e| Error::Any(anyhow::anyhow!("agent connect: {e}")))?;
            Ok(agent)
        })
        .await
        .cloned()
}

// ---------------------------------------------------------------------------
// CloudHvInstance — implements shimkit's Instance trait
// ---------------------------------------------------------------------------

pub struct CloudHvInstance {
    id: String,
    bundle: PathBuf,
    exit: WaitableCell<(u32, DateTime<Utc>)>,
    is_sandbox: bool,
    sandbox_id: String,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Instance for CloudHvInstance {
    async fn new(id: String, cfg: &InstanceConfig) -> Result<Self, Error> {
        info!("CloudHvInstance::new id={}", id);

        let spec_path = cfg.bundle.join("config.json");
        let (is_sandbox, sandbox_id) = parse_container_type(&spec_path, &id);

        Ok(Self {
            id,
            bundle: cfg.bundle.clone(),
            exit: WaitableCell::new(),
            is_sandbox,
            sandbox_id,
            stdout: cfg.stdout.clone(),
            stderr: cfg.stderr.clone(),
        })
    }

    async fn start(&self) -> Result<u32, Error> {
        info!(
            "CloudHvInstance::start id={} sandbox={}",
            self.id, self.is_sandbox
        );

        if self.is_sandbox {
            self.start_sandbox().await
        } else {
            self.start_container().await
        }
    }

    async fn kill(&self, signal: u32) -> Result<(), Error> {
        info!("CloudHvInstance::kill id={} signal={}", self.id, signal);

        // Best-effort agent RPC — fire and forget (skip if agent never connected)
        if let Some(vm_state) = get_vm(&self.sandbox_id) {
            if let Some(agent) = vm_state.agent.get() {
                let cid = self.id.clone();
                let agent = agent.clone();
                tokio::spawn(async move {
                    let mut kreq = cloudhv_proto::KillContainerRequest::new();
                    kreq.container_id = cid;
                    kreq.signal = signal;
                    kreq.all = true;
                    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
                    let _ = agent.kill_container(ctx, &kreq).await;
                });
            }
        }

        let _ = self.exit.set((137, Utc::now()));
        Ok(())
    }

    async fn delete(&self) -> Result<(), Error> {
        info!("CloudHvInstance::delete id={}", self.id);

        let vm_state = get_vm(&self.sandbox_id);

        if let Some(vm_state) = vm_state {
            // Best-effort delete RPC (skip if agent never connected)
            if let Some(agent) = vm_state.agent.get() {
                let cid = self.id.clone();
                let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
                del_req.container_id = cid;
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(10));
                let _ = agent.delete_container(ctx, &del_req).await;
            }

            // Clean up disk image
            if !self.is_sandbox {
                let state_dir = vm_state.shared_dir.parent().unwrap_or(&vm_state.shared_dir);
                let disk_id = format!("ctr-{}", &self.id[..12.min(self.id.len())]);
                let disk_img = state_dir.join(format!("{disk_id}.img"));
                match std::fs::remove_file(&disk_img) {
                    Ok(()) => info!("removed disk image: {}", disk_img.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => info!("failed to remove disk image: {e}"),
                }
            }

            // Decrement container count; clean up VM if zero
            let prev = vm_state.container_count.fetch_sub(1, Ordering::SeqCst);
            if prev <= 1 {
                let removed = VMS
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.sandbox_id);
                if let Some(removed_state) = removed {
                    // Take ownership of VmManager for cleanup
                    match Arc::try_unwrap(removed_state) {
                        Ok(state) => {
                            let mut vm = state.vm;
                            let _ = vm.cleanup().await;
                            info!("VM cleaned up for sandbox {}", self.sandbox_id);
                        }
                        Err(arc) => {
                            // Other references exist — just log, VM will be cleaned up on drop
                            info!("VM {} still referenced, skipping cleanup", self.sandbox_id);
                            drop(arc);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn wait(&self) -> (u32, DateTime<Utc>) {
        info!("CloudHvInstance::wait id={}", self.id);
        let result = *self.exit.wait().await;
        info!(
            "CloudHvInstance::wait done id={} code={}",
            self.id, result.0
        );
        result
    }
}

// ---------------------------------------------------------------------------
// Helper methods
// ---------------------------------------------------------------------------

impl CloudHvInstance {
    /// Boot a VM for the sandbox container.
    async fn start_sandbox(&self) -> Result<u32, Error> {
        let sandbox_id = self.id.clone();

        let spec_path = self.bundle.join("config.json");
        let (netns_path, pod_annotations, mem_request, mem_limit) = parse_sandbox_spec(&spec_path);

        // Set up TAP device in the pod's network namespace
        let (tap_name, tap_mac, ip_config) = if let Some(ref netns) = netns_path {
            match setup_tap_in_netns(netns, &sandbox_id) {
                Ok(tap_info) => {
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
                Err(e) => {
                    info!("TAP setup failed (proceeding without network): {e}");
                    (None, None, None)
                }
            }
        } else {
            info!("no network namespace — VM will boot without networking");
            (None, None, None)
        };

        let config = load_config(None).ctx("config error")?;
        let config = crate::annotations::apply_annotations(config, &pod_annotations);
        let config = crate::annotations::apply_resource_limits(config, mem_request, mem_limit);

        // Cold-boot a new VM
        let mut vm = VmManager::new(sandbox_id.clone(), config.clone()).ctx("VmManager")?;

        vm.prepare().await.ctx("prepare")?;

        if let Some((ref ip_cidr, ref gw)) = ip_config {
            let parts: Vec<&str> = ip_cidr.split('/').collect();
            let ip = parts[0];
            let prefix: u32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
            let mask = prefix_to_netmask(prefix);
            let ip_param = format!(" ip={ip}::{gw}:{mask}::eth0:off");
            vm.append_kernel_args(&ip_param);
            info!("kernel network: {}", ip_param.trim());
        }

        vm.start_swtpm().await.ctx("swtpm")?;

        vm.spawn_vmm_in_netns(netns_path.as_deref()).ctx("vmm")?;

        vm.wait_vmm_ready().await.ctx("vmm")?;

        vm.create_and_boot_vm(tap_name.as_deref(), tap_mac.as_deref())
            .await
            .ctx("boot")?;

        // Agent connection is deferred until the first container needs it
        // (see get_or_connect_agent). This removes wait_for_agent and
        // connect_ttrpc from the critical sandbox-start path.

        let vsock_socket = vm.vsock_socket().to_path_buf();
        let shared_dir = vm.shared_dir().to_path_buf();
        let api_socket = vm.api_socket_path().to_path_buf();
        let ch_pid = vm.ch_pid().unwrap_or(std::process::id());

        // Start memory monitor if hotplug is configured
        if config.hotplug_memory_mb > 0 {
            let boot_bytes = config.default_memory_mb * 1024 * 1024;
            let max_bytes = boot_bytes + config.hotplug_memory_mb * 1024 * 1024;
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let monitor_config = crate::memory::MemoryMonitorConfig {
                boot_memory_bytes: boot_bytes,
                max_memory_bytes: max_bytes,
                api_socket: api_socket.clone(),
                vsock_socket: vsock_socket.clone(),
                shared_dir: shared_dir.clone(),
            };
            let _monitor = crate::memory::spawn_memory_monitor(monitor_config, shutdown_rx);
            info!(
                "memory monitor started: boot={}MiB max={}MiB",
                config.default_memory_mb,
                config.default_memory_mb + config.hotplug_memory_mb
            );
            std::mem::forget(shutdown_tx);
        }

        let vm_state = Arc::new(SharedVmState {
            vm,
            agent: OnceCell::new(),
            vsock_socket,
            shared_dir,
            container_count: AtomicUsize::new(1), // sandbox itself counts
            api_socket,
        });

        VMS.write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sandbox_id.clone(), vm_state.clone());

        info!("sandbox VM {} ready (ch_pid={})", sandbox_id, ch_pid);
        Ok(ch_pid)
    }

    /// Create and start an application container inside an existing sandbox VM.
    async fn start_container(&self) -> Result<u32, Error> {
        let container_id = &self.id;
        info!("starting app container: {}", container_id);

        let vm_state = get_vm(&self.sandbox_id).ok_or_else(|| {
            Error::Any(anyhow::anyhow!("sandbox VM not found: {}", self.sandbox_id))
        })?;

        let agent = get_or_connect_agent(&vm_state).await?;
        let shared_dir = &vm_state.shared_dir;

        let rootfs_path = self.bundle.join("rootfs");
        let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
        let parent_dir = shared_dir.parent().unwrap_or(shared_dir);

        // Extract volumes from OCI spec
        let spec_path = self.bundle.join("config.json");
        let volumes = extract_volumes(&spec_path).ctx("extract volumes")?;

        // Rootfs image cache: the rootfs ext4 image is expensive to create
        // (mkfs.ext4 on full rootfs). We cache it per unique rootfs content
        // and clone it for each container — zero mkfs.ext4 on the hot path.
        // Config.json and volume data are sent inline via the RPC and the
        // agent writes them to tmpfs inside the VM.
        let cache_dir = std::path::PathBuf::from("/opt/cloudhv/cache");
        let rootfs_clone = rootfs_path.clone();

        let cache_key = tokio::task::spawn_blocking(move || compute_rootfs_hash(&rootfs_clone))
            .await
            .map_err(|_| Error::Any(anyhow::anyhow!("hash task panicked")))?
            .ctx("compute rootfs hash")?;

        let cached_img = cache_dir.join(format!("{cache_key}.img"));
        let rootfs_disk_path = parent_dir.join(format!("{disk_id}.img"));

        // Ensure rootfs base image is cached (one-time cost per container image).
        // Multiple shim processes may race here — use a lock file to serialize
        // cache creation for the same hash.
        if !cached_img.exists() {
            info!("rootfs cache miss for {cache_key}, creating base image");
            let rootfs_for_cache = rootfs_path.clone();
            let cached_img_clone = cached_img.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                // Ensure cache directory exists before creating lock file
                if let Some(parent) = cached_img_clone.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                let lock_path = cached_img_clone.with_extension("lock");
                let lock_file = std::fs::File::create(&lock_path)
                    .map_err(|e| anyhow::anyhow!("create cache lock: {e}"))?;

                // Exclusive flock — blocks until we own the lock
                let fd = std::os::unix::io::AsRawFd::as_raw_fd(&lock_file);
                let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
                if ret != 0 {
                    return Err(anyhow::anyhow!(
                        "flock cache lock: {}",
                        std::io::Error::last_os_error()
                    ));
                }

                // Re-check after acquiring lock (another process may have created it)
                if cached_img_clone.exists() {
                    log::info!(
                        "cache populated by another process: {}",
                        cached_img_clone.display()
                    );
                    return Ok(());
                }

                // Write to a temp file then atomic-rename
                let tmp_path = cached_img_clone.with_extension("img.tmp");
                create_cached_rootfs_image(&rootfs_for_cache, &tmp_path)?;

                // fsync the temp file to ensure it's fully flushed to disk
                let f = std::fs::File::open(&tmp_path)?;
                f.sync_all()?;
                drop(f);

                std::fs::rename(&tmp_path, &cached_img_clone).map_err(|e| {
                    std::fs::remove_file(&tmp_path).ok();
                    anyhow::anyhow!("cache rename failed: {e}")
                })?;

                // Flush lock file (drop releases the flock)
                drop(lock_file);
                std::fs::remove_file(&lock_path).ok();
                Ok(())
            })
            .await
            .map_err(|_| Error::Any(anyhow::anyhow!("cache build task panicked")))?
            .ctx("build rootfs cache")?;
        } else {
            info!("rootfs cache hit for {cache_key}");
        }

        // Clone cached rootfs image (fast cp, no mkfs.ext4)
        let cached_src = cached_img.clone();
        let rootfs_dst = rootfs_disk_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            std::fs::copy(&cached_src, &rootfs_dst)?;
            Ok(())
        })
        .await
        .map_err(|_| Error::Any(anyhow::anyhow!("rootfs copy panicked")))?
        .ctx("copy cached rootfs")?;

        // Hot-plug the rootfs disk into the VM
        let api_socket = vm_state.api_socket.clone();
        let disk_json = serde_json::json!({
            "path": rootfs_disk_path.to_string_lossy(),
            "readonly": false,
            "id": &disk_id,
        });
        VmManager::api_request_to_socket(
            &api_socket,
            "PUT",
            "/api/v1/vm.add-disk",
            Some(&disk_json.to_string()),
        )
        .await
        .ctx("hot-plug rootfs disk")?;
        info!("rootfs disk hot-plugged: {disk_id}");

        // Hot-plug separate empty disks for emptyDir volumes.
        for vol in &volumes {
            if !vol.is_empty_dir {
                continue;
            }
            let edir_id = format!("edir-{}", &vol.volume_id[..8.min(vol.volume_id.len())]);
            let edir_path = parent_dir.join(format!("{}.img", edir_id));

            let f = std::fs::File::create(&edir_path).ctx("create emptyDir image")?;
            f.set_len(16 * 1024 * 1024)?; // 16MB default
            drop(f);
            let status = std::process::Command::new("mkfs.ext4")
                .args(["-q", "-F", "-O", "^has_journal"])
                .arg(&edir_path)
                .status();
            if status.map(|s| !s.success()).unwrap_or(true) {
                info!("mkfs.ext4 failed for emptyDir {}", vol.destination);
                continue;
            }

            let edir_json = serde_json::json!({
                "path": edir_path.to_string_lossy(),
                "readonly": false,
                "id": edir_id,
            });
            VmManager::api_request_to_socket(
                &api_socket,
                "PUT",
                "/api/v1/vm.add-disk",
                Some(&edir_json.to_string()),
            )
            .await
            .ctx("hot-plug emptyDir disk")?;
            info!("emptyDir hot-plugged: {} for {}", edir_id, vol.destination);
        }

        let bundle_guest = format!("/run/containers/{}", container_id);

        // Read config.json to send inline via the RPC
        let config_json_bytes =
            std::fs::read(&spec_path).ctx("read config.json for inline delivery")?;

        // Read filesystem volume files to send inline
        // (ConfigMaps/Secrets are small — K8s caps at 1MB)
        {
            let mut create_req = cloudhv_proto::CreateContainerRequest::new();
            create_req.container_id = container_id.to_string();
            create_req.bundle_path = bundle_guest.clone();
            create_req.config_json = config_json_bytes;
            for vol in &volumes {
                let mut vm = cloudhv_proto::VolumeMount::new();
                vm.destination = vol.destination.clone();
                if vol.is_block || vol.is_empty_dir {
                    vm.source = vol.source.clone();
                    vm.volume_type = cloudhv_proto::VolumeType::BLOCK.into();
                    vm.fs_type = if vol.is_empty_dir {
                        "ext4".to_string()
                    } else {
                        vol.fs_type.clone()
                    };
                } else {
                    // Filesystem volume: send files inline, agent writes to tmpfs
                    vm.source = format!("{}/volumes/{}", bundle_guest, vol.volume_id);
                    vm.volume_type = cloudhv_proto::VolumeType::FILESYSTEM.into();
                    // Read all files from the volume source directory
                    let src = std::path::Path::new(&vol.source);
                    let entries =
                        read_volume_files(src).ctx("read volume files for inline delivery")?;
                    for (rel_path, content, mode) in entries {
                        let mut inline_file = cloudhv_proto::InlineFile::new();
                        inline_file.path = rel_path;
                        inline_file.content = content;
                        inline_file.mode = mode;
                        vm.files.push(inline_file);
                    }
                }
                vm.readonly = vol.readonly;
                create_req.volumes.push(vm);
            }
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
            agent.create_container(ctx, &create_req).await
        }
        .ctx("CreateContainer RPC error")?;

        // Start the container
        let start_resp = {
            let mut start_req = cloudhv_proto::StartContainerRequest::new();
            start_req.container_id = container_id.to_string();
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
            agent.start_container(ctx, &start_req).await
        }
        .ctx("StartContainer RPC error")?;

        vm_state.container_count.fetch_add(1, Ordering::SeqCst);

        // Stream container logs from agent via GetContainerLogs RPC.
        // Open FIFOs synchronously so containerd sees an active writer.
        let log_agent = agent.clone();
        let log_cid = container_id.to_string();
        let stdout_fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.stdout)
            .ok();
        let stderr_fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.stderr)
            .ok();
        let log_handle = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stdout_writer = stdout_fifo.map(tokio::fs::File::from_std);
            let mut stderr_writer = stderr_fifo.map(tokio::fs::File::from_std);
            let mut offset = 0u64;
            loop {
                let mut req = cloudhv_proto::GetContainerLogsRequest::new();
                req.container_id = log_cid.clone();
                req.offset = offset;
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
                match log_agent.get_container_logs(ctx, &req).await {
                    Ok(resp) => {
                        if let Some(ref mut w) = stdout_writer {
                            if !resp.stdout.is_empty() {
                                let _ = w.write_all(&resp.stdout).await;
                            }
                        }
                        if let Some(ref mut w) = stderr_writer {
                            if !resp.stderr.is_empty() {
                                let _ = w.write_all(&resp.stderr).await;
                            }
                        }
                        offset = resp.offset;
                        if resp.eof {
                            break;
                        }
                    }
                    Err(_) => break,
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        // Watch for container exit via WaitContainer RPC
        let exit = self.exit.clone();
        let cid = container_id.to_string();
        let agent_clone = agent.clone();
        tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let mut wait_req = cloudhv_proto::WaitContainerRequest::new();
            wait_req.container_id = cid.clone();
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(86400));
            let exit_code = match agent_clone.wait_container(ctx, &wait_req).await {
                Ok(resp) => resp.exit_status,
                Err(e) => {
                    log::info!("wait_container RPC error for {}: {e}", cid);
                    137
                }
            };
            let rpc_ms = t0.elapsed().as_millis();
            let t1 = std::time::Instant::now();
            let log_result =
                tokio::time::timeout(std::time::Duration::from_millis(100), log_handle).await;
            let log_ms = t1.elapsed().as_millis();
            let t2 = std::time::Instant::now();
            let _ = exit.set((exit_code, Utc::now()));
            let set_ms = t2.elapsed().as_millis();
            log::info!(
                "container {} exit path: rpc={}ms log_wait={}ms({}), set={}ms, total={}ms",
                cid,
                rpc_ms,
                log_ms,
                if log_result.is_ok() {
                    "completed"
                } else {
                    "timeout"
                },
                set_ms,
                t0.elapsed().as_millis()
            );
        });

        let pid = start_resp.pid;
        info!("started container {} pid={}", container_id, pid);
        Ok(pid)
    }
}

// ---------------------------------------------------------------------------
// Spec parsing helpers
// ---------------------------------------------------------------------------

/// Detect whether a container is sandbox or app, and extract sandbox-id.
fn parse_container_type(spec_path: &std::path::Path, default_id: &str) -> (bool, String) {
    let spec: serde_json::Value = std::fs::read_to_string(spec_path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();

    let is_sandbox = spec.pointer(CRI_CONTAINER_TYPE).and_then(|v| v.as_str()) == Some("sandbox");

    let sandbox_id = spec
        .pointer(CRI_SANDBOX_ID)
        .and_then(|v| v.as_str())
        .unwrap_or(default_id)
        .to_string();

    (is_sandbox, sandbox_id)
}

/// Parse sandbox OCI spec for network namespace, annotations, and resources.
fn parse_sandbox_spec(
    spec_path: &std::path::Path,
) -> (
    Option<String>,
    HashMap<String, String>,
    Option<u64>,
    Option<u64>,
) {
    let data = match std::fs::read_to_string(spec_path) {
        Ok(d) => d,
        Err(_) => return (None, HashMap::new(), None, None),
    };
    let spec: serde_json::Value = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(_) => return (None, HashMap::new(), None, None),
    };

    let netns = spec
        .pointer("/linux/namespaces")
        .and_then(|v| v.as_array())
        .and_then(|ns| {
            ns.iter()
                .find(|n| n.get("type").and_then(|t| t.as_str()) == Some("network"))
        })
        .and_then(|n| n.get("path").and_then(|p| p.as_str()))
        .map(String::from);

    let annotations = crate::annotations::annotations_from_spec(&spec);
    let (req, lim) = crate::annotations::memory_resources_from_spec(&spec);

    if netns.is_some() {
        info!("sandbox netns: {:?}", netns);
    }
    if !annotations.is_empty() {
        info!("sandbox annotations: {:?}", annotations);
    }
    if req.is_some() || lim.is_some() {
        info!("sandbox resources: request={:?}MiB limit={:?}MiB", req, lim);
    }

    (netns, annotations, req, lim)
}

/// Volume extracted from the OCI spec's mounts array.
#[derive(Clone, Debug)]
struct VolumeSpec {
    destination: String,
    source: String,
    readonly: bool,
    is_block: bool,
    is_empty_dir: bool,
    fs_type: String,
    volume_id: String,
}

/// System mount destinations that should NOT be treated as volumes.
const SYSTEM_MOUNTS: &[&str] = &[
    "/proc",
    "/dev",
    "/dev/pts",
    "/dev/mqueue",
    "/dev/shm",
    "/sys",
    "/sys/fs/cgroup",
];

/// Extract volume mounts from the OCI spec. Returns an error if the spec
/// cannot be read or parsed, since missing volumes would cause silent failures.
fn extract_volumes(spec_path: &std::path::Path) -> anyhow::Result<Vec<VolumeSpec>> {
    use anyhow::Context;

    let data = std::fs::read_to_string(spec_path)
        .with_context(|| format!("read OCI spec for volumes: {}", spec_path.display()))?;
    let spec: serde_json::Value = serde_json::from_str(&data)
        .with_context(|| format!("parse OCI spec: {}", spec_path.display()))?;

    let mounts = match spec.pointer("/mounts").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return Ok(vec![]),
    };

    let mut volumes = Vec::new();
    for mount in mounts {
        let dest = mount
            .get("destination")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let source = mount.get("source").and_then(|s| s.as_str()).unwrap_or("");
        let mount_type = mount.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let options = mount
            .get("options")
            .and_then(|o| o.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        // Skip system mounts
        if SYSTEM_MOUNTS.contains(&dest) {
            continue;
        }
        // Skip non-bind mounts (proc, tmpfs, sysfs, devpts, etc.)
        if mount_type != "bind" {
            continue;
        }
        // Skip empty source
        if source.is_empty() {
            continue;
        }

        let readonly = options.contains(&"ro");
        let is_block = std::path::Path::new(source)
            .metadata()
            .map(|m| {
                use std::os::unix::fs::FileTypeExt;
                m.file_type().is_block_device()
            })
            .unwrap_or(false);

        // Detect emptyDir: writable directory, typically under
        // /var/lib/kubelet/pods/<uid>/volumes/kubernetes.io~empty-dir/
        let is_empty_dir = !readonly && !is_block && source.contains("empty-dir");

        // Generate a short stable ID for the volume (hash of destination)
        let volume_id = format!("{:x}", {
            let mut h: u64 = 5381;
            for b in dest.bytes() {
                h = h.wrapping_mul(33).wrapping_add(b as u64);
            }
            h
        });

        // Fix #9: don't default to ext4 — let agent auto-detect
        let fs_type = if is_block {
            detect_block_fs_type(source).unwrap_or_default()
        } else {
            String::new()
        };

        volumes.push(VolumeSpec {
            destination: dest.to_string(),
            source: source.to_string(),
            readonly,
            is_block,
            is_empty_dir,
            fs_type,
            volume_id,
        });
    }

    Ok(volumes)
}

/// Detect filesystem type of a block device via blkid.
/// Returns None when detection fails — the agent will try auto-detect.
fn detect_block_fs_type(device: &str) -> Option<String> {
    let output = std::process::Command::new("blkid")
        .args(["-o", "value", "-s", "TYPE", device])
        .output()
        .ok()?;
    let fs = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if fs.is_empty() {
        None
    } else {
        Some(fs)
    }
}

/// Create an ext4 disk image containing the OCI bundle (config.json + rootfs).
///
/// Uses `mkfs.ext4 -d` to populate the image directly from a staging directory,
/// avoiding loopback mount/umount and kernel VFS lock contention.
///
/// NOTE: Retained for unit tests only. Production code uses
/// [`create_cached_rootfs_image`] + inline RPC for metadata.
#[cfg(test)]
fn create_rootfs_disk_image(
    bundle_path: &str,
    rootfs_path: &std::path::Path,
    disk_path: &std::path::Path,
    volumes: &[VolumeSpec],
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    // Calculate total size: rootfs + read-only volume data (emptyDir excluded)
    let mut total_size = dir_size(rootfs_path)?;
    for vol in volumes {
        if !vol.is_block && !vol.is_empty_dir {
            total_size += dir_size(std::path::Path::new(&vol.source)).context("volume dir_size")?;
        }
    }
    let image_size_mb = std::cmp::max(64, (total_size * 3 / 2 / 1024 / 1024 + 16) as u64);

    // Create sparse file
    let f = std::fs::File::create(disk_path)
        .with_context(|| format!("create disk image: {}", disk_path.display()))?;
    f.set_len(image_size_mb * 1024 * 1024)?;
    drop(f);

    // Stage the directory layout for mkfs.ext4 -d
    let staging = disk_path.with_extension("staging");
    std::fs::create_dir_all(staging.join("rootfs"))?;

    let status = Command::new("cp")
        .args(["-a", "--"])
        .arg(format!("{}/.", rootfs_path.display()))
        .arg(staging.join("rootfs"))
        .status()
        .context("cp rootfs to staging dir")?;
    if !status.success() {
        std::fs::remove_dir_all(&staging).ok();
        anyhow::bail!("cp rootfs failed: {status}");
    }

    let config_src = std::path::Path::new(bundle_path).join("config.json");
    if config_src.exists() {
        std::fs::copy(&config_src, staging.join("config.json"))?;
    }

    // Stage read-only filesystem volumes (ConfigMaps, Secrets) into volumes/<id>/.
    // emptyDir and block volumes are NOT baked in — they get separate disks.
    for vol in volumes {
        if vol.is_block || vol.is_empty_dir {
            continue;
        }
        let vol_staging = staging.join("volumes").join(&vol.volume_id);
        std::fs::create_dir_all(&vol_staging)?;
        let src = std::path::Path::new(&vol.source);
        if src.is_dir() {
            let status = Command::new("cp")
                .args(["-a", "--"])
                .arg(format!("{}/.", src.display()))
                .arg(&vol_staging)
                .status()
                .context("cp volume data")?;
            if !status.success() {
                std::fs::remove_dir_all(&staging).ok();
                anyhow::bail!("failed to copy volume {} to staging: {status}", vol.source);
            }
        } else if src.is_file() {
            // Copy single file preserving permissions (pure Rust, no shell)
            copy_preserving_metadata(src, &vol_staging.join(src.file_name().unwrap_or_default()))
                .with_context(|| format!("copy volume file: {}", src.display()))?;
        }
        log::info!(
            "staged volume: {} -> volumes/{} (ro={})",
            vol.destination,
            vol.volume_id,
            vol.readonly
        );
    }

    // Create and populate ext4 image in one step (no loopback mount)
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(&staging)
        .arg(disk_path)
        .status()
        .context("mkfs.ext4 -d")?;

    std::fs::remove_dir_all(&staging).ok();

    if !status.success() {
        anyhow::bail!("mkfs.ext4 -d failed: {status}");
    }

    log::info!("disk image created: {}", disk_path.display());
    Ok(())
}

/// Copy a file preserving ownership and permissions (pure Rust, no shell).
#[cfg(test)]
fn copy_preserving_metadata(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::copy(src, dst)?;
    let meta = std::fs::metadata(src)?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode()))?;
    nix::unistd::chown(
        dst,
        Some(nix::unistd::Uid::from_raw(meta.uid())),
        Some(nix::unistd::Gid::from_raw(meta.gid())),
    )?;
    Ok(())
}

/// Calculate total size of a directory tree in bytes.
fn dir_size(path: &std::path::Path) -> anyhow::Result<u64> {
    let mut total = 0u64;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                total += dir_size(&entry.path())?;
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

/// Count total entries (files + directories + symlinks) in a directory tree.
/// Used to estimate inode requirements for ext4 image sizing.
fn dir_entry_count(path: &std::path::Path) -> anyhow::Result<u64> {
    let mut count = 0u64;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            count += 1;
            if entry.path().is_dir() {
                count += dir_entry_count(&entry.path())?;
            }
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Rootfs image cache — cached rootfs + inline metadata
// ---------------------------------------------------------------------------

/// Compute a metadata hash of a rootfs directory for cache keying.
///
/// Hashes the sorted list of (relative_path, size, mode) tuples using FNV-1a.
/// This is safe because containerd snapshot mounts are immutable — the same
/// image always produces the same overlayfs snapshot with identical file
/// metadata. Does not hash file contents for speed.
fn compute_rootfs_hash(rootfs: &std::path::Path) -> anyhow::Result<String> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;

    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut BTreeSet<String>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.path().symlink_metadata()?;
            let rel = entry
                .path()
                .strip_prefix(base)
                .unwrap_or(entry.path().as_path())
                .to_string_lossy()
                .to_string();
            out.insert(format!("{}:{}:{}", rel, meta.len(), meta.mode()));
            if meta.file_type().is_dir() {
                walk(base, &entry.path(), out)?;
            }
        }
        Ok(())
    }

    let mut entries = BTreeSet::new();
    walk(rootfs, rootfs, &mut entries)?;

    // FNV-1a 64-bit hash of the sorted entry list
    let mut h: u64 = 0xcbf29ce484222325;
    for entry in &entries {
        for b in entry.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("{:016x}", h))
}

/// Create a cached rootfs-only ext4 image (one-time cost per container image).
///
/// The resulting image contains only `rootfs/` — no config.json or volumes.
/// It is stored at `/opt/cloudhv/cache/<hash>.img` and cloned (cp) for each
/// container, eliminating mkfs.ext4 from the hot path.
fn create_cached_rootfs_image(
    rootfs_path: &std::path::Path,
    cache_path: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let total_size = dir_size(rootfs_path)?;
    let inode_count = dir_entry_count(rootfs_path)?;
    // Size: fit rootfs with 50% headroom. ext4 needs ~1 inode per entry plus
    // overhead. Minimum 16MB to ensure enough inodes for many-file images.
    // At default 1 inode per 16KiB, a 16MB image provides ~1024 inodes.
    let size_for_bytes = total_size * 3 / 2 / 1024 / 1024 + 4;
    let size_for_inodes = (inode_count * 16 / 1024).max(1); // 16KiB per inode
    let image_size_mb = std::cmp::max(16, std::cmp::max(size_for_bytes, size_for_inodes));

    let f = std::fs::File::create(cache_path)
        .with_context(|| format!("create cached image: {}", cache_path.display()))?;
    f.set_len(image_size_mb * 1024 * 1024)?;
    drop(f);

    let staging = cache_path.with_extension("staging");
    std::fs::create_dir_all(staging.join("rootfs"))?;

    let status = Command::new("cp")
        .args(["-a", "--"])
        .arg(format!("{}/.", rootfs_path.display()))
        .arg(staging.join("rootfs"))
        .status()
        .context("cp rootfs to cache staging")?;
    if !status.success() {
        std::fs::remove_dir_all(&staging).ok();
        anyhow::bail!("cp rootfs to cache staging failed: {status}");
    }

    // Create ext4 without journal — these are ephemeral container disks that
    // don't need crash consistency. Saves ~4MB per image and eliminates
    // journal write overhead on every file operation inside the guest.
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-O", "^has_journal", "-d"])
        .arg(&staging)
        .arg(cache_path)
        .status()
        .context("mkfs.ext4 -d for cache")?;

    std::fs::remove_dir_all(&staging).ok();

    if !status.success() {
        std::fs::remove_file(cache_path).ok();
        anyhow::bail!("mkfs.ext4 -d for rootfs cache failed: {status}");
    }

    log::info!(
        "rootfs cached: {} ({}MB)",
        cache_path.display(),
        image_size_mb
    );
    Ok(())
}

/// Create a tiny metadata ext4 disk (config.json + volume data).
///
/// NOTE: Retained for unit tests only. Production code uses inline RPC +
/// tmpfs instead of a metadata disk.
#[cfg(test)]
fn create_metadata_disk_image(
    bundle_path: &str,
    disk_path: &std::path::Path,
    volumes: &[VolumeSpec],
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    let staging = disk_path.with_extension("staging");
    std::fs::create_dir_all(&staging)?;

    // Copy config.json
    let config_src = std::path::Path::new(bundle_path).join("config.json");
    if config_src.exists() {
        std::fs::copy(&config_src, staging.join("config.json"))?;
    }

    // Stage read-only filesystem volumes (ConfigMaps, Secrets)
    let mut vol_size: u64 = 4096; // config.json + overhead
    for vol in volumes {
        if vol.is_block || vol.is_empty_dir {
            continue;
        }
        let vol_staging = staging.join("volumes").join(&vol.volume_id);
        std::fs::create_dir_all(&vol_staging)?;
        let src = std::path::Path::new(&vol.source);
        if src.is_dir() {
            let status = Command::new("cp")
                .args(["-a", "--"])
                .arg(format!("{}/.", src.display()))
                .arg(&vol_staging)
                .status()
                .context("cp volume data to metadata staging")?;
            if !status.success() {
                std::fs::remove_dir_all(&staging).ok();
                anyhow::bail!(
                    "failed to copy volume {} to metadata staging: {status}",
                    vol.source
                );
            }
            vol_size += dir_size(src).unwrap_or(0);
        } else if src.is_file() {
            copy_preserving_metadata(src, &vol_staging.join(src.file_name().unwrap_or_default()))
                .with_context(|| format!("copy volume file: {}", src.display()))?;
            vol_size += src.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }

    // Tiny image: just enough for metadata + padding (minimum 2MB for ext4)
    let image_size_mb = std::cmp::max(2, vol_size * 3 / 2 / 1024 / 1024 + 1);

    let f = std::fs::File::create(disk_path)
        .with_context(|| format!("create metadata disk: {}", disk_path.display()))?;
    f.set_len(image_size_mb * 1024 * 1024)?;
    drop(f);

    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(&staging)
        .arg(disk_path)
        .status()
        .context("mkfs.ext4 -d for metadata")?;

    std::fs::remove_dir_all(&staging).ok();

    if !status.success() {
        anyhow::bail!("mkfs.ext4 -d for metadata disk failed: {status}");
    }

    log::info!(
        "metadata disk created: {} ({}MB)",
        disk_path.display(),
        image_size_mb
    );
    Ok(())
}

/// Read all files from a volume source directory for inline RPC delivery.
/// Returns a vec of (relative_path, content, mode) tuples.
fn read_volume_files(src: &std::path::Path) -> anyhow::Result<Vec<(String, Vec<u8>, u32)>> {
    use std::os::unix::fs::PermissionsExt;

    let mut files = Vec::new();

    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, Vec<u8>, u32)>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = path.symlink_metadata()?;
            if meta.file_type().is_dir() {
                walk(base, &path, out)?;
            } else if meta.file_type().is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let content = std::fs::read(&path)?;
                let mode = path.metadata()?.permissions().mode();
                out.push((rel, content, mode));
            }
        }
        Ok(())
    }

    let src_meta = src.symlink_metadata()?;
    if src_meta.file_type().is_dir() {
        walk(src, src, &mut files)?;
    } else if src_meta.file_type().is_file() {
        let name = src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let content = std::fs::read(src)?;
        let mode = src.metadata()?.permissions().mode();
        files.push((name, content, mode));
    }

    Ok(files)
}

/// Information about a TAP device created for VM networking.
struct TapInfo {
    tap_name: String,
    mac: String,
    ip_cidr: String,
    gateway: String,
}

/// Create a TAP device in the pod's network namespace and set up
///
/// This implements the same pattern as firecracker-containerd's
/// tc-redirect-tap CNI plugin:
/// 1. Enter the network namespace
/// 2. Find the veth device (created by CNI)
/// 3. Create a TAP device
/// 4. Set up TC u32 filters to redirect veth ↔ TAP
/// 5. Return TAP info for Cloud Hypervisor's virtio-net config
fn setup_tap_in_netns(netns_path: &str, vm_id: &str) -> anyhow::Result<TapInfo> {
    use anyhow::Context;
    use std::process::Command;

    let tap_name = format!("tap_{}", &vm_id[..8.min(vm_id.len())]);
    let netns_arg = format!("--net={netns_path}");

    // Run the setup commands inside the network namespace using nsenter
    // Create TAP device
    let status = Command::new("nsenter")
        .args([
            &netns_arg, "--", "ip", "tuntap", "add", &tap_name, "mode", "tap",
        ])
        .status()
        .context("create TAP")?;
    if !status.success() {
        anyhow::bail!("ip tuntap add failed: {status}");
    }

    // Bring TAP up
    let status = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "link", "set", &tap_name, "up"])
        .status()
        .context("ip link set tap up")?;
    if !status.success() {
        anyhow::bail!("ip link set tap up failed: {status}");
    }

    // Find the veth device and its IP/MAC
    let output = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "-j", "addr", "show"])
        .output()
        .context("ip addr show")?;
    let addrs: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::json!([]));

    let mut veth_name = String::new();
    let mut ip_cidr = String::new();
    let mut mac = String::new();

    if let Some(interfaces) = addrs.as_array() {
        for iface in interfaces {
            let name = iface.get("ifname").and_then(|n| n.as_str()).unwrap_or("");
            // Skip loopback and our TAP
            if name == "lo" || name == tap_name {
                continue;
            }
            if let Some(addr_info) = iface.get("addr_info").and_then(|a| a.as_array()) {
                for addr in addr_info {
                    if addr.get("family").and_then(|f| f.as_str()) == Some("inet") {
                        ip_cidr = format!(
                            "{}/{}",
                            addr.get("local").and_then(|l| l.as_str()).unwrap_or(""),
                            addr.get("prefixlen").and_then(|p| p.as_u64()).unwrap_or(24)
                        );
                        veth_name = name.to_string();
                        mac = iface
                            .get("address")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .to_string();
                        break;
                    }
                }
            }
            if !veth_name.is_empty() {
                break;
            }
        }
    }

    if veth_name.is_empty() || ip_cidr.is_empty() {
        anyhow::bail!("could not find veth with IP in netns {netns_path}");
    }

    // Get default gateway
    let output = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "-j", "route", "show", "default"])
        .output()
        .context("ip route show default")?;
    let routes: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::json!([]));
    let gateway = routes
        .as_array()
        .and_then(|r| r.first())
        .and_then(|r| r.get("gateway"))
        .and_then(|g| g.as_str())
        .unwrap_or("10.88.0.1")
        .to_string();

    // Set up TC redirect: veth ingress → TAP egress, TAP ingress → veth egress
    for cmd in [
        // Add ingress qdisc to veth
        vec!["tc", "qdisc", "add", "dev", &veth_name, "ingress"],
        // Redirect veth ingress → TAP
        vec![
            "tc", "filter", "add", "dev", &veth_name, "parent", "ffff:", "protocol", "all", "u32",
            "match", "u32", "0", "0", "action", "mirred", "egress", "redirect", "dev", &tap_name,
        ],
        // Add ingress qdisc to TAP
        vec!["tc", "qdisc", "add", "dev", &tap_name, "ingress"],
        // Redirect TAP ingress → veth
        vec![
            "tc", "filter", "add", "dev", &tap_name, "parent", "ffff:", "protocol", "all", "u32",
            "match", "u32", "0", "0", "action", "mirred", "egress", "redirect", "dev", &veth_name,
        ],
    ] {
        let mut nsenter_cmd = vec!["nsenter", &netns_arg, "--"];
        nsenter_cmd.extend(cmd.iter().copied());

        let status = Command::new(nsenter_cmd[0])
            .args(&nsenter_cmd[1..])
            .status()
            .with_context(|| format!("tc command: {:?}", cmd))?;
        if !status.success() {
            log::warn!("tc command failed (may be ok): {:?} -> {}", cmd, status);
        }
    }

    // Remove the IP from the netns veth so packets destined for the pod IP
    // are forwarded through TC redirect to the TAP → VM, instead of being
    // handled locally by the netns kernel stack.
    let status = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "addr", "flush", "dev", &veth_name])
        .status()
        .context("flush IP from veth")?;
    if !status.success() {
        log::warn!("failed to flush IP from veth (may cause routing issues)");
    }

    log::info!(
        "TAP {} set up in netns {}: veth={} ip={} gw={} mac={}",
        tap_name,
        netns_path,
        veth_name,
        ip_cidr,
        gateway,
        mac
    );

    Ok(TapInfo {
        tap_name,
        mac,
        ip_cidr,
        gateway,
    })
}

/// Convert a CIDR prefix length to a dotted-decimal netmask.
fn prefix_to_netmask(prefix: u32) -> String {
    let mask: u32 = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    format!(
        "{}.{}.{}.{}",
        (mask >> 24) & 0xff,
        (mask >> 16) & 0xff,
        (mask >> 8) & 0xff,
        mask & 0xff,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: compute the djb2 volume ID hash for a destination path.
    fn volume_id_for(dest: &str) -> String {
        let mut h: u64 = 5381;
        for b in dest.bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u64);
        }
        format!("{:x}", h)
    }

    /// Write an OCI spec JSON to a file with optional mounts.
    fn write_spec(path: &std::path::Path, mounts: &[serde_json::Value]) {
        let spec = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": {
                "terminal": false,
                "user": { "uid": 0, "gid": 0 },
                "args": ["/bin/sh"],
                "env": ["PATH=/bin"],
                "cwd": "/"
            },
            "root": { "path": "rootfs", "readonly": false },
            "linux": { "namespaces": [{"type": "pid"}, {"type": "mount"}] },
            "mounts": mounts,
        });
        fs::write(path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();
    }

    #[test]
    fn extract_volumes_empty_spec() {
        let dir = TempDir::new().unwrap();
        let spec = dir.path().join("config.json");
        write_spec(&spec, &[]);
        let vols = extract_volumes(&spec).unwrap();
        assert!(vols.is_empty());
    }

    #[test]
    fn extract_volumes_skips_system_mounts() {
        let dir = TempDir::new().unwrap();
        let spec = dir.path().join("config.json");
        write_spec(
            &spec,
            &[
                serde_json::json!({"destination": "/proc", "source": "/proc", "type": "bind"}),
                serde_json::json!({"destination": "/dev", "source": "/dev", "type": "bind"}),
                serde_json::json!({"destination": "/sys", "source": "/sys", "type": "bind"}),
                serde_json::json!({"destination": "/dev/pts", "source": "/dev/pts", "type": "bind"}),
                serde_json::json!({"destination": "/dev/mqueue", "source": "/dev/mqueue", "type": "bind"}),
                serde_json::json!({"destination": "/dev/shm", "source": "/dev/shm", "type": "bind"}),
                serde_json::json!({"destination": "/sys/fs/cgroup", "source": "/sys/fs/cgroup", "type": "bind"}),
            ],
        );
        let vols = extract_volumes(&spec).unwrap();
        assert!(vols.is_empty(), "system mounts should be filtered out");
    }

    #[test]
    fn extract_volumes_skips_non_bind_mounts() {
        let dir = TempDir::new().unwrap();
        let spec = dir.path().join("config.json");
        write_spec(
            &spec,
            &[
                serde_json::json!({"destination": "/tmp", "source": "tmpfs", "type": "tmpfs"}),
                serde_json::json!({"destination": "/mnt/proc", "source": "proc", "type": "proc"}),
            ],
        );
        let vols = extract_volumes(&spec).unwrap();
        assert!(vols.is_empty(), "non-bind mounts should be filtered out");
    }

    #[test]
    fn extract_volumes_detects_configmap_readonly() {
        let dir = TempDir::new().unwrap();
        // Create a real source dir so metadata() succeeds (it's not a block device)
        let src = dir.path().join("configmap-data");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("key"), "value").unwrap();

        let spec = dir.path().join("config.json");
        write_spec(
            &spec,
            &[serde_json::json!({
                "destination": "/etc/config",
                "source": src.to_string_lossy(),
                "type": "bind",
                "options": ["rbind", "ro"]
            })],
        );
        let vols = extract_volumes(&spec).unwrap();
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].destination, "/etc/config");
        assert!(vols[0].readonly, "should be readonly");
        assert!(!vols[0].is_block, "directory is not a block device");
        assert!(!vols[0].is_empty_dir, "no 'empty-dir' in path");
        assert_eq!(vols[0].volume_id, volume_id_for("/etc/config"));
    }

    #[test]
    fn extract_volumes_detects_emptydir() {
        let dir = TempDir::new().unwrap();
        // Source path must contain "empty-dir" (kubelet convention)
        let src = dir
            .path()
            .join("pods/uid/volumes/kubernetes.io~empty-dir/scratch");
        fs::create_dir_all(&src).unwrap();

        let spec = dir.path().join("config.json");
        write_spec(
            &spec,
            &[serde_json::json!({
                "destination": "/scratch",
                "source": src.to_string_lossy(),
                "type": "bind",
                "options": []
            })],
        );
        let vols = extract_volumes(&spec).unwrap();
        assert_eq!(vols.len(), 1);
        assert!(vols[0].is_empty_dir, "path contains 'empty-dir'");
        assert!(!vols[0].readonly, "emptyDir is writable");
    }

    #[test]
    fn extract_volumes_readonly_emptydir_not_detected() {
        let dir = TempDir::new().unwrap();
        let src = dir
            .path()
            .join("pods/uid/volumes/kubernetes.io~empty-dir/cache");
        fs::create_dir_all(&src).unwrap();

        let spec = dir.path().join("config.json");
        write_spec(
            &spec,
            &[serde_json::json!({
                "destination": "/cache",
                "source": src.to_string_lossy(),
                "type": "bind",
                "options": ["ro"]
            })],
        );
        let vols = extract_volumes(&spec).unwrap();
        assert_eq!(vols.len(), 1);
        // emptyDir detection requires writable mount
        assert!(
            !vols[0].is_empty_dir,
            "readonly mount should not be emptyDir"
        );
    }

    #[test]
    fn extract_volumes_multiple_volumes() {
        let dir = TempDir::new().unwrap();

        let cm_src = dir.path().join("cm");
        fs::create_dir_all(&cm_src).unwrap();
        fs::write(cm_src.join("app.conf"), "setting=1").unwrap();

        let secret_src = dir.path().join("secret");
        fs::create_dir_all(&secret_src).unwrap();
        fs::write(secret_src.join("password"), "hunter2").unwrap();

        let edir_src = dir
            .path()
            .join("pods/x/volumes/kubernetes.io~empty-dir/tmp");
        fs::create_dir_all(&edir_src).unwrap();

        let spec = dir.path().join("config.json");
        write_spec(
            &spec,
            &[
                serde_json::json!({
                    "destination": "/etc/config",
                    "source": cm_src.to_string_lossy(),
                    "type": "bind",
                    "options": ["ro"]
                }),
                serde_json::json!({
                    "destination": "/etc/secrets",
                    "source": secret_src.to_string_lossy(),
                    "type": "bind",
                    "options": ["ro"]
                }),
                serde_json::json!({
                    "destination": "/tmp",
                    "source": edir_src.to_string_lossy(),
                    "type": "bind",
                    "options": []
                }),
                // System mount — should be filtered
                serde_json::json!({
                    "destination": "/proc",
                    "source": "/proc",
                    "type": "bind"
                }),
            ],
        );
        let vols = extract_volumes(&spec).unwrap();
        assert_eq!(vols.len(), 3, "3 user volumes, /proc filtered");
        assert!(vols[0].readonly);
        assert!(vols[1].readonly);
        assert!(vols[2].is_empty_dir);
    }

    #[test]
    fn extract_volumes_no_mounts_key() {
        let dir = TempDir::new().unwrap();
        let spec = dir.path().join("config.json");
        let data = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": { "args": ["/bin/sh"] },
            "root": { "path": "rootfs" }
        });
        fs::write(&spec, serde_json::to_string(&data).unwrap()).unwrap();
        let vols = extract_volumes(&spec).unwrap();
        assert!(vols.is_empty());
    }

    #[test]
    fn extract_volumes_invalid_json_is_error() {
        let dir = TempDir::new().unwrap();
        let spec = dir.path().join("config.json");
        fs::write(&spec, "not json").unwrap();
        assert!(extract_volumes(&spec).is_err());
    }

    #[test]
    fn extract_volumes_missing_file_is_error() {
        let result = extract_volumes(std::path::Path::new("/nonexistent/config.json"));
        assert!(result.is_err());
    }

    #[test]
    fn copy_preserving_metadata_copies_content_and_permissions() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dst = dir.path().join("dest.txt");
        fs::write(&src, "hello world").unwrap();

        // Set specific permissions
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();

        copy_preserving_metadata(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello world");
        let src_meta = fs::metadata(&src).unwrap();
        let dst_meta = fs::metadata(&dst).unwrap();
        assert_eq!(
            src_meta.permissions().mode() & 0o777,
            dst_meta.permissions().mode() & 0o777
        );
    }

    #[test]
    fn dir_size_counts_nested_files() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("a.txt"), "aaaa").unwrap(); // 4 bytes
        fs::write(sub.join("b.txt"), "bbbbbbbb").unwrap(); // 8 bytes
        let size = super::dir_size(dir.path()).unwrap();
        assert!(size >= 12, "total should be at least 12 bytes, got {size}");
    }

    #[test]
    fn dir_size_empty_dir() {
        let dir = TempDir::new().unwrap();
        let size = super::dir_size(dir.path()).unwrap();
        assert_eq!(size, 0);
    }

    #[test]
    fn volume_id_is_deterministic() {
        let id1 = volume_id_for("/etc/config");
        let id2 = volume_id_for("/etc/config");
        assert_eq!(id1, id2);
        // Different paths produce different IDs
        let id3 = volume_id_for("/etc/secrets");
        assert_ne!(id1, id3);
    }

    #[test]
    fn create_rootfs_disk_image_with_volumes() {
        // Skip if mkfs.ext4 isn't available
        if std::process::Command::new("which")
            .arg("mkfs.ext4")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("SKIPPING: mkfs.ext4 not available");
            return;
        }

        let dir = TempDir::new().unwrap();

        // Create a minimal rootfs
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();

        // Create bundle with config.json
        let bundle = dir.path().join("bundle");
        fs::create_dir_all(&bundle).unwrap();
        let config = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": { "args": ["/bin/hello"] },
            "root": { "path": "rootfs" }
        });
        fs::write(
            bundle.join("config.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        // Create ConfigMap volume data
        let cm_src = dir.path().join("cm-vol");
        fs::create_dir_all(&cm_src).unwrap();
        fs::write(cm_src.join("app.conf"), "key=value\n").unwrap();
        fs::write(cm_src.join("extra.yaml"), "foo: bar\n").unwrap();

        let volumes = vec![
            VolumeSpec {
                destination: "/etc/config".to_string(),
                source: cm_src.to_string_lossy().to_string(),
                readonly: true,
                is_block: false,
                is_empty_dir: false,
                fs_type: String::new(),
                volume_id: volume_id_for("/etc/config"),
            },
            // emptyDir should be skipped (separate disk)
            VolumeSpec {
                destination: "/tmp".to_string(),
                source: "/var/lib/kubelet/pods/x/volumes/kubernetes.io~empty-dir/tmp".to_string(),
                readonly: false,
                is_block: false,
                is_empty_dir: true,
                fs_type: String::new(),
                volume_id: volume_id_for("/tmp"),
            },
        ];

        let disk = dir.path().join("container.img");
        create_rootfs_disk_image(&bundle.to_string_lossy(), &rootfs, &disk, &volumes)
            .expect("create_rootfs_disk_image should succeed");

        // Verify the disk image was created and is non-trivial
        let meta = fs::metadata(&disk).unwrap();
        assert!(meta.len() > 0, "disk image should not be empty");

        // Verify staging directory was cleaned up
        assert!(
            !disk.with_extension("staging").exists(),
            "staging dir should be cleaned up"
        );
    }

    #[test]
    fn prefix_to_netmask_common_values() {
        assert_eq!(super::prefix_to_netmask(24), "255.255.255.0");
        assert_eq!(super::prefix_to_netmask(16), "255.255.0.0");
        assert_eq!(super::prefix_to_netmask(32), "255.255.255.255");
        assert_eq!(super::prefix_to_netmask(0), "0.0.0.0");
    }

    #[test]
    fn compute_rootfs_hash_deterministic() {
        let dir = TempDir::new().unwrap();
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
        fs::write(rootfs.join("bin/world"), "data").unwrap();

        let h1 = compute_rootfs_hash(&rootfs).unwrap();
        let h2 = compute_rootfs_hash(&rootfs).unwrap();
        assert_eq!(h1, h2, "same content should produce same hash");
        assert_eq!(h1.len(), 16, "hash should be 16 hex chars");
    }

    #[test]
    fn compute_rootfs_hash_differs_on_content_change() {
        let dir = TempDir::new().unwrap();
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/app"), "v1").unwrap();
        let h1 = compute_rootfs_hash(&rootfs).unwrap();

        // Change file content (size changes)
        fs::write(rootfs.join("bin/app"), "v2-longer").unwrap();
        let h2 = compute_rootfs_hash(&rootfs).unwrap();

        assert_ne!(h1, h2, "different content should produce different hash");
    }

    #[test]
    fn compute_rootfs_hash_differs_on_new_file() {
        let dir = TempDir::new().unwrap();
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/app"), "data").unwrap();
        let h1 = compute_rootfs_hash(&rootfs).unwrap();

        // Add a new file
        fs::write(rootfs.join("bin/extra"), "more").unwrap();
        let h2 = compute_rootfs_hash(&rootfs).unwrap();

        assert_ne!(h1, h2, "adding a file should change the hash");
    }

    #[test]
    fn create_cached_rootfs_image_works() {
        if std::process::Command::new("which")
            .arg("mkfs.ext4")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("SKIPPING: mkfs.ext4 not available");
            return;
        }

        let dir = TempDir::new().unwrap();
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();

        let cache_path = dir.path().join("cache").join("test.img");
        create_cached_rootfs_image(&rootfs, &cache_path).expect("should create cached image");

        assert!(cache_path.exists(), "cached image should exist");
        assert!(
            cache_path.metadata().unwrap().len() > 0,
            "image should not be empty"
        );
        assert!(
            !cache_path.with_extension("staging").exists(),
            "staging cleaned up"
        );
    }

    #[test]
    fn read_volume_files_reads_directory() {
        let dir = TempDir::new().unwrap();
        let vol_src = dir.path().join("configmap");
        fs::create_dir_all(vol_src.join("subdir")).unwrap();
        fs::write(vol_src.join("key"), "value").unwrap();
        fs::write(vol_src.join("subdir/nested"), "deep").unwrap();

        let files = read_volume_files(&vol_src).unwrap();
        assert_eq!(files.len(), 2);
        let paths: Vec<&str> = files.iter().map(|(p, _, _)| p.as_str()).collect();
        assert!(paths.contains(&"key"));
        assert!(paths.contains(&"subdir/nested"));
        let key_file = files.iter().find(|(p, _, _)| p == "key").unwrap();
        assert_eq!(key_file.1, b"value");
    }

    #[test]
    fn read_volume_files_reads_single_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("secret.txt");
        fs::write(&file_path, "s3cret").unwrap();

        let files = read_volume_files(&file_path).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "secret.txt");
        assert_eq!(files[0].1, b"s3cret");
    }

    #[test]
    fn cache_and_inline_workflow() {
        if std::process::Command::new("which")
            .arg("mkfs.ext4")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("SKIPPING: mkfs.ext4 not available");
            return;
        }

        let dir = TempDir::new().unwrap();

        // Simulate rootfs
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/app"), "binary-data").unwrap();

        // Build cache (first container)
        let cache_dir = dir.path().join("cache");
        let hash = compute_rootfs_hash(&rootfs).unwrap();
        let cached_img = cache_dir.join(format!("{hash}.img"));
        create_cached_rootfs_image(&rootfs, &cached_img).expect("cache build");
        assert!(cached_img.exists());

        // Clone for container 1 (cache hit)
        let ctr1_disk = dir.path().join("ctr1.img");
        fs::copy(&cached_img, &ctr1_disk).expect("cache clone");
        assert!(ctr1_disk.exists());

        // Clone for container 2 (same image, cache hit again)
        let hash2 = compute_rootfs_hash(&rootfs).unwrap();
        assert_eq!(hash, hash2, "same rootfs should hit cache");
        let ctr2_disk = dir.path().join("ctr2.img");
        fs::copy(&cached_img, &ctr2_disk).expect("cache clone 2");
        assert!(ctr2_disk.exists());

        // Volume files are read inline (no metadata disk needed)
        let vol_src = dir.path().join("configmap");
        fs::create_dir_all(&vol_src).unwrap();
        fs::write(vol_src.join("app.conf"), "setting=1").unwrap();
        let files = read_volume_files(&vol_src).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "app.conf");
        assert_eq!(files[0].1, b"setting=1");
    }
}
