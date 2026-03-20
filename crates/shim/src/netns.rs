//! In-process TAP device and tc redirect setup via raw libc syscalls.
//!
//! Replaces 10+ subprocess calls to nsenter/ip/tc with direct system calls:
//! - TAP creation: ioctl on /dev/net/tun
//! - Network queries: netlink RTM_GETLINK, RTM_GETADDR, RTM_GETROUTE
//! - TC redirect: netlink RTM_NEWQDISC, RTM_NEWTFILTER
//! - Address flush: netlink RTM_DELADDR
//!
//! Zero external dependencies beyond libc.

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result};
use cloudhv_common::netlink::Netlink;

// ── Public API ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TapInfo {
    pub tap_name: String,
    pub mac: String,
    pub ip_cidr: String,
    pub gateway: String,
}

pub async fn setup_tap(netns_path: &str, vm_id: &str) -> Result<TapInfo> {
    let tap_name = format!("tap_{}", &vm_id[..8.min(vm_id.len())]);
    let netns = netns_path.to_string();
    let tap = tap_name.clone();
    tokio::task::spawn_blocking(move || in_netns(&netns, || do_setup(&tap)))
        .await
        .context("TAP setup task panicked")?
}

pub async fn cleanup_tap(netns_path: &str, tap_name: &str) {
    let netns = netns_path.to_string();
    let tap = tap_name.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = in_netns_nowait(&netns, || {
            if let Ok(nl) = Netlink::open() {
                if let Ok(links) = nl.dump_links() {
                    for (idx, name, _) in &links {
                        if name != "lo" && name != &tap {
                            let _ = nl.del_ingress_qdisc(*idx);
                        }
                    }
                }
                let _ = nl.del_link(&tap);
                log::info!("cleaned up TAP {tap} via netlink");
            }
            Ok(())
        });
    })
    .await;
}

// ── Netns helper ────────────────────────────────────────────────────────────

struct NetnsGuard(OwnedFd);

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::setns(self.0.as_raw_fd(), libc::CLONE_NEWNET) };
    }
}

fn in_netns<F, T>(path: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    for attempt in 0..20 {
        if std::path::Path::new(path).exists() {
            if attempt > 0 {
                log::info!("netns appeared after {attempt} retries");
            }
            break;
        }
        if attempt == 19 {
            anyhow::bail!("netns {path} did not appear after 2s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
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

// ── Setup orchestrator ──────────────────────────────────────────────────────

fn do_setup(tap_name: &str) -> Result<TapInfo> {
    let nl = Netlink::open().context("open netlink")?;
    let _ = nl.del_link(tap_name);
    create_tap(tap_name)?;
    let tap_idx = nl.get_link_index(tap_name).context("TAP index")?;
    nl.set_link_up(tap_idx).context("TAP up")?;
    let (veth_name, veth_idx, ip_cidr, mac) =
        retry(20, 100, || nl.find_veth(tap_name)).context("find veth")?;
    let gw = retry(20, 100, || nl.get_default_gw()).context("find gw")?;
    nl.add_ingress_qdisc(veth_idx).context("ingress veth")?;
    nl.add_redirect_filter(veth_idx, tap_idx)
        .context("redir veth→tap")?;
    nl.add_ingress_qdisc(tap_idx).context("ingress tap")?;
    nl.add_redirect_filter(tap_idx, veth_idx)
        .context("redir tap→veth")?;
    if let Err(e) = nl.flush_addrs(veth_idx) {
        log::warn!("best-effort IP flush failed for veth index {veth_idx}: {e:#}");
    }
    log::info!("TAP {tap_name} via netlink: veth={veth_name} ip={ip_cidr} gw={gw} mac={mac}");
    Ok(TapInfo {
        tap_name: tap_name.to_string(),
        mac,
        ip_cidr,
        gateway: gw,
    })
}

fn retry<T>(max: u32, ms: u64, mut f: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    for i in 0..max {
        if let Some(v) = f()? {
            if i > 0 {
                log::info!("found after {i} retries");
            }
            return Ok(v);
        }
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    anyhow::bail!("not found after {max} retries")
}

// ── TAP ioctl ───────────────────────────────────────────────────────────────

fn create_tap(name: &str) -> Result<()> {
    let c = std::ffi::CString::new("/dev/net/tun")?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open /dev/net/tun");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut ifr = [0u8; 40];
    let b = name.as_bytes();
    ifr[..b.len().min(15)].copy_from_slice(&b[..b.len().min(15)]);
    ifr[16..18].copy_from_slice(&(0x0002i16 | 0x1000i16).to_ne_bytes());
    // Use i64 constants — libc::ioctl's second param type varies between
    // glibc (c_ulong) and musl (c_int). Casting to the right type at call site.
    if unsafe { libc::ioctl(fd.as_raw_fd(), 0x400454ca as _, ifr.as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error()).context("TUNSETIFF");
    }
    if unsafe { libc::ioctl(fd.as_raw_fd(), 0x400454cb as _, 1i32) } < 0 {
        return Err(std::io::Error::last_os_error()).context("TUNSETPERSIST");
    }
    Ok(())
}
