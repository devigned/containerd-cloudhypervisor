//! Network namespace helpers for the daemon.
//!
//! Handles TAP device creation in the host netns, moving to the pod netns,
//! and setting up TC redirects for traffic between the veth and TAP.
//!
//! Flow: create TAP in host → CH binds it via vm.add-net → move TAP to pod
//! netns → set up TC redirects (veth↔TAP). CH's open fd remains valid across
//! the namespace move.

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result};
use cloudhv_common::netlink::{create_tap, Netlink};

/// Network info discovered from the pod's network namespace.
pub struct PodNetInfo {
    pub ip_cidr: String,
    pub gateway: String,
    pub mac: String,
}

/// Phase 1: Discover veth info from pod netns, then create a persistent TAP
/// in the host (daemon) namespace. Must be called from a blocking context.
pub fn prepare_tap(netns_path: &str, tap_name: &str) -> Result<PodNetInfo> {
    // Discover veth info (IP, MAC, gateway) from pod netns
    let info = in_netns(netns_path, || {
        let nl = Netlink::open().context("netlink")?;
        let veth = retry(20, 100, || {
            for (idx, name, mac) in nl.dump_links()? {
                if name == "lo" || name.is_empty() {
                    continue;
                }
                if let Some(cidr) = nl.get_ipv4(idx)? {
                    return Ok(Some((cidr, mac)));
                }
            }
            Ok(None)
        })
        .context("find veth")?;
        // Gateway is optional — some CNI configs (e.g. bridge without
        // explicit routes) don't add a default route in the pod netns.
        // When missing, infer the gateway as the first IP in the subnet
        // (e.g. 10.88.0.2/16 → 10.88.0.1), which matches what bridge
        // CNI assigns to the cni0 bridge with isGateway: true.
        let gw = match nl.get_default_gw() {
            Ok(Some(g)) => g,
            _ => {
                let inferred = infer_gateway(&veth.0);
                log::info!(
                    "no default gateway in pod netns, inferred {} from {}",
                    inferred.as_deref().unwrap_or("none"),
                    veth.0
                );
                inferred.unwrap_or_default()
            }
        };
        Ok(PodNetInfo {
            ip_cidr: veth.0,
            mac: veth.1,
            gateway: gw,
        })
    })?;

    // Back in host netns — clean up stale TAP and create fresh one
    {
        let nl = Netlink::open().context("netlink")?;
        let _ = nl.del_link(tap_name);
    }
    create_tap(tap_name).context("create TAP")?;

    Ok(info)
}

/// Phase 3: Move TAP from host netns to pod netns and set up TC redirects.
///
/// Must be called after CH has opened the TAP (via vm.add-net). The fd CH
/// holds remains valid across the namespace move.
pub fn activate_tap(netns_path: &str, tap_name: &str) -> Result<()> {
    // Move TAP interface to pod netns via IFLA_NET_NS_FD
    {
        let nl = Netlink::open().context("netlink")?;
        let tap_idx = nl.get_link_index(tap_name).context("TAP index")?;
        let ns_fd = open_ns(netns_path)?;
        nl.set_link_netns_fd(tap_idx, ns_fd.as_raw_fd())
            .context("move TAP to pod netns")?;
    }

    // Enter pod netns and wire up TC redirects
    in_netns(netns_path, || {
        let nl = Netlink::open().context("netlink")?;
        let tap_idx = nl.get_link_index(tap_name).context("TAP index")?;
        nl.set_link_up(tap_idx).context("TAP up")?;
        let (_, veth_idx, _, _) = retry(20, 100, || nl.find_veth(tap_name)).context("find veth")?;
        nl.add_ingress_qdisc(veth_idx).context("ingress veth")?;
        nl.add_redirect_filter(veth_idx, tap_idx)
            .context("redir veth→tap")?;
        nl.add_ingress_qdisc(tap_idx).context("ingress tap")?;
        nl.add_redirect_filter(tap_idx, veth_idx)
            .context("redir tap→veth")?;
        if let Err(e) = nl.flush_addrs(veth_idx) {
            log::warn!("best-effort IP flush: {e:#}");
        }
        log::info!("TAP {tap_name}: TC redirects active in pod netns");
        Ok(())
    })
}

// ── Netns helpers ───────────────────────────────────────────────────────────

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

fn open_ns(path: &str) -> Result<OwnedFd> {
    let c = std::ffi::CString::new(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("open {path}"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
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

/// Infer a gateway from a CIDR address by using the first usable IP in the
/// subnet. For example, "10.88.0.2/16" → "10.88.0.1".
fn infer_gateway(ip_cidr: &str) -> Option<String> {
    let parts: Vec<&str> = ip_cidr.split('/').collect();
    let ip: std::net::Ipv4Addr = parts.first()?.parse().ok()?;
    let prefix: u32 = parts.get(1)?.parse().ok()?;
    if prefix > 30 {
        return None;
    }
    let mask = !((1u32 << (32 - prefix)) - 1);
    let network = u32::from(ip) & mask;
    let gateway = std::net::Ipv4Addr::from(network + 1);
    Some(gateway.to_string())
}
