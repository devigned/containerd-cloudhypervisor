use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

use chrono::{DateTime, Utc};
use containerd_shimkit::sandbox::instance::{Instance, InstanceConfig};
use containerd_shimkit::sandbox::sync::WaitableCell;
use containerd_shimkit::sandbox::Error;
use log::info;

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
    agent: cloudhv_proto::AgentServiceClient,
    shared_dir: PathBuf,
    container_count: AtomicUsize,
    api_socket: PathBuf,
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

        // Best-effort agent RPC — fire and forget
        if let Some(vm_state) = get_vm(&self.sandbox_id) {
            let cid = self.id.clone();
            let agent = vm_state.agent.clone();
            tokio::spawn(async move {
                let mut kreq = cloudhv_proto::KillContainerRequest::new();
                kreq.container_id = cid;
                kreq.signal = signal;
                kreq.all = true;
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
                let _ = agent.kill_container(ctx, &kreq).await;
            });
        }

        let _ = self.exit.set((137, Utc::now()));
        Ok(())
    }

    async fn delete(&self) -> Result<(), Error> {
        info!("CloudHvInstance::delete id={}", self.id);

        let vm_state = get_vm(&self.sandbox_id);

        if let Some(vm_state) = vm_state {
            // Best-effort delete RPC
            let agent = vm_state.agent.clone();
            let cid = self.id.clone();
            let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
            del_req.container_id = cid;
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(10));
            let _ = agent.delete_container(ctx, &del_req).await;

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

        vm.spawn_virtiofsd().ctx("virtiofsd")?;
        vm.spawn_vmm_in_netns(netns_path.as_deref()).ctx("vmm")?;

        let (vfsd_r, vmm_r) = tokio::join!(vm.wait_virtiofsd_ready(), vm.wait_vmm_ready());
        vfsd_r.ctx("virtiofsd")?;
        vmm_r.ctx("vmm")?;

        vm.create_and_boot_vm(tap_name.as_deref(), tap_mac.as_deref())
            .await
            .ctx("boot")?;

        vm.wait_for_agent().await.ctx("agent")?;

        let vsock_client = crate::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _health) = vsock_client.connect_ttrpc().await.ctx("ttrpc")?;

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
                vsock_socket: vm.vsock_socket().to_path_buf(),
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
            agent,
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

        let agent = vm_state.agent.clone();
        let shared_dir = &vm_state.shared_dir;

        // Set up I/O files in the shared directory
        let io_dir = shared_dir.join("io").join(container_id);
        std::fs::create_dir_all(&io_dir).ctx("failed to create I/O dir")?;
        let stdout_guest = format!(
            "{}/io/{}/stdout",
            cloudhv_common::VIRTIOFS_GUEST_MOUNT,
            container_id
        );
        let stderr_guest = format!(
            "{}/io/{}/stderr",
            cloudhv_common::VIRTIOFS_GUEST_MOUNT,
            container_id
        );

        // Create an ext4 disk image from the rootfs and hot-plug it into the VM.
        let rootfs_path = self.bundle.join("rootfs");
        let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
        let disk_path = shared_dir
            .parent()
            .unwrap_or(shared_dir)
            .join(format!("{}.img", disk_id));

        info!(
            "creating disk image: {} from rootfs {}",
            disk_path.display(),
            rootfs_path.display()
        );

        let bundle_str = self.bundle.to_string_lossy().to_string();
        let disk_path_clone = disk_path.clone();
        let rootfs_clone = rootfs_path.clone();
        tokio::task::spawn_blocking(move || {
            create_rootfs_disk_image(&bundle_str, &rootfs_clone, &disk_path_clone)
        })
        .await
        .map_err(|_| Error::Any(anyhow::anyhow!("disk image task panicked")))?
        .ctx("disk image")?;

        info!("disk image created: {}", disk_path.display());

        // Hot-plug the disk into the VM
        let disk_path_str = disk_path.to_string_lossy().to_string();
        let api_socket = vm_state.api_socket.clone();

        info!(
            "hot-plugging disk {} to VM via {}",
            disk_id,
            api_socket.display()
        );
        let disk_json = serde_json::json!({
            "path": disk_path_str,
            "readonly": false,
            "id": disk_id,
        });
        let add_disk_resp = VmManager::api_request_to_socket(
            &api_socket,
            "PUT",
            "/api/v1/vm.add-disk",
            Some(&disk_json.to_string()),
        )
        .await
        .ctx("hot-plug disk")?;
        info!("disk hot-plugged: {}", add_disk_resp);

        let bundle_guest = format!("/run/containers/{}", container_id);

        // Send CreateContainer RPC to the guest agent
        {
            let mut create_req = cloudhv_proto::CreateContainerRequest::new();
            create_req.container_id = container_id.to_string();
            create_req.bundle_path = bundle_guest;
            create_req.stdout = stdout_guest;
            create_req.stderr = stderr_guest;
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

        // Forward container stdout/stderr from virtio-fs files to containerd FIFOs.
        // This makes `kubectl logs` and `crictl logs` work.
        // We open the FIFOs synchronously here (before start returns) so containerd's
        // reader sees an active writer and doesn't get EOF.
        let stdout_src = io_dir.join("stdout");
        let stderr_src = io_dir.join("stderr");
        let stdout_dst = self.stdout.to_string_lossy().to_string();
        let stderr_dst = self.stderr.to_string_lossy().to_string();
        if !stdout_dst.is_empty() {
            let fifo = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&stdout_dst);
            if let Ok(fifo) = fifo {
                tokio::spawn(forward_output(stdout_src, fifo));
            }
        }
        if !stderr_dst.is_empty() {
            let fifo = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&stderr_dst);
            if let Ok(fifo) = fifo {
                tokio::spawn(forward_output(stderr_src, fifo));
            }
        }

        // Background task monitors container exit via agent WaitContainer RPC
        let exit = self.exit.clone();
        let cid = container_id.to_string();
        let agent_clone = vm_state.agent.clone();
        tokio::spawn(async move {
            let mut wait_req = cloudhv_proto::WaitContainerRequest::new();
            wait_req.container_id = cid.clone();
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(86400));
            let exit_code = match agent_clone.wait_container(ctx, &wait_req).await {
                Ok(resp) => resp.exit_status,
                Err(e) => {
                    info!("wait_container RPC error for {}: {e}", cid);
                    137
                }
            };
            info!("container {} exited with code {}", cid, exit_code);
            let _ = exit.set((exit_code, Utc::now()));
        });

        let pid = start_resp.pid;
        info!("started container {} pid={}", container_id, pid);
        Ok(pid)
    }
}

