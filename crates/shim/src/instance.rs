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

use cloudhv_common::types::VmDisk;

use crate::config::load_config;
use crate::vm::VmManager;

/// Directory for cached erofs images shared across sandboxes.
/// Same container image → same erofs image (keyed by content hash of
/// the overlayfs lowerdir paths).
const EROFS_CACHE_DIR: &str = "/run/cloudhv/erofs-cache";

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

/// Tracks the VM boot lifecycle so concurrent containers can synchronize.
#[derive(Debug, Clone, PartialEq)]
enum BootState {
    /// VM process is running but vm.create/boot has not been called yet.
    NotBooted,
    /// First container is currently booting the VM.
    Booting,
    /// VM booted and agent connected successfully.
    Booted,
    /// VM boot failed — subsequent containers should not proceed.
    Failed(String),
}

/// Shared state for a running VM (one per pod).
struct SharedVmState {
    vm: VmManager,
    agent: OnceCell<cloudhv_proto::AgentServiceClient>,
    vsock_socket: PathBuf,
    shared_dir: PathBuf,
    container_count: AtomicUsize,
    api_socket: PathBuf,
    boot_state: tokio::sync::Mutex<BootState>,
    boot_complete: tokio::sync::Notify,
    // Store sandbox boot config for deferred boot
    tap_name: Option<String>,
    tap_mac: Option<String>,
    netns: Option<String>,
    cgroups_path: Option<String>,
}

