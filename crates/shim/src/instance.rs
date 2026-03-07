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

/// Container state tracked by the shim.
struct ContainerState {
    vm: VmManager,
    vsock_client: VsockClient,
    pid: Option<u32>,
    exit_code: Option<u32>,
    exited_at: Option<chrono::DateTime<Utc>>,
}

/// The Cloud Hypervisor containerd shim implementation.
#[derive(Clone)]
pub struct CloudHvShim {
    exit: Arc<ExitSignal>,
    containers: Arc<Mutex<HashMap<String, ContainerState>>>,
}

#[async_trait]
impl Shim for CloudHvShim {
    type T = CloudHvShim;

    async fn new(_runtime_id: &str, _args: &Flags, _config: &mut Config) -> Self {
        CloudHvShim {
            exit: Arc::new(ExitSignal::default()),
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
#[async_trait]
impl Task for CloudHvShim {
    async fn create(
        &self,
        _ctx: &TtrpcContext,
        req: api::CreateTaskRequest,
    ) -> TtrpcResult<api::CreateTaskResponse> {
        let container_id = req.id.clone();
        info!("creating container: {}", container_id);

        // Load runtime config
        let config = load_config(None).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("config error: {e}"))
        })?;

        // Create and prepare VM
        let mut vm = VmManager::new(container_id.clone(), config).map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VM creation error: {e}"))
        })?;

        vm.prepare().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VM prepare error: {e}"))
        })?;

        // Start virtiofsd
        vm.start_virtiofsd().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("virtiofsd error: {e}"))
        })?;

        // Start Cloud Hypervisor VMM
        vm.start_vmm().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VMM start error: {e}"))
        })?;

        // Create and boot the VM
        vm.create_and_boot_vm().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("VM boot error: {e}"))
        })?;

        // Wait for the guest agent to be ready
        vm.wait_for_agent().await.map_err(|e| {
            containerd_shim_protos::ttrpc::Error::Others(format!("agent not ready: {e}"))
        })?;

        let vsock_client = VsockClient::new(vm.vsock_socket());

        // TODO: Send CreateContainer RPC to the guest agent
        let state = ContainerState {
            vm,
            vsock_client,
            pid: Some(1), // Placeholder: guest init PID
            exit_code: None,
            exited_at: None,
        };

        self.containers
            .lock()
            .unwrap()
            .insert(container_id.clone(), state);

        let mut resp = api::CreateTaskResponse::new();
        resp.pid = 1;
        Ok(resp)
    }

    async fn start(
        &self,
        _ctx: &TtrpcContext,
        req: api::StartRequest,
    ) -> TtrpcResult<api::StartResponse> {
        let container_id = &req.id;
        info!("starting container: {}", container_id);

        // TODO: Send StartContainer RPC to guest agent
        let mut resp = api::StartResponse::new();
        resp.pid = 1;
        Ok(resp)
    }

    async fn kill(
        &self,
        _ctx: &TtrpcContext,
        req: api::KillRequest,
    ) -> TtrpcResult<api::Empty> {
        let container_id = &req.id;
        info!("killing container: {} signal={}", container_id, req.signal);

        // TODO: Send KillContainer RPC to guest agent
        Ok(api::Empty::new())
    }

    async fn delete(
        &self,
        _ctx: &TtrpcContext,
        req: api::DeleteRequest,
    ) -> TtrpcResult<api::DeleteResponse> {
        let container_id = &req.id;
        info!("deleting container: {}", container_id);

        let removed = {
            let mut containers = self.containers.lock().unwrap();
            containers.remove(container_id)
        };

        if let Some(mut state) = removed {
            let _ = state.vm.cleanup().await;
        }

        let mut resp = api::DeleteResponse::new();
        resp.pid = 1;
        resp.exit_status = 0;
        Ok(resp)
    }

    async fn wait(
        &self,
        _ctx: &TtrpcContext,
        req: api::WaitRequest,
    ) -> TtrpcResult<api::WaitResponse> {
        let container_id = &req.id;
        info!("waiting for container: {}", container_id);

        // TODO: Wait for container exit via guest agent WaitContainer RPC
        let resp = api::WaitResponse::new();
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
