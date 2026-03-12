// Suppress removed lints from ttrpc-codegen generated code
#![allow(unknown_lints)]

use log::{error, info, warn};
use nix::unistd::getpid;

// Generated ttrpc/protobuf code from agent.proto
#[allow(clippy::all)]
#[allow(non_snake_case)]
#[allow(non_camel_case_types)]
#[path = "generated"]
mod proto {
    pub mod agent;
    pub mod agent_ttrpc;
}

mod container;
mod mount;
mod pressure;
mod reaper;
mod server;

/// The guest agent runs as PID 1 (init) inside the Cloud Hypervisor VM.
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let pid = getpid();
    info!(
        "cloudhv-agent starting (version {}, pid={})",
        env!("CARGO_PKG_VERSION"),
        pid
    );

    if pid.as_raw() == 1 {
        info!("running as PID 1 (init)");
        init_setup();
    } else {
        info!("running as non-init process (pid={})", pid);
        reaper::set_child_subreaper();
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    rt.block_on(async {
        if let Err(e) = run_agent().await {
            error!("agent failed: {}", e);
            std::process::exit(1);
        }
    });
}

/// PID 1 init setup: mount filesystems and configure the system.
fn init_setup() {
    if let Err(e) = mount::mount_essential_filesystems() {
        error!("failed to mount essential filesystems: {}", e);
    }

    if let Err(e) = mount::mount_virtiofs() {
        warn!(
            "failed to mount virtio-fs (may not be available yet): {}",
            e
        );
    }

    reaper::set_child_subreaper();

    // Start PSI memory pressure watcher — writes to shared dir on pressure events
    pressure::start_pressure_watcher(std::path::Path::new(cloudhv_common::VIRTIOFS_GUEST_MOUNT));

    info!("init setup complete");
}

async fn run_agent() -> anyhow::Result<()> {
    // NOTE: No orphan reaper — waitpid(-1) interferes with crun's internal
    // child management (crun forks twice, and reaping its intermediate children
    // causes it to hang). As PID 1 with PR_SET_CHILD_SUBREAPER, zombies will
    // accumulate but this is acceptable in a short-lived VM.

    let server = server::AgentServer::new();
    server.run().await
}