/// Lazily connect to the guest agent over vsock, caching the client in the
/// `OnceCell`. The first caller pays the connection cost; subsequent callers
/// get the cached client.
///
/// Uses exponential backoff with jitter to avoid thundering herd when many
/// VMs boot simultaneously on the same node.
async fn get_or_connect_agent(
    vm_state: &SharedVmState,
) -> Result<cloudhv_proto::AgentServiceClient, Error> {
    vm_state
        .agent
        .get_or_try_init(|| async {
            let vsock_client = crate::vsock::VsockClient::new(&vm_state.vsock_socket);

            // Retry with exponential backoff + jitter.
            // Base delay doubles each attempt: 200ms, 400ms, 800ms, 1600ms, 3000ms, ...
            // Jitter adds ±50% to each delay to desynchronize concurrent boots.
            // Overall deadline: 60s hard cap regardless of attempt count.
            let max_attempts = 10u32;
            let base_delay_ms = 200u64;
            let max_delay_ms = 3000u64;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut last_err = String::new();

            for attempt in 0..max_attempts {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }

                // Try to connect directly — no separate health-check poll to avoid
                // connection churn and log spam under load.
                match vsock_client.connect_ttrpc().await {
                    Ok((agent, _health)) => {
                        if attempt > 0 {
                            info!("agent connected after {attempt} retries");
                        }
                        return Ok(agent);
                    }
                    Err(e) => {
                        last_err = format!("{e:#}");
                        if attempt < max_attempts - 1 && tokio::time::Instant::now() < deadline {
                            // Exponential backoff with jitter
                            let exp_delay = base_delay_ms * 2u64.pow(attempt);
                            let capped = exp_delay.min(max_delay_ms);
                            let jitter_seed = (std::process::id() as u64)
                                .wrapping_mul(attempt as u64 + 1)
                                .wrapping_add(0x517cc1b727220a95);
                            let jitter_frac = (jitter_seed % 100) as f64 / 100.0;
                            let jitter_ms = (capped as f64 * (0.5 + jitter_frac)) as u64;
                            info!(
                                "agent connect attempt {attempt} failed, retrying in {jitter_ms}ms: {last_err}"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms))
                                .await;
                        }
                    }
                }
            }

            Err(Error::Any(anyhow::anyhow!(
                "agent connect: {last_err} (after {max_attempts} attempts)"
            )))
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

        // Record exit so shimkit's task_delete can retrieve an exit code.
        // shimkit >=0.1.1+patch allows kill() on Exited tasks (no-op),
        // so repeated calls here are harmless.
        let _ = self.exit.set((137, Utc::now()));
        Ok(())
    }

    async fn delete(&self) -> Result<(), Error> {
        let t_total = std::time::Instant::now();
        info!("CloudHvInstance::delete id={}", self.id);

        // Ensure exit is recorded — if kill() was never called (e.g. force-delete),
        // this lets shimkit's task_delete retrieve an exit code.
        let _ = self.exit.set((137, Utc::now()));

        let vm_state = get_vm(&self.sandbox_id);

        if let Some(vm_state) = vm_state {
            // Best-effort delete RPC — use a short timeout since the VM may
            // already be dead (crash, liveness probe kill). A long timeout here
            // blocks containerd's snapshot cleanup, causing "device busy" loops.
            let t_rpc = std::time::Instant::now();
            if let Some(agent) = vm_state.agent.get() {
                let cid = self.id.clone();
                let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
                del_req.container_id = cid;
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(2));
                let _ = agent.delete_container(ctx, &del_req).await;
            }
            let rpc_ms = t_rpc.elapsed().as_millis();

            // Clean up disk image (erofs or ext4)
            if !self.is_sandbox {
                let state_dir = vm_state.shared_dir.parent().unwrap_or(&vm_state.shared_dir);
                let disk_id = format!("ctr-{}", &self.id[..12.min(self.id.len())]);
                for ext in &["erofs", "img"] {
                    let disk_path = state_dir.join(format!("{disk_id}.{ext}"));
                    match std::fs::remove_file(&disk_path) {
                        Ok(()) => info!("removed disk image: {}", disk_path.display()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => info!("failed to remove disk image: {e}"),
                    }
                }
            }

            // Decrement container count; clean up VM if zero
            let prev = vm_state.container_count.fetch_sub(1, Ordering::SeqCst);
            if prev <= 1 {
                // Clean up network state BEFORE shutting down VM — the netns
                // may be reused by the next sandbox if we don't remove the TAP
                // device and tc redirect rules.
                let t_tap = std::time::Instant::now();
                if let (Some(ref tap), Some(ref netns)) = (&vm_state.tap_name, &vm_state.netns) {
                    cleanup_tap_in_netns(netns, tap);
                }
                let tap_ms = t_tap.elapsed().as_millis();

                let t_vm = std::time::Instant::now();
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
                let vm_ms = t_vm.elapsed().as_millis();

                info!(
                    "TIMING delete {}: rpc={}ms tap_cleanup={}ms vm_cleanup={}ms total={}ms",
                    self.id,
                    rpc_ms,
                    tap_ms,
                    vm_ms,
                    t_total.elapsed().as_millis()
                );
            } else {
                info!(
                    "TIMING delete {}: rpc={}ms total={}ms (container only, {} remaining)",
                    self.id,
                    rpc_ms,
                    t_total.elapsed().as_millis(),
                    prev - 1
                );
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
        let t_total = std::time::Instant::now();

        let spec_path = self.bundle.join("config.json");
        let sandbox_spec = parse_sandbox_spec(&spec_path);

        // Set up TAP device in the pod's network namespace.
        // Retry the entire setup since CNI populates the netns asynchronously —
        // the netns file may exist but the veth/IP/routes may not be configured yet.
        let t_tap = std::time::Instant::now();
        let (tap_name, tap_mac, ip_config) = if let Some(ref netns) = sandbox_spec.netns {
            let mut result = None;
            for attempt in 0..10 {
                match setup_tap_in_netns(netns, &sandbox_id) {
                    Ok(tap_info) => {
                        if attempt > 0 {
                            info!("TAP setup succeeded after {attempt} retries");
                        }
                        result = Some(tap_info);
                        break;
                    }
                    Err(e) => {
                        if attempt < 9 {
                            info!("TAP setup attempt {attempt} failed ({e:#}), retrying...");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        } else {
                            info!("TAP setup failed after 10 attempts (proceeding without network): {e}");
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
            info!("no network namespace — VM will boot without networking");
            (None, None, None)
        };
        let tap_ms = t_tap.elapsed().as_millis();

        let t_config = std::time::Instant::now();
        let config = load_config(None).ctx("config error")?;
        let config = crate::annotations::apply_annotations(config, &sandbox_spec.annotations);
        let config = crate::annotations::apply_resource_limits(
            config,
            sandbox_spec.mem_request,
            sandbox_spec.mem_limit,
            sandbox_spec.cpu_limit,
        );

        // Cold-boot a new VM
        let mut vm = VmManager::new(sandbox_id.clone(), config.clone()).ctx("VmManager")?;

        vm.prepare().await.ctx("prepare")?;
        let config_ms = t_config.elapsed().as_millis();

        if let Some((ref ip_cidr, ref gw)) = ip_config {
            let parts: Vec<&str> = ip_cidr.split('/').collect();
            let ip = parts[0];
            let prefix: u32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
            let mask = prefix_to_netmask(prefix);
            let ip_param = format!(" ip={ip}::{gw}:{mask}::eth0:off");
            vm.append_kernel_args(&ip_param);
            info!("kernel network: {}", ip_param.trim());
        }

        let t_swtpm = std::time::Instant::now();
        vm.start_swtpm().await.ctx("swtpm")?;
        let swtpm_ms = t_swtpm.elapsed().as_millis();

        let t_vmm = std::time::Instant::now();
        vm.spawn_vmm_in_netns(sandbox_spec.netns.as_deref())
            .ctx("vmm")?;
        let spawn_ms = t_vmm.elapsed().as_millis();

        vm.wait_vmm_ready().await.ctx("vmm")?;
        let vmm_ready_ms = t_vmm.elapsed().as_millis();

        // VM boot is deferred to start_container() — the CH process is
        // spawned and listening on the API socket, but the VM is not yet
        // created/booted.  The first container's rootfs will be pre-attached
        // at boot time as /dev/vdb, eliminating hot-plug discovery latency.

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
            boot_state: tokio::sync::Mutex::new(BootState::NotBooted),
            boot_complete: tokio::sync::Notify::new(),
            tap_name,
            tap_mac,
            netns: sandbox_spec.netns.clone(),
            cgroups_path: sandbox_spec.cgroups_path.clone(),
        });

        VMS.write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sandbox_id.clone(), vm_state.clone());

        info!("sandbox VM {} ready (ch_pid={})", sandbox_id, ch_pid);
        info!(
            "TIMING start_sandbox {}: tap={}ms config={}ms swtpm={}ms vmm_spawn={}ms vmm_ready={}ms total={}ms",
            sandbox_id, tap_ms, config_ms, swtpm_ms, spawn_ms, vmm_ready_ms, t_total.elapsed().as_millis()
        );
        Ok(ch_pid)
    }

    /// Create and start an application container inside an existing sandbox VM.
    async fn start_container(&self) -> Result<u32, Error> {
        let container_id = &self.id;
        let t_total = std::time::Instant::now();
        info!("starting app container: {}", container_id);

        let vm_state = get_vm(&self.sandbox_id).ok_or_else(|| {
            Error::Any(anyhow::anyhow!("sandbox VM not found: {}", self.sandbox_id))
        })?;

        let shared_dir = &vm_state.shared_dir;

        let rootfs_path = self.bundle.join("rootfs");
        let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
        let parent_dir = shared_dir.parent().unwrap_or(shared_dir);

        // Extract volumes from OCI spec
        let spec_path = self.bundle.join("config.json");
        let volumes = extract_volumes(&spec_path).ctx("extract volumes")?;

        // Find erofs layer blobs backing the rootfs.
        // Path 1: erofs snapshotter (containerd 2.1+) — layer.erofs files exist
        // Path 2: overlayfs snapshotter (containerd 2.0) — convert rootfs dir to erofs
        let t_erofs = std::time::Instant::now();
        let erofs_layers = find_erofs_layers(&rootfs_path);
        let mut return_erofs: Option<Vec<(std::path::PathBuf, String, bool)>> = None;
        let rootfs_disks: Vec<(std::path::PathBuf, String, bool)> = if !erofs_layers.is_empty() {
            // Path 1: erofs snapshotter — pass layer.erofs blobs directly
            info!(
                "rootfs backed by {} erofs layer(s), using direct passthrough",
                erofs_layers.len()
            );
            for (i, layer) in erofs_layers.iter().enumerate() {
                let size = std::fs::metadata(layer).map(|m| m.len()).unwrap_or(0);
                info!("  erofs layer {}: {} ({} bytes)", i, layer.display(), size);
            }
            erofs_layers
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    let id = format!("{disk_id}-layer{i}");
                    (path.clone(), id, true)
                })
                .collect()
        } else {
            // Path 2: overlayfs snapshotter — create erofs image from rootfs directory.
            // Cache the erofs image keyed by the overlayfs lowerdir paths, which are
            // deterministic per container image (containerd's snapshotter creates them
            // from layer digests). This means mkfs.erofs runs once per image, not per pod.
            //
            // Race condition handling: multiple shim processes may try to build the
            // same image concurrently. We use flock on a lock file so only one
            // process runs mkfs.erofs; the others wait and then hardlink the result.
            info!("no erofs snapshotter layers, converting rootfs to erofs image");
            let cache_key = erofs_cache_key(&rootfs_path);
            let erofs_path = parent_dir.join(format!("{disk_id}.erofs"));

            if let Some(ref key) = cache_key {
                let cache_dir = std::path::Path::new(EROFS_CACHE_DIR);
                let _ = std::fs::create_dir_all(cache_dir);
                let cached = cache_dir.join(format!("{key}.erofs"));
                let lock_path = cache_dir.join(format!("{key}.lock"));

                // Acquire an exclusive lock — first process builds, others block and wait
                let lock_result = tokio::task::spawn_blocking({
                    let cached = cached.clone();
                    let erofs_path = erofs_path.clone();
                    let lock_path = lock_path.clone();
                    let rootfs_for_erofs = rootfs_path.clone();
                    move || -> anyhow::Result<bool> {
                        use std::os::unix::io::AsRawFd;
                        let lock_file = std::fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(false)
                            .open(&lock_path)?;
                        // Blocking exclusive lock — retry on EINTR, fail on other errors
                        loop {
                            let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
                            if rc == 0 {
                                break;
                            }
                            let err = std::io::Error::last_os_error();
                            if err.kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            return Err(err.into());
                        }

                        if cached.exists() {
                            // Another process built it while we waited
                            std::fs::hard_link(&cached, &erofs_path)
                                .or_else(|_| std::fs::copy(&cached, &erofs_path).map(|_| ()))?;
                            log::info!(
                                "erofs cache hit (after lock): {} ({} bytes)",
                                cached.display(),
                                std::fs::metadata(&erofs_path).map(|m| m.len()).unwrap_or(0)
                            );
                            return Ok(true); // cache hit
                        }

                        // We're the builder — run mkfs.erofs
                        let status = std::process::Command::new("mkfs.erofs")
                            .arg("--quiet")
                            .arg(&erofs_path)
                            .arg(&rootfs_for_erofs)
                            .status()
                            .map_err(|e| anyhow::anyhow!("mkfs.erofs spawn: {e}"))?;
                        if !status.success() {
                            anyhow::bail!("mkfs.erofs failed: {status}");
                        }

                        // Populate cache atomically: write to unique tmp then rename.
                        // Remove any stale tmp from a previous crash.
                        let tmp =
                            cached.with_extension(format!("erofs.{}.tmp", std::process::id()));
                        let _ = std::fs::remove_file(&tmp);
                        std::fs::hard_link(&erofs_path, &tmp)
                            .or_else(|_| std::fs::copy(&erofs_path, &tmp).map(|_| ()))?;
                        std::fs::rename(&tmp, &cached)?;

                        log::info!(
                            "erofs built and cached: {} ({} bytes)",
                            cached.display(),
                            std::fs::metadata(&erofs_path).map(|m| m.len()).unwrap_or(0)
                        );
                        // Lock released on drop
                        Ok(false) // cache miss — we built it
                    }
                })
                .await
                .map_err(|_| Error::Any(anyhow::anyhow!("erofs cache task panicked")))?
                .ctx("erofs cache")?;

                let _ = lock_result; // true=hit, false=miss — already logged
                return_erofs = Some(vec![(erofs_path.clone(), disk_id.clone(), true)]);
            }

            if return_erofs.is_none() {
                // No cache key (unusual) — build without caching
                let rootfs_for_erofs = rootfs_path.clone();
                let erofs_dst = erofs_path.clone();
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let status = std::process::Command::new("mkfs.erofs")
                        .arg("--quiet")
                        .arg(&erofs_dst)
                        .arg(&rootfs_for_erofs)
                        .status()
                        .map_err(|e| anyhow::anyhow!("mkfs.erofs: {e}"))?;
                    if !status.success() {
                        anyhow::bail!("mkfs.erofs failed: {status}");
                    }
                    Ok(())
                })
                .await
                .map_err(|_| Error::Any(anyhow::anyhow!("erofs conversion panicked")))?
                .ctx("convert rootfs to erofs")?;

                let size = std::fs::metadata(&erofs_path).map(|m| m.len()).unwrap_or(0);
                info!(
                    "erofs image created (uncached): {} ({} bytes)",
                    erofs_path.display(),
                    size
                );
                return_erofs = Some(vec![(erofs_path, disk_id.clone(), true)]);
            }

            return_erofs.unwrap()
        };
        let erofs_ms = t_erofs.elapsed().as_millis();

        // Determine boot role: first container boots the VM, others wait.
        enum BootRole {
            First,
            Subsequent,
        }

        let boot_role = {
            let mut state = vm_state.boot_state.lock().await;
            match &*state {
                BootState::NotBooted => {
                    *state = BootState::Booting;
                    BootRole::First
                }
                BootState::Booting => BootRole::Subsequent,
                BootState::Booted => BootRole::Subsequent,
                BootState::Failed(msg) => {
                    return Err(Error::Any(anyhow::anyhow!(
                        "VM boot previously failed: {msg}"
                    )));
                }
            }
        };
        let is_first_container = matches!(boot_role, BootRole::First);

        match boot_role {
            BootRole::First => {
                // First container — boot VM with rootfs disks pre-attached
                let t_boot = std::time::Instant::now();
                let extra_disks: Vec<VmDisk> = rootfs_disks
                    .iter()
                    .map(|(path, id, readonly)| VmDisk {
                        path: path.to_string_lossy().to_string(),
                        readonly: *readonly,
                        id: Some(id.clone()),
                    })
                    .collect();
                let boot_result = vm_state
                    .vm
                    .create_and_boot_vm(
                        vm_state.tap_name.as_deref(),
                        vm_state.tap_mac.as_deref(),
                        extra_disks,
                    )
                    .await;

                if let Err(e) = boot_result {
                    let msg = format!("{e:#}");
                    *vm_state.boot_state.lock().await = BootState::Failed(msg.clone());
                    vm_state.boot_complete.notify_waiters();
                    return Err(Error::Any(anyhow::anyhow!("boot with rootfs: {msg}")));
                }
                info!("VM booted with pre-attached rootfs: {disk_id}");
                let boot_ms = t_boot.elapsed().as_millis();

                // Connect to agent — confirms VM is fully booted and agent is responsive
                let t_agent = std::time::Instant::now();
                match get_or_connect_agent(&vm_state).await {
                    Ok(_) => {}
                    Err(e) => {
                        let msg = format!("{e:#}");
                        *vm_state.boot_state.lock().await = BootState::Failed(msg.clone());
                        vm_state.boot_complete.notify_waiters();
                        return Err(Error::Any(anyhow::anyhow!(
                            "agent connect after boot: {msg}"
                        )));
                    }
                }

                // Mark boot as complete and wake any waiting containers
                *vm_state.boot_state.lock().await = BootState::Booted;
                vm_state.boot_complete.notify_waiters();
                let agent_ms = t_agent.elapsed().as_millis();

                info!(
                    "TIMING first_boot {}: vm_boot={}ms agent_connect={}ms",
                    container_id, boot_ms, agent_ms
                );

                // Place CH in pod cgroup now that the VM is running
                if let Some(ref cg) = vm_state.cgroups_path {
                    let ch_pid = vm_state.vm.ch_pid().unwrap_or(std::process::id());
                    if let Err(e) = place_in_pod_cgroup(ch_pid, cg) {
                        info!("cgroup placement failed (non-fatal): {e}");
                    } else {
                        info!("CH pid {ch_pid} placed in pod cgroup: {cg}");
                    }
                }
            }
            BootRole::Subsequent => {
                // Wait for boot to complete (first container drives boot)
                loop {
                    let state = vm_state.boot_state.lock().await.clone();
                    match state {
                        BootState::Booted => break,
                        BootState::Failed(msg) => {
                            return Err(Error::Any(anyhow::anyhow!(
                                "VM boot previously failed: {msg}"
                            )));
                        }
                        _ => {
                            // Still booting — wait for notification
                            vm_state.boot_complete.notified().await;
                        }
                    }
                }

                // Hot-plug disks for this container
                let api_socket = vm_state.api_socket.clone();
                for (path, id, readonly) in &rootfs_disks {
                    let disk_json = serde_json::json!({
                        "path": path.to_string_lossy(),
                        "readonly": *readonly,
                        "id": id,
                    });
                    VmManager::api_request_to_socket(
                        &api_socket,
                        "PUT",
                        "/api/v1/vm.add-disk",
                        Some(&disk_json.to_string()),
                    )
                    .await
                    .ctx("hot-plug rootfs disk")?;
                    info!("rootfs disk hot-plugged: {id}");
                }
            }
        }

        let agent = get_or_connect_agent(&vm_state).await?;
        let api_socket = vm_state.api_socket.clone();

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

        // Build the CreateContainerRequest
        let mut create_req = cloudhv_proto::CreateContainerRequest::new();
        create_req.container_id = container_id.to_string();
        create_req.bundle_path = bundle_guest.clone();
        create_req.config_json = config_json_bytes;
        create_req.rootfs_preattached = is_first_container;
        create_req.erofs_layers = rootfs_disks.len() as u32;
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

        let t_rpc = std::time::Instant::now();
        let start_resp = if is_first_container {
            // First container — use RunContainer (create + start atomically)
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
            agent.run_container(ctx, &create_req).await
        } else {
            // Subsequent containers — create then start separately
            {
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
                agent.create_container(ctx, &create_req).await
            }
            .ctx("CreateContainer RPC error")?;
            let mut start_req = cloudhv_proto::StartContainerRequest::new();
            start_req.container_id = container_id.to_string();
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
            agent.start_container(ctx, &start_req).await
        }
        .ctx("start container RPC error")?;

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
        let rpc_ms = t_rpc.elapsed().as_millis();
        info!("started container {} pid={}", container_id, pid);
        info!(
            "TIMING start_container {}: erofs={}ms rpc={}ms total={}ms first_boot={}",
            container_id,
            erofs_ms,
            rpc_ms,
            t_total.elapsed().as_millis(),
            is_first_container
        );
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

