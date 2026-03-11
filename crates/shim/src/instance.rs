use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use containerd_shim::api;
use containerd_shim::asynchronous::{spawn, ExitSignal, Shim};
use containerd_shim::{Config, Error, Flags, StartOpts, TtrpcResult};
use containerd_shim_protos::shim_async::Task;
use containerd_shim_protos::ttrpc::r#async::TtrpcContext;
use log::info;
use protobuf::well_known_types::timestamp::Timestamp;

use crate::config::load_config;
use crate::pool::VmPool;
use crate::vm::VmManager;

/// Create a protobuf Timestamp for the current time.
fn timestamp_now() -> protobuf::MessageField<Timestamp> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut ts = Timestamp::new();
    ts.seconds = now.as_secs() as i64;
    ts.nanos = now.subsec_nanos() as i32;
    protobuf::MessageField::some(ts)
}

/// A running VM that may host multiple containers.
struct VmState {
    vm: VmManager,
    agent: cloudhv_proto::AgentServiceClient,
    container_count: usize,
    shared_dir: std::path::PathBuf,
}

/// Per-container state tracked by the shim.
struct ContainerState {
    vm_id: String,
    pid: Option<u32>,
    exit_code: Option<u32>,
    /// Notifies wait() when exit_code is set by kill().
    exit_notify: Arc<tokio::sync::Notify>,
    /// Host-side file in the virtio-fs shared dir (agent writes crun output here)
    stdout_shared_path: Option<std::path::PathBuf>,
    stderr_shared_path: Option<std::path::PathBuf>,
    /// Containerd's FIFO paths (shim must write container output here)
    stdout_fifo: Option<String>,
    stderr_fifo: Option<String>,
    /// Bind-mounted volume paths in the shared dir (need unmount on delete)
    volume_mounts: Vec<std::path::PathBuf>,
    /// Hot-plugged block volume disk IDs (need vm.remove-device on delete)
    block_volume_ids: Vec<String>,
}

/// The Cloud Hypervisor containerd shim implementation.
#[derive(Clone)]
pub struct CloudHvShim {
    exit: Arc<ExitSignal>,
    /// Active VMs keyed by VM ID.
    vms: Arc<Mutex<HashMap<String, VmState>>>,
    /// Containers keyed by container ID, referencing their VM.
    containers: Arc<Mutex<HashMap<String, ContainerState>>>,
    /// Pool of pre-warmed VMs for instant container start.
    pool: Arc<tokio::sync::Mutex<VmPool>>,
}

#[async_trait]
impl Shim for CloudHvShim {
    type T = CloudHvShim;

    async fn new(_runtime_id: &str, _args: &Flags, _config: &mut Config) -> Self {
        // Load config but don't warm the pool here — new() is called for
        // every shim action (start, delete, main). Pool warming should only
        // happen in the main daemon loop, not during the "start" fork.
        let rt_config =
            load_config(None).unwrap_or_else(|_| cloudhv_common::types::RuntimeConfig {
                cloud_hypervisor_binary: cloudhv_common::DEFAULT_CH_BINARY.to_string(),
                virtiofsd_binary: cloudhv_common::DEFAULT_VIRTIOFSD_BINARY.to_string(),
                kernel_path: String::new(),
                rootfs_path: String::new(),
                default_vcpus: cloudhv_common::DEFAULT_VCPUS,
                default_memory_mb: cloudhv_common::DEFAULT_MEMORY_MB,
                vsock_port: cloudhv_common::AGENT_VSOCK_PORT,
                agent_startup_timeout_secs: cloudhv_common::AGENT_STARTUP_TIMEOUT_SECS,
                kernel_args: "console=hvc0 root=/dev/vda rw quiet init=/init net.ifnames=0"
                    .to_string(),
                debug: false,
                pool_size: cloudhv_common::DEFAULT_POOL_SIZE,
                max_containers_per_vm: cloudhv_common::DEFAULT_MAX_CONTAINERS_PER_VM,
                hotplug_memory_mb: 0,
                hotplug_method: "acpi".to_string(),
                tpm_enabled: false,
            });

        let pool = VmPool::new(rt_config);

        CloudHvShim {
            exit: Arc::new(ExitSignal::default()),
            vms: Arc::new(Mutex::new(HashMap::new())),
            containers: Arc::new(Mutex::new(HashMap::new())),
            pool: Arc::new(tokio::sync::Mutex::new(pool)),
        }
    }

    async fn start_shim(&mut self, opts: StartOpts) -> Result<String, Error> {
        // Use Kubernetes sandbox-id annotation for grouping (like runwasi).
        // This ensures all containers in a pod share the same shim instance.
        let dir = std::env::current_dir().map_err(|e| Error::Other(e.to_string()))?;
        let grouping = match oci_spec::runtime::Spec::load(dir.join("config.json")) {
            Ok(spec) => spec
                .annotations()
                .as_ref()
                .and_then(|a| a.get("io.kubernetes.cri.sandbox-id"))
                .cloned()
                .unwrap_or_else(|| opts.id.clone()),
            Err(_) => opts.id.clone(),
        };
        let address = spawn(opts, &grouping, Vec::new()).await?;
        Ok(address)
    }

