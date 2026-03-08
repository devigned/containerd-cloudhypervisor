use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use containerd_shim::api;
use containerd_shim::asynchronous::{spawn, ExitSignal, Shim};
use containerd_shim::{Config, Error, Flags, StartOpts, TtrpcResult};
use containerd_shim_protos::shim_async::Task;
use containerd_shim_protos::ttrpc::r#async::TtrpcContext;
use log::{debug, info};

use crate::config::load_config;
use crate::vm::VmManager;
use crate::vsock::VsockClient;

/// A running VM that may host multiple containers.
struct VmState {
    vm: VmManager,
    agent: cloudhv_proto::AgentServiceClient,
    container_count: usize,
}

/// Per-container state tracked by the shim.
struct ContainerState {
    vm_id: String,
    pid: Option<u32>,
    exit_code: Option<u32>,
    exited_at: Option<chrono::DateTime<Utc>>,
    stdout_path: Option<std::path::PathBuf>,
    stderr_path: Option<std::path::PathBuf>,
}

/// The Cloud Hypervisor containerd shim implementation.
#[derive(Clone)]
pub struct CloudHvShim {
    exit: Arc<ExitSignal>,
    /// Active VMs keyed by VM ID.
    vms: Arc<Mutex<HashMap<String, VmState>>>,
    /// Containers keyed by container ID, referencing their VM.
    containers: Arc<Mutex<HashMap<String, ContainerState>>>,
}

#[async_trait]
impl Shim for CloudHvShim {
    type T = CloudHvShim;

    async fn new(_runtime_id: &str, _args: &Flags, _config: &mut Config) -> Self {
        CloudHvShim {
            exit: Arc::new(ExitSignal::default()),
            vms: Arc::new(Mutex::new(HashMap::new())),
            containers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn start_shim(&mut self, opts: StartOpts) -> Result<String, Error> {
        let address = spawn(opts, "", Vec::new()).await?;
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

        let config = load_config(None).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("config error: {e}"))
        })?;

        // For now, each container gets its own VM. The vm_id matches the container_id.
        // Future: support VM reuse by looking up an existing VM first.
        let vm_id = container_id.clone();

        let mut vm = VmManager::new(vm_id.clone(), config).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VM creation error: {e}"))
        })?;

        vm.prepare().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VM prepare error: {e}"))
        })?;

        vm.start_virtiofsd().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("virtiofsd error: {e}"))
        })?;

        vm.start_vmm().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VMM start error: {e}"))
        })?;

        vm.create_and_boot_vm().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VM boot error: {e}"))
        })?;

        vm.wait_for_agent().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("agent not ready: {e}"))
        })?;

        let vsock_client = VsockClient::new(vm.vsock_socket());
        let (agent, _health) = vsock_client.connect_ttrpc().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("ttrpc connect error: {e}"))
        })?;

        // Set up I/O files in the virtio-fs shared directory
        let io_dir = vm.shared_dir().join("io").join(&container_id);
        std::fs::create_dir_all(&io_dir).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("failed to create I/O dir: {e}"))
        })?;
        let stdout_host_path = io_dir.join("stdout");
        let stderr_host_path = io_dir.join("stderr");
        let stdout_guest_path = format!(
            "{}/io/{}/stdout",
            cloudhv_common::VIRTIOFS_GUEST_MOUNT,
            container_id
        );
        let stderr_guest_path = format!(
            "{}/io/{}/stderr",
            cloudhv_common::VIRTIOFS_GUEST_MOUNT,
            container_id
        );

        // Send CreateContainer RPC to the guest agent
        let mut create_req = cloudhv_proto::CreateContainerRequest::new();
        create_req.container_id = container_id.clone();
        create_req.bundle_path = req.bundle.clone();
        create_req.stdout = stdout_guest_path;
        create_req.stderr = stderr_guest_path;
        let ctx = ttrpc::context::with_timeout(30);
        let create_resp = agent
            .create_container(ctx, &create_req)
            .await
            .map_err(|e| {
                containerd_shim_protos::ttrpc::Error::Others(format!(
                    "CreateContainer RPC error: {e}"
                ))
            })?;

        // Store VM state
        self.vms.lock().unwrap().insert(
            vm_id.clone(),
            VmState {
                vm,
                agent,
                container_count: 1,
            },
        );

        // Store container state
        self.containers.lock().unwrap().insert(
            container_id.clone(),
            ContainerState {
                vm_id,
                pid: Some(create_resp.pid),
                exit_code: None,
                exited_at: None,
                stdout_path: Some(stdout_host_path),
                stderr_path: Some(stderr_host_path),
            },
        );

        let mut resp = api::CreateTaskResponse::new();
        resp.pid = create_resp.pid;
        Ok(resp)
    }

    async fn start(
        &self,
        _ctx: &TtrpcContext,
        req: api::StartRequest,
    ) -> TtrpcResult<api::StartResponse> {
        let container_id = &req.id;
        info!("starting container: {}", container_id);

        let agent = self.get_agent_for_container(container_id)?;

        let mut start_req = cloudhv_proto::StartContainerRequest::new();
        start_req.container_id = container_id.to_string();
        let ctx = ttrpc::context::with_timeout(30);
        let start_resp = agent.start_container(ctx, &start_req).await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("StartContainer RPC error: {e}"))
        })?;

        let mut resp = api::StartResponse::new();
        resp.pid = start_resp.pid;
        Ok(resp)
    }

    async fn kill(&self, _ctx: &TtrpcContext, req: api::KillRequest) -> TtrpcResult<api::Empty> {
        let container_id = &req.id;
        info!("killing container: {} signal={}", container_id, req.signal);

        let agent = self.get_agent_for_container(container_id)?;

        let mut kreq = cloudhv_proto::KillContainerRequest::new();
        kreq.container_id = container_id.to_string();
        kreq.signal = req.signal;
        kreq.all = req.all;
        let ctx = ttrpc::context::with_timeout(10);
        agent.kill_container(ctx, &kreq).await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("KillContainer RPC error: {e}"))
        })?;

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
            let ctx = ttrpc::context::with_timeout(10);
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

        // Remove container and clean up its VM if no other containers use it
        let vm_id = {
            let mut containers = self.containers.lock().unwrap();
            containers.remove(container_id).map(|s| s.vm_id)
        };
        if let Some(vm_id) = vm_id {
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
        Ok(resp)
    }

    async fn wait(
        &self,
        _ctx: &TtrpcContext,
        req: api::WaitRequest,
    ) -> TtrpcResult<api::WaitResponse> {
        let container_id = &req.id;
        info!("waiting for container: {}", container_id);

        let agent = self.get_agent_for_container(container_id)?;

        let mut wait_req = cloudhv_proto::WaitContainerRequest::new();
        wait_req.container_id = container_id.to_string();
        let ctx = ttrpc::context::with_timeout(300);
        let wait_resp = agent.wait_container(ctx, &wait_req).await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("WaitContainer RPC error: {e}"))
        })?;

        let mut resp = api::WaitResponse::new();
        resp.exit_status = wait_resp.exit_status;
        Ok(resp)
    }

    async fn state(
        &self,
        _ctx: &TtrpcContext,
        req: api::StateRequest,
    ) -> TtrpcResult<api::StateResponse> {
        let container_id = &req.id;
        debug!("state query for container: {}", container_id);

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
        }

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