/// Place a process into the pod's cgroup so that Kubernetes metrics (kubectl top,
/// HPA, cAdvisor) see the full VM resource usage.
///
/// Tries cgroup v2 unified hierarchy first, then systemd-style v2, then v1.
/// The `cgroups_path` comes from the OCI spec's `linux.cgroupsPath` field.
fn place_in_pod_cgroup(pid: u32, cgroups_path: &str) -> anyhow::Result<()> {
    place_in_pod_cgroup_at(pid, cgroups_path, std::path::Path::new("/sys/fs/cgroup"))
}

/// Testable implementation that accepts the cgroup root path.
fn place_in_pod_cgroup_at(
    pid: u32,
    cgroups_path: &str,
    cgroup_root: &std::path::Path,
) -> anyhow::Result<()> {
    let pid_str = pid.to_string();
    // Strip leading slash — OCI spec paths often have one
    let cgroups_path = cgroups_path.trim_start_matches('/');

    // Try cgroup v2 (unified hierarchy) — use existing path only.
    // Never create_dir_all on systemd-managed cgroup trees; that corrupts
    // systemd's transient unit tracking and can brick the node.
    let v2_path = cgroup_root.join(cgroups_path);
    if v2_path.join("cgroup.procs").exists() {
        std::fs::write(v2_path.join("cgroup.procs"), &pid_str)
            .map_err(|e| anyhow::anyhow!("write cgroup v2 procs: {e}"))?;
        return Ok(());
    }

    // Try systemd-style cgroup v2 path
    let systemd_path = to_systemd_cgroup_path(cgroups_path);
    let v2_systemd = cgroup_root.join(&systemd_path);
    if v2_systemd.join("cgroup.procs").exists() {
        std::fs::write(v2_systemd.join("cgroup.procs"), &pid_str)
            .map_err(|e| anyhow::anyhow!("write cgroup v2 systemd procs: {e}"))?;
        return Ok(());
    }

    // Try cgroup v1 (separate controller hierarchies)
    let mut placed = false;
    for controller in &["memory", "cpu,cpuacct", "pids"] {
        let v1_path = cgroup_root.join(controller).join(cgroups_path);
        if v1_path.join("cgroup.procs").exists() {
            std::fs::write(v1_path.join("cgroup.procs"), &pid_str)
                .map_err(|e| anyhow::anyhow!("write cgroup v1 {controller} procs: {e}"))?;
            placed = true;
        }
    }

    if placed {
        Ok(())
    } else {
        anyhow::bail!("no cgroup found for path {cgroups_path} (tried v2, v2-systemd, v1)")
    }
}