    async fn delete_shim(&mut self) -> Result<api::DeleteResponse, Error> {
        Ok(api::DeleteResponse::new())
    }

    async fn wait(&mut self) {
        self.exit.wait().await;
    }

    async fn create_task_service(
        &self,
        _publisher: containerd_shim::asynchronous::publisher::RemotePublisher,
    ) -> Self::T {
        // Initialize logging in daemon mode only — never before run()
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .try_init()
            .ok();
        let backend = crate::hypervisor::detect_hypervisor();
        log::info!(
            "containerd-shim-cloudhv-v1 daemon started (version {}, hypervisor: {})",
            env!("CARGO_PKG_VERSION"),
            backend
        );
        self.clone()
    }
}

/// Task service implementation: handles container lifecycle over ttrpc.
impl CloudHvShim {
    /// Get the agent client for a container by looking up its VM.
    fn get_agent_for_container(
        &self,
        container_id: &str,
    ) -> TtrpcResult<cloudhv_proto::AgentServiceClient> {
        let containers = self.containers.lock().unwrap();
        let container = containers.get(container_id).ok_or_else(|| {
            containerd_shim_protos::ttrpc::Error::Others(format!(
                "container not found: {container_id}"
            ))
        })?;
        let vms = self.vms.lock().unwrap();
        let vm_state = vms.get(&container.vm_id).ok_or_else(|| {
            containerd_shim_protos::ttrpc::Error::Others(format!(
                "VM not found for container: {container_id}"
            ))
        })?;
        Ok(vm_state.agent.clone())
    }

