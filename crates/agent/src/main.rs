use log::{error, info, warn};
use nix::unistd::getpid;

mod container;
mod mount;
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
    install_sigchld_handler();
    info!("init setup complete");
}

#[cfg(target_os = "linux")]
fn install_sigchld_handler() {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    unsafe {
        let action = SigAction::new(
            SigHandler::Handler(reaper::sigchld_handler),
            SaFlags::SA_NOCLDSTOP | SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        if let Err(e) = sigaction(Signal::SIGCHLD, &action) {
            error!("failed to install SIGCHLD handler: {}", e);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn install_sigchld_handler() {
    info!("SIGCHLD handler: no-op on non-Linux");
}

async fn run_agent() -> anyhow::Result<()> {
    let server = server::AgentServer::new();
    server.run().await
}