/// Convert a kubelet-style cgroup path to a systemd slice path.
///
/// Input:  `kubepods/burstable/pod-uid/ctr-id`
/// Output: `kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod_uid.slice/cri-containerd-ctr_id.scope`
fn to_systemd_cgroup_path(cgroups_path: &str) -> String {
    let parts: Vec<&str> = cgroups_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return cgroups_path.to_string();
    }

    let mut result = String::new();
    let mut prefix = String::new();
    for (i, part) in parts.iter().enumerate() {
        let sanitized = part.replace('-', "_");
        if i == parts.len() - 1 {
            // Last segment is a scope (container ID)
            result.push_str(&format!("cri-containerd-{sanitized}.scope"));
        } else {
            // Intermediate segments are slices with hierarchical prefix
            if prefix.is_empty() {
                result.push_str(&format!("{sanitized}.slice/"));
                prefix = sanitized;
            } else {
                let slice_name = format!("{prefix}-{sanitized}");
                result.push_str(&format!("{slice_name}.slice/"));
                prefix = slice_name;
            }
        }
    }
    result
}

/// Compute a cache key for the erofs image based on the overlayfs lowerdir paths.
/// The lowerdirs are containerd snapshot directories keyed by image layer digests,
/// so the same container image always produces the same set of lowerdirs.
/// Returns None if the rootfs isn't an overlayfs mount.
fn erofs_cache_key(rootfs_path: &std::path::Path) -> Option<String> {
    erofs_cache_key_from_mountinfo(
        rootfs_path,
        &std::fs::read_to_string("/proc/self/mountinfo").ok()?,
    )
}

