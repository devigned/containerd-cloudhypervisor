//! In-process virtiofsd backend using the virtiofsd library crate.
//!
//! Instead of spawning virtiofsd as a separate process (~5MB RSS per VM),
//! this module runs the vhost-user-fs backend as a thread within the shim
//! process. Cloud Hypervisor connects to the same Unix socket.
//!
//! Enabled via the `embedded-virtiofsd` feature flag.

#[cfg(all(target_os = "linux", feature = "embedded-virtiofsd"))]
mod inner {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use log::{error, info};

    use vhost::vhost_user::Listener;
    use vhost_user_backend::VhostUserDaemon;
    use virtiofsd::passthrough::{self, PassthroughFs};
    use virtiofsd::vhost_user::VhostUserFsBackendBuilder;
    use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

    /// Handle to an in-process virtiofsd backend.
    ///
    /// The backend runs on a dedicated OS thread (not a tokio task, since
    /// the vhost-user daemon uses blocking I/O). Dropping the handle
    /// signals the thread to stop.
    pub struct VirtiofsBackend {
        socket_path: PathBuf,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl VirtiofsBackend {
        /// Start an in-process virtiofsd serving `shared_dir` on `socket_path`.
        ///
        /// The backend runs on a dedicated OS thread. The socket is created
        /// and ready for CH to connect when this function returns.
        pub fn start(socket_path: &Path, shared_dir: &Path) -> Result<Self> {
            let socket_path = socket_path.to_path_buf();
            let shared_dir_str = shared_dir.to_string_lossy().to_string();

            // Remove any stale socket
            let _ = std::fs::remove_file(&socket_path);

            // Create the listener before spawning the thread so we can
            // verify it's ready immediately.
            let socket_str = socket_path.to_string_lossy().to_string();
            let listener = Listener::new(&socket_str, true)
                .with_context(|| format!("create virtiofsd listener on {socket_str}"))?;

            let sp = socket_path.clone();
            let thread = std::thread::Builder::new()
                .name("virtiofsd".to_string())
                .spawn(move || {
                    if let Err(e) = run_virtiofsd(listener, &shared_dir_str) {
                        error!("in-process virtiofsd failed: {e:#}");
                    }
                    info!("in-process virtiofsd stopped (socket={})", sp.display());
                })
                .context("spawn virtiofsd thread")?;

            info!(
                "in-process virtiofsd started: socket={} shared_dir={}",
                socket_path.display(),
                shared_dir.display()
            );

            Ok(Self {
                socket_path,
                thread: Some(thread),
            })
        }
    }

    impl Drop for VirtiofsBackend {
        fn drop(&mut self) {
            // Remove the socket file. This won't stop an already-connected
            // daemon (it's blocked in daemon.wait()), but ensures no new
            // connections can be made. The daemon thread exits when CH
            // (the vhost-user client) disconnects or is killed — which
            // happens in VmManager's Drop/shutdown before this runs.
            let _ = std::fs::remove_file(&self.socket_path);

            if let Some(thread) = self.thread.take() {
                match thread.join() {
                    Ok(_) => {}
                    Err(panic_payload) => {
                        error!(
                            "virtiofsd thread panicked: {:?}",
                            panic_payload
                                .downcast_ref::<String>()
                                .map(|s| s.as_str())
                                .or_else(|| panic_payload.downcast_ref::<&str>().copied())
                                .unwrap_or("unknown panic")
                        );
                    }
                }
            }
        }
    }

    /// Run the virtiofsd daemon on the current thread (blocks until done).
    fn run_virtiofsd(listener: Listener, shared_dir: &str) -> Result<()> {
        let fs_cfg = passthrough::Config {
            root_dir: shared_dir.to_string(),
            cache_policy: passthrough::CachePolicy::Never,
            // Minimal config matching our spawned virtiofsd flags:
            // --cache=never --sandbox=none
            ..Default::default()
        };

        let fs = PassthroughFs::new(fs_cfg).context("create PassthroughFs")?;

        let fs_backend = Arc::new(
            VhostUserFsBackendBuilder::default()
                .set_thread_pool_size(1)
                .build(fs)
                .context("create vhost-user-fs backend")?,
        );

        let mut daemon = VhostUserDaemon::new(
            String::from("virtiofsd-embedded"),
            fs_backend,
            GuestMemoryAtomic::new(GuestMemoryMmap::new()),
        )
        .map_err(|e| anyhow::anyhow!("create VhostUserDaemon: {e}"))?;

        daemon
            .start(listener)
            .map_err(|e| anyhow::anyhow!("start VhostUserDaemon: {e:?}"))?;

        info!("in-process virtiofsd: client connected, serving requests");

        // Block until the client (CH) disconnects
        let _ = daemon.wait();

        Ok(())
    }
}

// Re-export the backend when the feature is enabled
#[cfg(all(target_os = "linux", feature = "embedded-virtiofsd"))]
pub use inner::VirtiofsBackend;