    /// Create a sandbox: boot the VM with networking.
    ///
    /// Reads the network namespace from the OCI spec (assigned by containerd's
    /// CNI), creates a TAP device in that namespace, and boots the VM with
    /// virtio-net connected to the TAP. The guest kernel gets the pod IP
    /// via boot parameters.
    async fn create_sandbox(
        &self,
        sandbox_id: &str,
        bundle_path: &str,
    ) -> TtrpcResult<api::CreateTaskResponse> {
        info!("creating sandbox (VM): {}", sandbox_id);

        // Extract network namespace, annotations, and resource limits from the OCI spec
        let spec_path = std::path::Path::new(bundle_path).join("config.json");
        let (netns_path, pod_annotations, mem_request, mem_limit) =
            if let Ok(data) = std::fs::read_to_string(&spec_path) {
                if let Ok(spec) = serde_json::from_str::<serde_json::Value>(&data) {
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
                    info!("sandbox netns: {:?}", netns);
                    if !annotations.is_empty() {
                        info!("sandbox annotations: {:?}", annotations);
                    }
                    if req.is_some() || lim.is_some() {
                        info!("sandbox resources: request={:?}MiB limit={:?}MiB", req, lim);
                    }
                    (netns, annotations, req, lim)
                } else {
                    (None, std::collections::HashMap::new(), None, None)
                }
            } else {
                (None, std::collections::HashMap::new(), None, None)
            };

        // Set up TAP device in the pod's network namespace
        let (tap_name, tap_mac, ip_config) = if let Some(ref netns) = netns_path {
            match setup_tap_in_netns(netns, sandbox_id) {
                Ok(info) => {
                    info!(
                        "TAP created: dev={} mac={} ip={} gw={}",
                        info.tap_name, info.mac, info.ip_cidr, info.gateway
                    );
                    (
                        Some(info.tap_name),
                        Some(info.mac),
                        Some((info.ip_cidr, info.gateway)),
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

        let config = load_config(None).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("config error: {e}"))
        })?;

        // Apply pod annotations to override VM resource settings
        let config = crate::annotations::apply_annotations(config, &pod_annotations);

        // Apply OCI resource limits — enables hotplug when limit > request
        let config = crate::annotations::apply_resource_limits(config, mem_request, mem_limit);

        let (vm, agent) = {
            let mut pool = self.pool.lock().await;
            if let Some(warm) = pool.try_acquire() {
                info!("acquired VM {} from pool", warm.vm.vm_id());
                // Pool VMs don't have networking — would need TAP added later
                (warm.vm, warm.agent)
            } else {
                // Create VM with networking support
                let mut vm = crate::vm::VmManager::new(sandbox_id.to_string(), config.clone())
                    .map_err(|e| {
                        containerd_shim_protos::ttrpc::Error::Others(format!("VmManager: {e}"))
                    })?;

                vm.prepare().await.map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("prepare: {e}"))
                })?;

                // Configure guest network via kernel boot parameters.
                // CONFIG_IP_PNP + net.ifnames=0 ensures the kernel assigns
                // the pod IP to eth0 at boot — no agent-side config needed.
                if let Some((ref ip_cidr, ref gw)) = ip_config {
                    let parts: Vec<&str> = ip_cidr.split('/').collect();
                    let ip = parts[0];
                    let prefix: u32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
                    let mask = prefix_to_netmask(prefix);
                    let ip_param = format!(" ip={ip}::{gw}:{mask}::eth0:off");
                    vm.append_kernel_args(&ip_param);
                    info!("kernel network: {}", ip_param.trim());
                }

                vm.start_swtpm().await.map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("swtpm: {e}"))
                })?;

                vm.spawn_virtiofsd().map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("virtiofsd: {e}"))
                })?;
                vm.spawn_vmm_in_netns(netns_path.as_deref()).map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("vmm: {e}"))
                })?;

                let (vfsd_r, vmm_r) = tokio::join!(vm.wait_virtiofsd_ready(), vm.wait_vmm_ready(),);
                vfsd_r.map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("virtiofsd: {e}"))
                })?;
                vmm_r.map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("vmm: {e}"))
                })?;

                vm.create_and_boot_vm(tap_name.as_deref(), tap_mac.as_deref())
                    .await
                    .map_err(|e| {
                        containerd_shim_protos::ttrpc::Error::Others(format!("boot: {e}"))
                    })?;

                vm.wait_for_agent().await.map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("agent: {e}"))
                })?;

                let vsock_client = crate::vsock::VsockClient::new(vm.vsock_socket());
                let (agent, _health) = vsock_client.connect_ttrpc().await.map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!("ttrpc: {e}"))
                })?;

                (vm, agent)
            }
        };
        let shared_dir = vm.shared_dir().to_path_buf();
        let vm_id = vm.vm_id().to_string();
        let api_socket_path = vm.api_socket_path().to_path_buf();
        let vsock_socket_path = vm.vsock_socket().to_path_buf();

        self.vms.lock().unwrap().insert(
            vm_id.clone(),
            VmState {
                vm,
                agent,
                container_count: 0,
                shared_dir: shared_dir.clone(),
            },
        );

        // Track the sandbox as a "container" so start/kill/delete work on it
        self.containers.lock().unwrap().insert(
            sandbox_id.to_string(),
            ContainerState {
                vm_id,
                pid: Some(1), // agent is PID 1 inside the VM
                exit_code: None,
                exit_notify: Arc::new(tokio::sync::Notify::new()),
                stdout_shared_path: None,
                stderr_shared_path: None,
                stdout_fifo: None,
                stderr_fifo: None,
                volume_mounts: vec![],
                block_volume_ids: vec![],
            },
        );

        info!("sandbox VM {} ready", sandbox_id);

        // Start memory monitor if hotplug is configured (limit > request)
        if config.hotplug_memory_mb > 0 {
            let boot_bytes = config.default_memory_mb * 1024 * 1024;
            let max_bytes = boot_bytes + config.hotplug_memory_mb * 1024 * 1024;
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let monitor_config = crate::memory::MemoryMonitorConfig {
                boot_memory_bytes: boot_bytes,
                max_memory_bytes: max_bytes,
                api_socket: api_socket_path.clone(),
                vsock_socket: vsock_socket_path.clone(),
                shared_dir: shared_dir.clone(),
            };
            let _monitor = crate::memory::spawn_memory_monitor(monitor_config, shutdown_rx);
            info!(
                "memory monitor started: boot={}MiB max={}MiB",
                config.default_memory_mb,
                config.default_memory_mb + config.hotplug_memory_mb
            );
            // Keep shutdown_tx alive — the monitor stops when the shim process exits.
            // Using mem::forget since the shim is one-process-per-pod.
            std::mem::forget(shutdown_tx);
        }

        let mut resp = api::CreateTaskResponse::new();
        resp.pid = std::process::id();
        Ok(resp)
    }

    /// Create an application container inside an existing sandbox VM.
    async fn create_container(
        &self,
        container_id: &str,
        req: &api::CreateTaskRequest,
    ) -> TtrpcResult<api::CreateTaskResponse> {
        info!("creating app container: {}", container_id);

        // Find the sandbox VM for this container via the sandbox-id annotation
        let sandbox_id = {
            let spec_path = std::path::Path::new(&req.bundle).join("config.json");
            let data = std::fs::read_to_string(&spec_path).map_err(|e| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "failed to read config.json: {e}"
                ))
            })?;
            let spec: serde_json::Value = serde_json::from_str(&data).map_err(|e| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "failed to parse config.json: {e}"
                ))
            })?;
            spec.pointer("/annotations/io.kubernetes.cri.sandbox-id")
                .and_then(|v| v.as_str())
                .unwrap_or(container_id)
                .to_string()
        };

        // Get the sandbox VM's agent and shared dir
        let (agent, shared_dir) = {
            let vms = self.vms.lock().unwrap();
            let vm_state = vms.get(&sandbox_id).ok_or_else(|| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "sandbox VM not found: {sandbox_id}"
                ))
            })?;
            (vm_state.agent.clone(), vm_state.shared_dir.clone())
        };

        // Set up I/O files in the shared directory
        let io_dir = shared_dir.join("io").join(container_id);
        std::fs::create_dir_all(&io_dir).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("failed to create I/O dir: {e}"))
        })?;
        let stdout_host_path = io_dir.join("stdout");
        let stderr_host_path = io_dir.join("stderr");
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

        // Mount the container rootfs from containerd's snapshot.
        let rootfs_path = std::path::Path::new(&req.bundle).join("rootfs");
        std::fs::create_dir_all(&rootfs_path).ok();
        for m in req.rootfs.iter() {
            containerd_shim::mount::mount_rootfs(
                if m.type_.is_empty() {
                    None
                } else {
                    Some(m.type_.as_str())
                },
                if m.source.is_empty() {
                    None
                } else {
                    Some(m.source.as_str())
                },
                &m.options.to_vec(),
                &rootfs_path,
            )
            .map_err(|e| {
                containerd_shim_protos::ttrpc::Error::Others(format!("failed to mount rootfs: {e}"))
            })?;
        }

        // Create an ext4 disk image from the rootfs and hot-plug it into the VM.
        let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
        let disk_path = shared_dir
            .parent()
            .unwrap_or(&shared_dir)
            .join(format!("{}.img", disk_id));

        info!(
            "creating disk image: {} from rootfs {}",
            disk_path.display(),
            rootfs_path.display()
        );

        let bundle_path_str = req.bundle.clone();
        let disk_path_clone = disk_path.clone();
        let rootfs_path_clone = rootfs_path.clone();
        tokio::task::spawn_blocking(move || {
            create_rootfs_disk_image(&bundle_path_str, &rootfs_path_clone, &disk_path_clone)
        })
        .await
        .map_err(|e| containerd_shim_protos::ttrpc::Error::Others(format!("disk image task: {e}")))?
        .map_err(|e| containerd_shim_protos::ttrpc::Error::Others(format!("disk image: {e:#}")))?;

        info!("disk image created: {}", disk_path.display());

        // Hot-plug the disk into the VM
        let disk_path_str = disk_path.to_string_lossy().to_string();
        let api_socket = {
            let vms = self.vms.lock().unwrap();
            let vm_state = vms.get(&sandbox_id).ok_or_else(|| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "sandbox VM not found for disk plug: {sandbox_id}"
                ))
            })?;
            vm_state.vm.api_socket_path().to_path_buf()
        }; // lock released here

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
        let add_disk_resp = crate::vm::VmManager::api_request_to_socket(
            &api_socket,
            "PUT",
            "/api/v1/vm.add-disk",
            Some(&disk_json.to_string()),
        )
        .await
        .map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("hot-plug disk: {e:#}"))
        })?;
        info!("disk hot-plugged: {}", add_disk_resp);

        // Tell the agent to discover the new block device, mount it, and
        // prepare the OCI bundle for crun.
        let bundle_guest = format!("/run/containers/{}", container_id);

        // Extract volume mounts from the OCI spec and stage them.
        // Block volumes are hot-plugged; filesystem volumes are bind-mounted
        // into the shared directory.
        let mut volumes = {
            let spec_path = std::path::Path::new(&req.bundle).join("config.json");
            extract_and_stage_volumes(&spec_path, container_id, &shared_dir)
        };

        // Hot-plug block volumes into the VM and collect cleanup info
        let mut volume_mount_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut block_volume_disk_ids: Vec<String> = Vec::new();

        for vol in &mut volumes {
            if vol.volume_type == cloudhv_proto::VolumeType::BLOCK.into() {
                let vol_disk_id = format!(
                    "vol-{}",
                    &vol.destination.trim_start_matches('/').replace('/', "-")
                );
                let readonly = vol.readonly;
                info!(
                    "hot-plugging block volume: {} -> {} (fs={})",
                    vol.source, vol.destination, vol.fs_type
                );
                crate::vm::VmManager::api_request_to_socket(
                    &api_socket,
                    "PUT",
                    "/api/v1/vm.add-disk",
                    Some(
                        &serde_json::json!({
                            "path": vol.source,
                            "readonly": readonly,
                            "id": vol_disk_id,
                        })
                        .to_string(),
                    ),
                )
                .await
                .map_err(|e| {
                    containerd_shim_protos::ttrpc::Error::Others(format!(
                        "hot-plug block volume {}: {e:#}",
                        vol.destination
                    ))
                })?;
                block_volume_disk_ids.push(vol_disk_id.clone());
                vol.source = vol_disk_id;
            } else {
                // Track filesystem volume bind mount paths for cleanup
                let safe_dest = vol.destination.trim_start_matches('/').replace('/', "_");
                volume_mount_paths.push(
                    shared_dir
                        .join("volumes")
                        .join(container_id)
                        .join(&safe_dest),
                );
            }
        }

        if !volumes.is_empty() {
            info!(
                "staged {} volume(s) for container {}",
                volumes.len(),
                container_id
            );
        }

        // Send CreateContainer RPC to the guest agent
        let mut create_req = cloudhv_proto::CreateContainerRequest::new();
        create_req.container_id = container_id.to_string();
        create_req.bundle_path = bundle_guest;
        create_req.stdout = stdout_guest;
        create_req.stderr = stderr_guest;
        create_req.volumes = volumes;
        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
        let create_resp = agent
            .create_container(ctx, &create_req)
            .await
            .map_err(|e| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "CreateContainer RPC error: {e}"
                ))
            })?;

        // Increment container count on the sandbox VM
        {
            let mut vms = self.vms.lock().unwrap();
            if let Some(vm_state) = vms.get_mut(&sandbox_id) {
                vm_state.container_count += 1;
            }
        }

        self.containers.lock().unwrap().insert(
            container_id.to_string(),
            ContainerState {
                vm_id: sandbox_id,
                pid: Some(create_resp.pid),
                exit_code: None,
                exit_notify: Arc::new(tokio::sync::Notify::new()),
                stdout_shared_path: Some(stdout_host_path),
                stderr_shared_path: Some(stderr_host_path),
                stdout_fifo: if req.stdout.is_empty() {
                    None
                } else {
                    Some(req.stdout.clone())
                },
                stderr_fifo: if req.stderr.is_empty() {
                    None
                } else {
                    Some(req.stderr.clone())
                },
                volume_mounts: volume_mount_paths,
                block_volume_ids: block_volume_disk_ids,
            },
        );

        let mut resp = api::CreateTaskResponse::new();
        resp.pid = create_resp.pid;
        Ok(resp)
    }
}