/// Testable implementation that accepts a mountinfo string.
fn erofs_cache_key_from_mountinfo(
    rootfs_path: &std::path::Path,
    mountinfo: &str,
) -> Option<String> {
    let path_str = rootfs_path.to_string_lossy();

    for line in mountinfo.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() > 4 && parts[4] == path_str.as_ref() {
            if let Some(sep) = parts.iter().position(|&p| p == "-") {
                if parts.len() > sep + 1 && parts[sep + 1] == "overlay" {
                    // Extract lowerdir from mount options (after separator)
                    let all_opts = parts.get(sep + 3).unwrap_or(&"");
                    for opt in all_opts.split(',') {
                        if let Some(dirs) = opt.strip_prefix("lowerdir=") {
                            return Some(stable_hash_hex(dirs));
                        }
                    }
                    // Also check before separator
                    for part in parts.iter().take(sep).skip(5) {
                        if let Some(dirs) = part.strip_prefix("lowerdir=") {
                            return Some(stable_hash_hex(dirs));
                        }
                    }
                }
            }
        }
    }
    None
}

/// Stable 128-bit hash (SipHash-like, deterministic across Rust versions).
/// Uses FNV-1a 128-bit, which is simple, stable, and collision-resistant
/// enough for a filesystem cache key.
fn stable_hash_hex(input: &str) -> String {
    const FNV_OFFSET: u128 = 0x6c62272e07bb0142_62b821756295c58d;
    const FNV_PRIME: u128 = 0x0000000001000000_000000000000013b;
    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:032x}")
}

