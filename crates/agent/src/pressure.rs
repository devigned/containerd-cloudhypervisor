//! PSI (Pressure Stall Information) memory pressure watcher.
//!
//! Monitors `/proc/pressure/memory` using Linux's PSI trigger mechanism.
//! When memory pressure exceeds the threshold, writes a signal file to
//! the virtio-fs shared directory so the host shim can react immediately
//! by resizing the VM's memory.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsFd;
use std::path::Path;

use log::{debug, error, info, warn};

/// PSI trigger: 100ms of *some* memory stall in any 1-second window.
/// "some" means at least one task was delayed waiting for memory.
const PSI_TRIGGER: &str = "some 100000 1000000";

/// Well-known signal file name in the shared directory.
pub const PRESSURE_SIGNAL_FILE: &str = "memory-pressure";

/// Start the PSI memory pressure watcher on a dedicated OS thread.
///
/// When pressure is detected, writes current meminfo to
/// `<shared_dir>/memory-pressure`. The host shim watches this file
/// and triggers an immediate vm.resize.
///
/// Falls back gracefully if PSI is not available (older kernels).
pub fn start_pressure_watcher(shared_dir: &Path) {
    let shared_dir = shared_dir.to_path_buf();

    std::thread::Builder::new()
        .name("psi-watcher".to_string())
        .spawn(move || {
            if let Err(e) = run_pressure_watcher(&shared_dir) {
                warn!("PSI watcher exited: {e}");
            }
        })
        .expect("spawn PSI watcher thread");

    info!("PSI memory pressure watcher started");
}

fn run_pressure_watcher(shared_dir: &Path) -> anyhow::Result<()> {
    let psi_path = Path::new("/proc/pressure/memory");
    if !psi_path.exists() {
        info!("PSI not available (/proc/pressure/memory missing), skipping pressure watcher");
        return Ok(());
    }

    // Open for read+write to register the trigger
    let mut file = OpenOptions::new().read(true).write(true).open(psi_path)?;

    // Write the trigger threshold
    file.write_all(PSI_TRIGGER.as_bytes())?;
    info!("PSI trigger registered: {}", PSI_TRIGGER);

    let mut poll_fds = [nix::poll::PollFd::new(
        file.as_fd(),
        nix::poll::PollFlags::POLLPRI,
    )];

    loop {
        // Block until pressure threshold is breached
        match nix::poll::poll(&mut poll_fds, nix::poll::PollTimeout::NONE) {
            Ok(n) if n > 0 => {
                if let Some(revents) = poll_fds[0].revents() {
                    if revents.contains(nix::poll::PollFlags::POLLPRI) {
                        handle_pressure_event(shared_dir);
                    }
                }
            }
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                error!("PSI poll failed: {e}");
                return Err(e.into());
            }
        }
    }
}

fn handle_pressure_event(shared_dir: &Path) {
    debug!("PSI memory pressure event detected");

    // Read current meminfo to include in the signal
    let mem_available_kb = read_mem_available().unwrap_or(0);

    // Write signal file — the shim watches for this
    let signal_path = shared_dir.join(PRESSURE_SIGNAL_FILE);
    match std::fs::write(&signal_path, format!("{}\n", mem_available_kb)) {
        Ok(()) => {
            info!(
                "PSI pressure signal written: available={}MiB",
                mem_available_kb / 1024
            );
        }
        Err(e) => {
            warn!("failed to write pressure signal: {e}");
        }
    }
}

fn read_mem_available() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if line.starts_with("MemAvailable:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                return parts[1].parse().ok();
            }
        }
    }
    None
}