#[async_trait]
impl Task for CloudHvShim {
    async fn create(
        &self,
        _ctx: &TtrpcContext,
        req: api::CreateTaskRequest,
    ) -> TtrpcResult<api::CreateTaskResponse> {
        let container_id = req.id.clone();
        info!("creating container: {}", container_id);

        // Detect whether this is a sandbox (pause) container or an
        // application container by reading the OCI spec annotations.
        let spec_path = std::path::Path::new(&req.bundle).join("config.json");
        let is_sandbox = if let Ok(data) = std::fs::read_to_string(&spec_path) {
            if let Ok(spec) = serde_json::from_str::<serde_json::Value>(&data) {
                spec.pointer("/annotations/io.kubernetes.cri.container-type")
                    .and_then(|v| v.as_str())
                    == Some("sandbox")
            } else {
                false
            }
        } else {
            false
        };

        if is_sandbox {
            self.create_sandbox(&container_id, &req.bundle).await
        } else {
            self.create_container(&container_id, &req).await
        }
    }

    async fn start(
        &self,
        _ctx: &TtrpcContext,
        req: api::StartRequest,
    ) -> TtrpcResult<api::StartResponse> {
        let container_id = &req.id;
        info!("starting container: {}", container_id);

        // Check if this is the sandbox — if so, the VM is already running
        let is_sandbox = {
            let vms = self.vms.lock().unwrap();
            vms.contains_key(container_id)
        };

        let mut resp = api::StartResponse::new();
        if is_sandbox {
            resp.pid = std::process::id();
        } else {
            let agent = self.get_agent_for_container(container_id)?;
            let mut start_req = cloudhv_proto::StartContainerRequest::new();
            start_req.container_id = container_id.to_string();
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
            let start_resp = agent.start_container(ctx, &start_req).await.map_err(|e| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "StartContainer RPC error: {e}"
                ))
            })?;
            resp.pid = start_resp.pid;

            // Forward container I/O: shared dir files → containerd FIFOs
            let (shared_stdout, shared_stderr, fifo_stdout, fifo_stderr) = {
                let containers = self.containers.lock().unwrap();
                if let Some(state) = containers.get(container_id) {
                    (
                        state.stdout_shared_path.clone(),
                        state.stderr_shared_path.clone(),
                        state.stdout_fifo.clone(),
                        state.stderr_fifo.clone(),
                    )
                } else {
                    (None, None, None, None)
                }
            };
            if let (Some(src), Some(dst)) = (shared_stdout, fifo_stdout) {
                tokio::spawn(forward_output(src, dst));
            }
            if let (Some(src), Some(dst)) = (shared_stderr, fifo_stderr) {
                tokio::spawn(forward_output(src, dst));
            }
        }
        Ok(resp)
    }

    async fn kill(&self, _ctx: &TtrpcContext, req: api::KillRequest) -> TtrpcResult<api::Empty> {
        let container_id = &req.id;
        info!("container signal: {} signal={}", container_id, req.signal);

        // For sandbox: mark all containers in this VM as stopped, then
        // signal shim exit so sandbox wait() returns
        let is_sandbox = {
            let vms = self.vms.lock().unwrap();
            vms.contains_key(container_id)
        };
        if is_sandbox {
            // Mark all containers associated with this sandbox as stopped
            let mut containers = self.containers.lock().unwrap();
            for state in containers.values_mut() {
                if state.vm_id == *container_id && state.exit_code.is_none() {
                    state.exit_code = Some(137);
                }
                state.exit_notify.notify_waiters();
            }
            // Mark the sandbox container itself
            if let Some(state) = containers.get_mut(container_id) {
                if state.exit_code.is_none() {
                    state.exit_code = Some(0);
                }
                state.exit_notify.notify_waiters();
            }
            self.exit.signal();
            return Ok(api::Empty::new());
        }

        // For app containers: mark stopped first, then best-effort agent RPC.
        // Setting exit_code before the agent RPC ensures wait() returns
        // immediately via the Notify.
        {
            let mut containers = self.containers.lock().unwrap();
            if let Some(state) = containers.get_mut(container_id) {
                if state.exit_code.is_none() {
                    state.exit_code = Some(137);
                }
                state.exit_notify.notify_waiters();
            }
        }

        // Best-effort agent RPC — fire and forget since exit_code is
        // already set and wait() already unblocked.
        if let Ok(agent) = self.get_agent_for_container(container_id) {
            let cid = container_id.to_string();
            let signal = req.signal;
            let all = req.all;
            tokio::spawn(async move {
                let mut kreq = cloudhv_proto::KillContainerRequest::new();
                kreq.container_id = cid;
                kreq.signal = signal;
                kreq.all = all;
                let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
                let _ = agent.kill_container(ctx, &kreq).await;
            });
        }

        Ok(api::Empty::new())
    }

    async fn delete(
        &self,
        _ctx: &TtrpcContext,
        req: api::DeleteRequest,
    ) -> TtrpcResult<api::DeleteResponse> {
        let container_id = &req.id;
        info!("deleting container: {}", container_id);

        let agent = self.get_agent_for_container(container_id).ok();

        let (pid, exit_status) = if let Some(agent) = agent {
            let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
            del_req.container_id = container_id.to_string();
            let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(10));
            match agent.delete_container(ctx, &del_req).await {
                Ok(r) => (r.pid, r.exit_status),
                Err(e) => {
                    info!("delete RPC failed (may be expected): {e}");
                    (0, 0)
                }
            }
        } else {
            (0, 0)
        };

        // Remove container and clean up its resources
        let removed_state = {
            let mut containers = self.containers.lock().unwrap();
            containers.remove(container_id)
        };
        let vm_id = removed_state.as_ref().map(|s| s.vm_id.clone());

        if let (Some(state), Some(ref vm_id)) = (&removed_state, &vm_id) {
            // Unmount bind-mounted filesystem volumes
            for mount_path in &state.volume_mounts {
                let _ = std::process::Command::new("umount")
                    .arg(mount_path.to_string_lossy().to_string())
                    .status();
                let _ = std::fs::remove_dir_all(mount_path);
            }

            // Remove hot-plugged block volumes
            let api_socket = {
                let vms = self.vms.lock().unwrap();
                vms.get(vm_id).map(|s| {
                    s.shared_dir
                        .parent()
                        .unwrap_or(&s.shared_dir)
                        .join("api.sock")
                })
            };
            if let Some(api_socket) = api_socket {
                for disk_id in &state.block_volume_ids {
                    let body = format!(r#"{{"id":"{disk_id}"}}"#);
                    let _ = crate::vm::VmManager::api_request_to_socket(
                        &api_socket,
                        "PUT",
                        "/api/v1/vm.remove-device",
                        Some(&body),
                    )
                    .await;
                    info!("removed block volume: {}", disk_id);
                }
            }

            // Clean up the container's volume directory
            let shared_dir = {
                let vms = self.vms.lock().unwrap();
                vms.get(vm_id).map(|s| s.shared_dir.clone())
            };
            if let Some(shared_dir) = shared_dir {
                let vol_dir = shared_dir.join("volumes").join(container_id);
                let _ = std::fs::remove_dir_all(&vol_dir);
            }
        }

        if let Some(vm_id) = vm_id {
            // Clean up the disk image for this container
            let state_dir = {
                let vms = self.vms.lock().unwrap();
                vms.get(&vm_id)
                    .map(|s| s.shared_dir.parent().unwrap_or(&s.shared_dir).to_path_buf())
            };
            if let Some(state_dir) = state_dir {
                let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
                let disk_img = state_dir.join(format!("{disk_id}.img"));
                match std::fs::remove_file(&disk_img) {
                    Ok(()) => info!("removed disk image: {}", disk_img.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => info!("failed to remove disk image: {e}"),
                }
            }

            let should_cleanup = {
                let mut vms = self.vms.lock().unwrap();
                if let Some(vm_state) = vms.get_mut(&vm_id) {
                    vm_state.container_count = vm_state.container_count.saturating_sub(1);
                    vm_state.container_count == 0
                } else {
                    false
                }
            };
            if should_cleanup {
                let removed_vm = {
                    let mut vms = self.vms.lock().unwrap();
                    vms.remove(&vm_id)
                };
                if let Some(mut vm_state) = removed_vm {
                    let _ = vm_state.vm.cleanup().await;
                }
            }
        }

        let mut resp = api::DeleteResponse::new();
        resp.pid = pid;
        resp.exit_status = exit_status;
        resp.exited_at = timestamp_now();
        Ok(resp)
    }

    async fn wait(
        &self,
        _ctx: &TtrpcContext,
        req: api::WaitRequest,
    ) -> TtrpcResult<api::WaitResponse> {
        let container_id = &req.id;
        info!("waiting for container: {}", container_id);

        // For sandbox, wait until shim exit is signaled
        let is_sandbox = {
            let vms = self.vms.lock().unwrap();
            vms.contains_key(container_id)
        };
        if is_sandbox {
            self.exit.wait().await;
            let mut resp = api::WaitResponse::new();
            resp.exit_status = 0;
            resp.exited_at = timestamp_now();
            return Ok(resp);
        }

        // Clone the Notify and register the waiter while holding the lock.
        // This prevents a race where kill() calls notify_waiters() between
        // our lock release and our notified() registration.
        let exit_notify = {
            let containers = self.containers.lock().unwrap();
            if let Some(state) = containers.get(container_id) {
                if let Some(exit_code) = state.exit_code {
                    info!("container {} already exited: {}", container_id, exit_code);
                    let mut resp = api::WaitResponse::new();
                    resp.exit_status = exit_code;
                    resp.exited_at = timestamp_now();
                    return Ok(resp);
                }
                state.exit_notify.clone()
            } else {
                let mut resp = api::WaitResponse::new();
                resp.exit_status = 0;
                resp.exited_at = timestamp_now();
                return Ok(resp);
            }
        };
        // Register the waiter outside the lock — the clone ensures the
        // Notify outlives the MutexGuard. Any notify_waiters() call after
        // our lock release will wake this future.
        let notified_future = exit_notify.notified();

        // Wait for either:
        // 1. kill() sets exit_code and notifies (in-process Notify)
        // 2. shim exit signal (sandbox killed via ExitSignal)
        tokio::select! {
            _ = notified_future => {},
            _ = self.exit.wait() => {},
        }

        let exit_code = {
            let containers = self.containers.lock().unwrap();
            containers
                .get(container_id)
                .and_then(|s| s.exit_code)
                .unwrap_or(137)
        };
        info!("container {} exited: {}", container_id, exit_code);
        let mut resp = api::WaitResponse::new();
        resp.exit_status = exit_code;
        resp.exited_at = timestamp_now();
        Ok(resp)
    }

    async fn state(
        &self,
        _ctx: &TtrpcContext,
        req: api::StateRequest,
    ) -> TtrpcResult<api::StateResponse> {
        let container_id = &req.id;

        let containers = self.containers.lock().unwrap();
        let mut resp = api::StateResponse::new();
        resp.id = container_id.clone();

        if let Some(state) = containers.get(container_id) {
            resp.pid = state.pid.unwrap_or(0);
            if state.exit_code.is_some() {
                resp.status = api::Status::STOPPED.into();
                resp.exit_status = state.exit_code.unwrap_or(0);
            } else {
                resp.status = api::Status::RUNNING.into();
            }
        } else {
            // Container not found — treat as stopped
            resp.status = api::Status::STOPPED.into();
        }

        info!(
            "state query: id={} status={:?} exit={}",
            container_id, resp.status, resp.exit_status
        );
        Ok(resp)
    }

    async fn connect(
        &self,
        _ctx: &TtrpcContext,
        _req: api::ConnectRequest,
    ) -> TtrpcResult<api::ConnectResponse> {
        let mut resp = api::ConnectResponse::new();
        resp.version = env!("CARGO_PKG_VERSION").to_string();
        Ok(resp)
    }

    async fn shutdown(
        &self,
        _ctx: &TtrpcContext,
        _req: api::ShutdownRequest,
    ) -> TtrpcResult<api::Empty> {
        info!("shutdown requested");
        self.exit.signal();
        Ok(api::Empty::new())
    }
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

/// Forward container output from a shared dir file to a containerd FIFO.
///
/// The agent writes crun's stdout/stderr to files in the virtio-fs shared
/// directory. This task tails the file and writes to containerd's FIFO so
/// that `crictl logs` and `kubectl logs` work.
async fn forward_output(src: std::path::PathBuf, dst: String) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Wait for the source file to appear (agent creates it on start)
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

    // Open the containerd FIFO for writing (this blocks until a reader connects)
    let dst_file = match tokio::fs::OpenOptions::new().write(true).open(&dst).await {
        Ok(f) => f,
        Err(e) => {
            info!("I/O forward: can't open FIFO {}: {e}", dst);
            return;
        }
    };

    let mut reader = BufReader::new(src_file);
    let mut writer = dst_file;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF — file may still be written to, poll
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                // Check if source file has grown
                match tokio::fs::metadata(&src).await {
                    Ok(_) => continue,
                    Err(_) => break, // File removed, stop
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

/// Extract volume mounts from the OCI spec and stage them for the guest.
///
/// Dual-path transport:
/// - **Block devices** (raw block PVCs): hot-plugged via vm.add-disk for
///   direct virtio-blk access (no FUSE overhead)
/// - **Filesystem volumes** (ConfigMaps, Secrets, emptyDirs, fs PVCs):
///   bind-mounted into the virtio-fs shared dir for live host access
fn extract_and_stage_volumes(
    spec_path: &std::path::Path,
    container_id: &str,
    shared_dir: &std::path::Path,
) -> Vec<cloudhv_proto::VolumeMount> {
    let data = match std::fs::read_to_string(spec_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let spec: serde_json::Value = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let mounts = match spec.get("mounts").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return vec![],
    };

    let skip_destinations = [
        "/proc",
        "/dev",
        "/dev/pts",
        "/dev/shm",
        "/dev/mqueue",
        "/sys",
        "/sys/fs/cgroup",
        "/etc/hostname",
        "/etc/hosts",
        "/etc/resolv.conf",
        "/dev/termination-log",
    ];

    let mut volumes = Vec::new();

    for mount in mounts {
        let dest = match mount.get("destination").and_then(|d| d.as_str()) {
            Some(d) => d,
            None => continue,
        };
        let source = match mount.get("source").and_then(|s| s.as_str()) {
            Some(s) => s,
            None => continue,
        };

        if skip_destinations.contains(&dest) || dest.starts_with("/var/run/secrets") {
            continue;
        }

        let src_path = std::path::Path::new(source);
        if !src_path.exists() {
            continue;
        }

        let options = mount
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        let readonly = options.contains(&"ro");

        // Detect block devices vs filesystem paths
        if is_block_device(src_path) {
            // Block device — will be hot-plugged via vm.add-disk.
            // The agent discovers it as a new /dev/vdX and mounts it.
            let mut vol_msg = cloudhv_proto::VolumeMount::new();
            vol_msg.destination = dest.to_string();
            vol_msg.source = source.to_string();
            vol_msg.readonly = readonly;
            vol_msg.volume_type = cloudhv_proto::VolumeType::BLOCK.into();
            vol_msg.fs_type = detect_fs_type(source).unwrap_or_else(|| "ext4".to_string());
            vol_msg.options = options.iter().map(|s| s.to_string()).collect();
            log::info!(
                "block volume: {} -> {} (fs={})",
                source,
                dest,
                vol_msg.fs_type
            );
            volumes.push(vol_msg);
        } else {
            // Filesystem path — bind-mount into the shared directory so
            // the guest sees a live view via virtio-fs.
            let mount_type = mount.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let is_bind =
                mount_type == "bind" || options.iter().any(|o| *o == "bind" || *o == "rbind");
            if !is_bind {
                continue;
            }

            let safe_dest = dest.trim_start_matches('/').replace('/', "_");
            let vol_dir = shared_dir
                .join("volumes")
                .join(container_id)
                .join(&safe_dest);

            if let Err(e) = bind_mount_volume(src_path, &vol_dir) {
                log::warn!(
                    "failed to bind-mount volume {} -> {}: {e}",
                    source,
                    vol_dir.display()
                );
                continue;
            }

            let guest_source = format!(
                "{}/volumes/{}/{}",
                cloudhv_common::VIRTIOFS_GUEST_MOUNT,
                container_id,
                safe_dest
            );

            let mut vol_msg = cloudhv_proto::VolumeMount::new();
            vol_msg.destination = dest.to_string();
            vol_msg.source = guest_source;
            vol_msg.readonly = readonly;
            vol_msg.volume_type = cloudhv_proto::VolumeType::FILESYSTEM.into();
            vol_msg.options = options.iter().map(|s| s.to_string()).collect();
            volumes.push(vol_msg);
            log::info!("fs volume: {} -> {} (ro={})", source, dest, readonly);
        }
    }

    volumes
}

/// Check if a path is a block device.
fn is_block_device(path: &std::path::Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.file_type().is_block_device(),
        Err(_) => false,
    }
}

/// Detect the filesystem type of a block device via blkid.
fn detect_fs_type(device: &str) -> Option<String> {
    let output = std::process::Command::new("blkid")
        .args(["-s", "TYPE", "-o", "value", device])
        .output()
        .ok()?;
    if output.status.success() {
        let fs = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !fs.is_empty() {
            return Some(fs);
        }
    }
    None
}

/// Bind-mount a host path into the shared directory.
fn bind_mount_volume(source: &std::path::Path, target: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(target)?;
    let status = std::process::Command::new("mount")
        .args([
            "--bind",
            &source.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("mount --bind failed: {status}");
    }
    Ok(())
}