/// Find erofs layer blobs backing the container's rootfs.
///
/// When the erofs snapshotter is active, containerd mounts the rootfs as
/// overlayfs with erofs lower layers.  We parse `/proc/self/mountinfo` to
/// find the overlay mount at `rootfs_path`, extract its `lowerdir` entries,
/// then find the erofs source files backing those mounts.
///
/// Returns the layer.erofs file paths in overlayfs lowerdir order
/// (leftmost = top/highest precedence), or an empty
/// vec if the rootfs is not backed by erofs (e.g., plain overlayfs).
fn find_erofs_layers(rootfs_path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mountinfo = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    let path_str = rootfs_path.to_string_lossy();

    // Step 1: find the overlay mount at rootfs_path and extract lowerdirs
    let mut lowerdirs: Vec<String> = Vec::new();
    for line in mountinfo.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() > 4 && parts[4] == path_str.as_ref() {
            if let Some(sep) = parts.iter().position(|&p| p == "-") {
                if parts.len() > sep + 1 && parts[sep + 1] == "overlay" {
                    // Found the overlay mount — extract lowerdir from options
                    let super_opts = parts.get(sep + 3).unwrap_or(&"");
                    for opt in super_opts.split(',') {
                        if let Some(dirs) = opt.strip_prefix("lowerdir=") {
                            lowerdirs = dirs.split(':').map(String::from).collect();
                            break;
                        }
                    }
                    // Also check mount options before the separator
                    for part in parts.iter().take(sep).skip(5) {
                        if let Some(dirs) = part.strip_prefix("lowerdir=") {
                            lowerdirs = dirs.split(':').map(String::from).collect();
                            break;
                        }
                    }
                }
            }
        }
    }

    if lowerdirs.is_empty() {
        return Vec::new();
    }

    // Step 2: for each lowerdir, find the erofs source file from mountinfo
    let mut erofs_layers: Vec<std::path::PathBuf> = Vec::new();
    for ldir in &lowerdirs {
        for line in mountinfo.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() > 4 && parts[4] == ldir.as_str() {
                if let Some(sep) = parts.iter().position(|&p| p == "-") {
                    if parts.len() > sep + 2 && parts[sep + 1] == "erofs" {
                        let src = parts[sep + 2];
                        let path = std::path::PathBuf::from(src);
                        if path.exists() {
                            erofs_layers.push(path);
                        }
                    }
                }
            }
        }
    }

    erofs_layers
}

/// Parsed sandbox configuration from the OCI spec.
struct SandboxSpec {
    netns: Option<String>,
    annotations: HashMap<String, String>,
    mem_request: Option<u64>,
    mem_limit: Option<u64>,
    cpu_limit: Option<u32>,
    cgroups_path: Option<String>,
}