// ---------------------------------------------------------------------------
// I/O forwarding
// ---------------------------------------------------------------------------

/// Forward container output from a virtio-fs file to an already-opened containerd FIFO.
///
/// The agent writes crun's stdout/stderr to files in the virtio-fs shared
/// directory. This task tails the file and writes to the FIFO so that
/// `crictl logs` and `kubectl logs` work.
async fn forward_output(src: std::path::PathBuf, fifo: std::fs::File) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Wait for the source file to appear (agent creates it on container start)
    for _ in 0..100 {
        if src.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let src_file = match tokio::fs::File::open(&src).await {
        Ok(f) => f,
        Err(e) => {
            info!("I/O forward: can't open source {}: {e}", src.display());
            return;
        }
    };

    let mut reader = BufReader::new(src_file);
    let mut writer = tokio::fs::File::from_std(fifo);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF — file may still be written to, poll
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if tokio::fs::metadata(&src).await.is_err() {
                    break; // File removed, stop
                }
            }
            Ok(_) => {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break; // FIFO reader disconnected
                }
            }
            Err(_) => break,
        }
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

/// Create an ext4 disk image containing the OCI bundle (config.json + rootfs).
///
/// The image is sized to fit the rootfs content plus headroom. The guest agent
/// mounts this block device and runs crun from it — no FUSE involved.
fn create_rootfs_disk_image(
    bundle_path: &str,
    rootfs_path: &std::path::Path,
    disk_path: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    // Calculate rootfs size
    let rootfs_size = dir_size(rootfs_path)?;
    // Add 50% headroom + 16MB for ext4 metadata, minimum 64MB
    let image_size_mb = std::cmp::max(64, (rootfs_size * 3 / 2 / 1024 / 1024 + 16) as u64);

    log::info!(
        "creating disk image: {}MB for rootfs ({}MB content)",
        image_size_mb,
        rootfs_size / 1024 / 1024
    );

    // Create sparse file
    let f = std::fs::File::create(disk_path)
        .with_context(|| format!("create disk image: {}", disk_path.display()))?;
    f.set_len(image_size_mb * 1024 * 1024)?;
    drop(f);

    // Format as ext4
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F"])
        .arg(disk_path)
        .status()
        .context("mkfs.ext4")?;
    if !status.success() {
        anyhow::bail!("mkfs.ext4 failed: {status}");
    }

    // Mount, copy content, unmount
    let mount_dir = disk_path.with_extension("mnt");
    std::fs::create_dir_all(&mount_dir)?;

    let status = Command::new("mount")
        .args(["-o", "loop"])
        .arg(disk_path)
        .arg(&mount_dir)
        .status()
        .context("mount disk image")?;
    if !status.success() {
        anyhow::bail!("mount disk image failed: {status}");
    }

    // Copy rootfs content into the image
    let rootfs_dest = mount_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs_dest)?;
    let status = Command::new("cp")
        .args(["-a", "--"])
        .arg(format!("{}/.", rootfs_path.display()))
        .arg(&rootfs_dest)
        .status()
        .context("cp rootfs to disk image")?;

    // Copy config.json
    let config_src = std::path::Path::new(bundle_path).join("config.json");
    if config_src.exists() {
        std::fs::copy(&config_src, mount_dir.join("config.json"))?;
    }

    // Unmount
    let umount_status = Command::new("umount").arg(&mount_dir).status();
    std::fs::remove_dir(&mount_dir).ok();

    if !status.success() {
        anyhow::bail!("cp rootfs failed: {status}");
    }
    if let Ok(s) = umount_status {
        if !s.success() {
            anyhow::bail!("umount failed: {s}");
        }
    }

    log::info!("disk image created: {}", disk_path.display());
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

/// Information about a TAP device created for VM networking.
struct TapInfo {
    tap_name: String,
    mac: String,
    ip_cidr: String,
    gateway: String,
}

/// Create a TAP device in the pod's network namespace and set up
/// traffic redirection between the CNI veth and the TAP.
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
