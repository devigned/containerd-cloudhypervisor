//! Network namespace cleanup for the containerd shim.
//!
//! The daemon handles TAP creation, namespace movement, and TC redirect
//! setup. The shim only needs cleanup (removing qdiscs and TAP on pod
//! teardown).

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result};
use cloudhv_common::netlink::Netlink;

pub async fn cleanup_tap(netns_path: &str, tap_name: &str) {
    let netns = netns_path.to_string();
    let tap = tap_name.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = in_netns_nowait(&netns, || {
            if let Ok(nl) = Netlink::open() {
                // Only remove ingress qdisc from the paired veth, not all interfaces
                if let Ok(Some((_, veth_idx, _, _))) = nl.find_veth(&tap) {
                    let _ = nl.del_ingress_qdisc(veth_idx);
                }
                if let Ok(tap_idx) = nl.get_link_index(&tap) {
                    let _ = nl.del_ingress_qdisc(tap_idx);
                }
                let _ = nl.del_link(&tap);
                log::info!("cleaned up TAP {tap} via netlink");
            }
            Ok(())
        });
    })
    .await;
}

// ── Netns helpers ───────────────────────────────────────────────────────────

struct NetnsGuard(OwnedFd);

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::setns(self.0.as_raw_fd(), libc::CLONE_NEWNET) };
    }
}

fn in_netns_nowait<F, T>(path: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    if !std::path::Path::new(path).exists() {
        anyhow::bail!("netns {path} gone");
    }
    let orig = open_ns("/proc/self/ns/net")?;
    let target = open_ns(path)?;
    if unsafe { libc::setns(target.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
        return Err(std::io::Error::last_os_error()).context("setns into target");
    }
    drop(target);
    let _guard = NetnsGuard(orig);
    f()
}

fn open_ns(path: &str) -> Result<OwnedFd> {
    let c = std::ffi::CString::new(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("open {path}"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