/// Parse sandbox OCI spec for network namespace, annotations, and resources.
fn parse_sandbox_spec(spec_path: &std::path::Path) -> SandboxSpec {
    let empty = SandboxSpec {
        netns: None,
        annotations: HashMap::new(),
        mem_request: None,
        mem_limit: None,
        cpu_limit: None,
        cgroups_path: None,
    };
    let data = match std::fs::read_to_string(spec_path) {
        Ok(d) => d,
        Err(_) => return empty,
    };
    let spec: serde_json::Value = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(_) => return empty,
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
    let cpu_limit = crate::annotations::cpu_resources_from_spec(&spec);

    // Extract cgroups path for VM process placement
    let cgroups_path = spec
        .pointer("/linux/cgroupsPath")
        .and_then(|v| v.as_str())
        .map(String::from);

    if netns.is_some() {
        info!("sandbox netns: {:?}", netns);
    }
    if !annotations.is_empty() {
        info!("sandbox annotations: {:?}", annotations);
    }
    if req.is_some() || lim.is_some() || cpu_limit.is_some() {
        info!(
            "sandbox resources: mem_request={:?}MiB mem_limit={:?}MiB cpu_limit={:?}vcpus",
            req, lim, cpu_limit
        );
    }
    if cgroups_path.is_some() {
        info!("sandbox cgroups: {:?}", cgroups_path);
    }

    SandboxSpec {
        netns,
        annotations,
        mem_request: req,
        mem_limit: lim,
        cpu_limit,
        cgroups_path,
    }
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

/// Create an erofs disk image containing the container rootfs.
///
/// Uses `mkfs.erofs` to create a read-only erofs image from the rootfs
/// directory. The shim passes this to the VM as a read-only virtio-blk device.
#[cfg(test)]
fn create_rootfs_erofs_image(
    rootfs_path: &std::path::Path,
    disk_path: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    let status = Command::new("mkfs.erofs")
        .args(["--quiet"])
        .arg(disk_path)
        .arg(rootfs_path)
        .status()
        .context("mkfs.erofs")?;

    if !status.success() {
        anyhow::bail!("mkfs.erofs failed: {status}");
    }

    log::info!("erofs image created: {}", disk_path.display());
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
/// Clean up TAP device and tc redirect rules in the pod's network namespace.
/// Must be called before the VM is destroyed so the next sandbox using this
/// netns gets a clean slate. Best-effort — failures are logged but don't
/// prevent further cleanup.
fn cleanup_tap_in_netns(netns_path: &str, tap_name: &str) {
    use std::process::Command;

    let netns_arg = format!("--net={netns_path}");

    // Remove tc ingress qdiscs (which hold the redirect filters).
    // We delete from the TAP first, then the veth. If the TAP is already
    // gone (e.g. netns was destroyed), this is a no-op.
    for dev in [tap_name, "eth0"] {
        let _ = Command::new("nsenter")
            .args([
                &netns_arg, "--", "tc", "qdisc", "del", "dev", dev, "ingress",
            ])
            .output();
    }

    // Delete the TAP device
    let result = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "link", "del", tap_name])
        .output();
    match result {
        Ok(output) if output.status.success() => {
            log::info!("cleaned up TAP {tap_name} in netns {netns_path}");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::info!("TAP {tap_name} cleanup (may already be gone): {stderr}");
        }
        Err(e) => {
            log::info!("TAP cleanup nsenter failed (netns gone?): {e}");
        }
    }
}

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

    // Wait for the network namespace to exist — CNI creates it asynchronously
    // and the shim may start before the netns file appears.
    for attempt in 0..20 {
        if std::path::Path::new(netns_path).exists() {
            if attempt > 0 {
                log::info!("netns {netns_path} appeared after {attempt} retries");
            }
            break;
        }
        if attempt == 19 {
            anyhow::bail!("netns {netns_path} did not appear after 2s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Dump pre-existing netns state for diagnostics
    if let Ok(output) = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "link", "show"])
        .output()
    {
        let links = String::from_utf8_lossy(&output.stdout);
        log::info!("netns pre-setup links:\n{links}");
    }
    if let Ok(output) = Command::new("nsenter")
        .args([&netns_arg, "--", "tc", "qdisc", "show"])
        .output()
    {
        let qdiscs = String::from_utf8_lossy(&output.stdout);
        if qdiscs.contains("ingress") {
            log::warn!("netns has pre-existing tc ingress rules — stale state:\n{qdiscs}");
        }
    }

    // Run the setup commands inside the network namespace using nsenter
    // Clean up any stale TAP from a previous failed attempt (the caller retries)
    let _ = Command::new("nsenter")
        .args([&netns_arg, "--", "ip", "link", "del", &tap_name])
        .output();

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

    // Find the veth device and its IP/MAC — retry briefly since CNI may
    // still be configuring addresses when the shim starts.
    let mut veth_name = String::new();
    let mut ip_cidr = String::new();
    let mut mac = String::new();

    for attempt in 0..10 {
        let output = Command::new("nsenter")
            .args([&netns_arg, "--", "ip", "-j", "addr", "show"])
            .output()
            .context("ip addr show")?;
        let addrs: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or(serde_json::json!([]));

        if let Some(interfaces) = addrs.as_array() {
            for iface in interfaces {
                let name = iface.get("ifname").and_then(|n| n.as_str()).unwrap_or("");
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
        if !veth_name.is_empty() {
            if attempt > 0 {
                log::info!("veth with IP appeared after {attempt} retries");
            }
            break;
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    if veth_name.is_empty() || ip_cidr.is_empty() {
        anyhow::bail!("could not find veth with IP in netns {netns_path} after retries");
    }

    // Get default gateway — retry briefly since CNI may still be populating
    // routes when the shim starts.
    let mut gateway = String::new();
    for attempt in 0..10 {
        let output = Command::new("nsenter")
            .args([&netns_arg, "--", "ip", "-j", "route", "show", "default"])
            .output()
            .context("ip route show default")?;
        let routes: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or(serde_json::json!([]));
        if let Some(gw) = routes
            .as_array()
            .and_then(|r| r.first())
            .and_then(|r| r.get("gateway"))
            .and_then(|g| g.as_str())
        {
            gateway = gw.to_string();
            if attempt > 0 {
                log::info!("default route appeared after {attempt} retries");
            }
            break;
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    if gateway.is_empty() {
        anyhow::bail!("no default route in netns {netns_path} after retries (CNI not ready?)");
    }

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
    fn volume_id_is_deterministic() {
        let id1 = volume_id_for("/etc/config");
        let id2 = volume_id_for("/etc/config");
        assert_eq!(id1, id2);
        // Different paths produce different IDs
        let id3 = volume_id_for("/etc/secrets");
        assert_ne!(id1, id3);
    }

    #[test]
    fn prefix_to_netmask_common_values() {
        assert_eq!(super::prefix_to_netmask(24), "255.255.255.0");
        assert_eq!(super::prefix_to_netmask(16), "255.255.0.0");
        assert_eq!(super::prefix_to_netmask(32), "255.255.255.255");
        assert_eq!(super::prefix_to_netmask(0), "0.0.0.0");
    }

    #[test]
    fn to_systemd_cgroup_path_converts_correctly() {
        assert_eq!(
            super::to_systemd_cgroup_path("kubepods/burstable/pod-abc/ctr-123"),
            "kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod_abc.slice/cri-containerd-ctr_123.scope"
        );
        assert_eq!(
            super::to_systemd_cgroup_path("kubepods/besteffort/pod-xyz/ctr-456"),
            "kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod_xyz.slice/cri-containerd-ctr_456.scope"
        );
    }

    #[test]
    fn place_in_pod_cgroup_v2_unified() {
        let dir = TempDir::new().unwrap();
        let cg = dir.path().join("kubepods/burstable/pod-abc");
        fs::create_dir_all(&cg).unwrap();
        fs::write(cg.join("cgroup.procs"), "").unwrap();

        super::place_in_pod_cgroup_at(12345, "kubepods/burstable/pod-abc", dir.path())
            .expect("should place in v2 cgroup");

        let content = fs::read_to_string(cg.join("cgroup.procs")).unwrap();
        assert_eq!(content, "12345");
    }

    #[test]
    fn place_in_pod_cgroup_v1_fallback() {
        let dir = TempDir::new().unwrap();
        // Create v1-style hierarchy (no unified v2 path)
        let mem_cg = dir.path().join("memory/kubepods/burstable/pod-abc");
        let cpu_cg = dir.path().join("cpu,cpuacct/kubepods/burstable/pod-abc");
        fs::create_dir_all(&mem_cg).unwrap();
        fs::create_dir_all(&cpu_cg).unwrap();
        fs::write(mem_cg.join("cgroup.procs"), "").unwrap();
        fs::write(cpu_cg.join("cgroup.procs"), "").unwrap();

        super::place_in_pod_cgroup_at(99999, "kubepods/burstable/pod-abc", dir.path())
            .expect("should place in v1 cgroups");

        assert_eq!(
            fs::read_to_string(mem_cg.join("cgroup.procs")).unwrap(),
            "99999"
        );
        assert_eq!(
            fs::read_to_string(cpu_cg.join("cgroup.procs")).unwrap(),
            "99999"
        );
    }

    #[test]
    fn place_in_pod_cgroup_no_path_fails() {
        let dir = TempDir::new().unwrap();
        let result = super::place_in_pod_cgroup_at(1, "nonexistent/path", dir.path());
        assert!(result.is_err());
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
    fn erofs_cache_key_extracts_lowerdir() {
        let mountinfo = "389 34 0:50 / /run/containerd/task/k8s.io/abc/rootfs rw,relatime shared:335 - overlay overlay rw,lowerdir=/snap/917/fs:/snap/916/fs:/snap/915/fs,upperdir=/snap/930/fs,workdir=/snap/930/work";
        let key = super::erofs_cache_key_from_mountinfo(
            std::path::Path::new("/run/containerd/task/k8s.io/abc/rootfs"),
            mountinfo,
        );
        assert!(key.is_some());
        // Same input → same key
        let key2 = super::erofs_cache_key_from_mountinfo(
            std::path::Path::new("/run/containerd/task/k8s.io/abc/rootfs"),
            mountinfo,
        );
        assert_eq!(key, key2);
    }

    #[test]
    fn erofs_cache_key_different_lowerdirs_differ() {
        let mi1 = "1 0 0:1 / /rootfs1 rw - overlay overlay rw,lowerdir=/a:/b";
        let mi2 = "1 0 0:1 / /rootfs1 rw - overlay overlay rw,lowerdir=/a:/c";
        let k1 = super::erofs_cache_key_from_mountinfo(std::path::Path::new("/rootfs1"), mi1);
        let k2 = super::erofs_cache_key_from_mountinfo(std::path::Path::new("/rootfs1"), mi2);
        assert_ne!(k1, k2);
    }

    #[test]
    fn erofs_cache_key_none_for_non_overlay() {
        let mountinfo = "1 0 8:1 / /rootfs rw - ext4 /dev/sda1 rw";
        let key = super::erofs_cache_key_from_mountinfo(std::path::Path::new("/rootfs"), mountinfo);
        assert!(key.is_none());
    }

    #[test]
    fn stable_hash_is_deterministic() {
        let h1 = super::stable_hash_hex("hello world");
        let h2 = super::stable_hash_hex("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32); // 128-bit hex
        let h3 = super::stable_hash_hex("different input");
        assert_ne!(h1, h3);
    }
}
