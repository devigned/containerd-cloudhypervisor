use log::debug;
#[cfg(target_os = "linux")]
use log::warn;

/// Set this process as a child subreaper.
#[cfg(target_os = "linux")]
pub fn set_child_subreaper() {
    unsafe {
        let ret = libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        if ret < 0 {
            warn!(
                "failed to set child subreaper: {}",
                std::io::Error::last_os_error()
            );
        } else {
            debug!("set as child subreaper");
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_child_subreaper() {
    debug!("set_child_subreaper: no-op on non-Linux");
}

/// SIGCHLD signal handler: reap zombie child processes.
#[cfg(target_os = "linux")]
pub extern "C" fn sigchld_handler(_signal: libc::c_int) {
    unsafe {
        loop {
            let mut status: libc::c_int = 0;
            let pid = libc::waitpid(-1, &mut status, libc::WNOHANG);
            if pid <= 0 {
                break;
            }
            #[cfg(debug_assertions)]
            {
                let msg = format!("reaped child pid={} status={}\n", pid, status);
                libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub extern "C" fn sigchld_handler(_signal: libc::c_int) {
    // no-op on non-Linux
}
