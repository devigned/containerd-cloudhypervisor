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
