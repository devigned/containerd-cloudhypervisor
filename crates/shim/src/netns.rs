//! In-process TAP device and tc redirect setup via raw libc syscalls.
//!
//! Replaces 10+ subprocess calls to nsenter/ip/tc with direct system calls:
//! - TAP creation: ioctl on /dev/net/tun
//! - Network queries: netlink RTM_GETLINK, RTM_GETADDR, RTM_GETROUTE
//! - TC redirect: netlink RTM_NEWQDISC, RTM_NEWTFILTER
//! - Address flush: netlink RTM_DELADDR
//!
//! Zero external dependencies beyond libc.

use std::net::Ipv4Addr;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result};

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

// ── Netlink socket ──────────────────────────────────────────────────────────

struct Netlink {
    fd: OwnedFd,
    seq: std::cell::Cell<u32>,
}

impl Netlink {
    fn open() -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("socket(AF_NETLINK)");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as u16;
        if unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &sa as *const _ as *const _,
                std::mem::size_of_val(&sa) as u32,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error()).context("bind(AF_NETLINK)");
        }
        let mut dst: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        dst.nl_family = libc::AF_NETLINK as u16;
        // nl_pid=0 targets the kernel
        if unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &dst as *const _ as *const _,
                std::mem::size_of_val(&dst) as u32,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error()).context("connect(AF_NETLINK)");
        }
        Ok(Self {
            fd,
            seq: std::cell::Cell::new(1),
        })
    }

    fn seq(&self) -> u32 {
        let s = self.seq.get();
        self.seq.set(s + 1);
        s
    }

    fn send(&self, buf: &[u8]) -> Result<()> {
        if unsafe { libc::send(self.fd.as_raw_fd(), buf.as_ptr() as _, buf.len(), 0) } < 0 {
            Err(std::io::Error::last_os_error()).context("nl send")
        } else {
            Ok(())
        }
    }

    fn recv_buf(&self, buf: &mut [u8]) -> Result<usize> {
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr() as _, buf.len(), 0) };
        if n < 0 {
            Err(std::io::Error::last_os_error()).context("nl recv")
        } else {
            Ok(n as usize)
        }
    }

    fn request(&self, buf: &[u8]) -> Result<()> {
        self.send(buf)?;
        let mut r = [0u8; 4096];
        let n = self.recv_buf(&mut r)?;
        if n >= 20 && u16_at(&r, 4) == 2 {
            let e = i32_at(&r, 16);
            if e != 0 {
                return Err(std::io::Error::from_raw_os_error(-e)).context("nl error");
            }
        }
        Ok(())
    }

    fn dump(&self, buf: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.send(buf)?;
        let mut out = Vec::new();
        let mut r = [0u8; 32768];
        loop {
            let n = self.recv_buf(&mut r)?;
            let mut off = 0;
            while off + 16 <= n {
                let len = u32_at(&r, off) as usize;
                let ty = u16_at(&r, off + 4);
                if ty == 3 {
                    return Ok(out);
                }
                if ty == 2 {
                    let e = i32_at(&r, off + 16);
                    if e != 0 {
                        anyhow::bail!("nl dump: {}", std::io::Error::from_raw_os_error(-e));
                    }
                }
                if len > 0 && off + len <= n {
                    out.push(r[off..off + len].to_vec());
                }
                off += (len + 3) & !3;
            }
        }
    }

    fn get_link_index(&self, name: &str) -> Result<u32> {
        let nb = name.as_bytes();
        let attr = 4 + nb.len() + 1;
        let total = 16 + 16 + ((attr + 3) & !3);
        let mut m = vec![0u8; total];
        nlhdr(&mut m, total, 18, 5, self.seq());
        let o = 32;
        put_nla(&mut m[o..], 3, nb.len() + 1);
        m[o + 4..o + 4 + nb.len()].copy_from_slice(nb);
        self.send(&m)?;
        let mut r = [0u8; 4096];
        let n = self.recv_buf(&mut r)?;
        if n >= 20 && u16_at(&r, 4) == 2 {
            let e = i32_at(&r, 16);
            if e != 0 {
                anyhow::bail!("link {name}: {}", std::io::Error::from_raw_os_error(-e));
            }
        }
        if n < 24 {
            anyhow::bail!("link {name} not found");
        }
        Ok(i32_at(&r, 20) as u32)
    }

    fn set_link_up(&self, idx: u32) -> Result<()> {
        let mut m = vec![0u8; 32];
        nlhdr(&mut m, 32, 16, 5, self.seq());
        m[20..24].copy_from_slice(&(idx as i32).to_ne_bytes());
        let up = (libc::IFF_UP as u32).to_ne_bytes();
        m[24..28].copy_from_slice(&up);
        m[28..32].copy_from_slice(&up);
        self.request(&m)
    }

    fn del_link(&self, name: &str) -> Result<()> {
        let idx = self.get_link_index(name)?;
        let mut m = vec![0u8; 32];
        nlhdr(&mut m, 32, 17, 5, self.seq());
        m[20..24].copy_from_slice(&(idx as i32).to_ne_bytes());
        self.request(&m)
    }

    fn dump_links(&self) -> Result<Vec<(u32, String, String)>> {
        let mut m = vec![0u8; 32];
        nlhdr(&mut m, 32, 18, 0x301, self.seq());
        let mut out = Vec::new();
        for msg in self.dump(&m)? {
            if msg.len() < 32 || u16_at(&msg, 4) != 16 {
                continue;
            }
            let idx = i32_at(&msg, 20) as u32;
            let (name, mac) = parse_link_nlas(&msg[32..]);
            out.push((idx, name, mac));
        }
        Ok(out)
    }

    fn find_veth(&self, tap: &str) -> Result<Option<(String, u32, String, String)>> {
        for (idx, name, mac) in self.dump_links()? {
            if name == "lo" || name == tap || name.is_empty() {
                continue;
            }
            if let Some(cidr) = self.get_ipv4(idx)? {
                return Ok(Some((name, idx, cidr, mac)));
            }
        }
        Ok(None)
    }

    fn get_ipv4(&self, ifindex: u32) -> Result<Option<String>> {
        let mut m = vec![0u8; 24];
        nlhdr(&mut m, 24, 22, 0x301, self.seq());
        m[16] = libc::AF_INET as u8;
        for msg in self.dump(&m)? {
            if msg.len() < 24 || u16_at(&msg, 4) != 20 {
                continue;
            }
            if msg[16] != libc::AF_INET as u8 {
                continue;
            }
            let pfx = msg[17];
            if u32_at(&msg, 20) != ifindex {
                continue;
            }
            if let Some(ip) = find_ipv4_nla(&msg[24..], 1) {
                return Ok(Some(format!("{ip}/{pfx}")));
            }
        }
        Ok(None)
    }

    fn flush_addrs(&self, ifindex: u32) -> Result<()> {
        let mut m = vec![0u8; 24];
        nlhdr(&mut m, 24, 22, 0x301, self.seq());
        m[16] = libc::AF_INET as u8;
        for msg in self.dump(&m)? {
            if msg.len() < 24 || u16_at(&msg, 4) != 20 {
                continue;
            }
            if msg[16] != libc::AF_INET as u8 || u32_at(&msg, 20) != ifindex {
                continue;
            }
            let mut del = msg.clone();
            let del_len = del.len();
            nlhdr(&mut del, del_len, 21, 5, self.seq());
            self.request(&del)?;
        }
        Ok(())
    }

    fn get_default_gw(&self) -> Result<Option<String>> {
        let mut m = vec![0u8; 28];
        nlhdr(&mut m, 28, 26, 0x301, self.seq());
        m[16] = libc::AF_INET as u8;
        for msg in self.dump(&m)? {
            if msg.len() < 28 || u16_at(&msg, 4) != 24 {
                continue;
            }
            if msg[17] != 0 {
                continue;
            }
            if let Some(gw) = find_ipv4_nla(&msg[28..], 5) {
                return Ok(Some(gw.to_string()));
            }
        }
        Ok(None)
    }

    fn add_ingress_qdisc(&self, ifindex: u32) -> Result<()> {
        let kind = b"ingress\0";
        let attr_len = (4 + kind.len() + 3) & !3;
        let total = 16 + 20 + attr_len;
        let mut m = vec![0u8; total];
        nlhdr(&mut m, total, 36, 0x605, self.seq());
        m[16] = libc::AF_UNSPEC as u8;
        m[20..24].copy_from_slice(&(ifindex as i32).to_ne_bytes());
        m[24..28].copy_from_slice(&0xFFFF0000u32.to_ne_bytes());
        m[28..32].copy_from_slice(&0xFFFFFFF1u32.to_ne_bytes());
        let o = 36;
        put_nla(&mut m[o..], 1, kind.len());
        m[o + 4..o + 4 + kind.len()].copy_from_slice(kind);
        match self.request(&m) {
            Ok(()) => Ok(()),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.raw_os_error() == Some(libc::EEXIST)) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn del_ingress_qdisc(&self, ifindex: u32) -> Result<()> {
        let kind = b"ingress\0";
        let attr_len = (4 + kind.len() + 3) & !3;
        let total = 16 + 20 + attr_len;
        let mut m = vec![0u8; total];
        nlhdr(&mut m, total, 37, 5, self.seq()); // RTM_DELQDISC
        m[16] = libc::AF_UNSPEC as u8;
        m[20..24].copy_from_slice(&(ifindex as i32).to_ne_bytes());
        m[24..28].copy_from_slice(&0xFFFF0000u32.to_ne_bytes());
        m[28..32].copy_from_slice(&0xFFFFFFF1u32.to_ne_bytes());
        let o = 36;
        put_nla(&mut m[o..], 1, kind.len());
        m[o + 4..o + 4 + kind.len()].copy_from_slice(kind);
        self.request(&m)
    }

    fn add_redirect_filter(&self, src_idx: u32, dst_idx: u32) -> Result<()> {
        let mut b = Vec::with_capacity(256);
        b.resize(16, 0);
        // tcmsg
        b.push(libc::AF_UNSPEC as u8);
        b.extend([0u8; 3]);
        b.extend((src_idx as i32).to_ne_bytes());
        b.extend(0u32.to_ne_bytes()); // handle
        b.extend(0xFFFF0000u32.to_ne_bytes()); // parent = ingress qdisc (ffff:0000)
                                               // tcm_info: (priority << 16) | htons(protocol)
                                               // priority=0, protocol=ETH_P_ALL → htons(0x0003) = 0x0300 on LE
        let tcm_info = (libc::ETH_P_ALL as u16).to_be() as u32;
        b.extend(tcm_info.to_ne_bytes());
        // TCA_KIND = "u32"
        push_nla_str(&mut b, 1, "u32");
        // TCA_OPTIONS (type=2, NO NLA_F_NESTED — kernel infers nesting from TCA_KIND)
        let opts = b.len();
        b.extend([0u8; 4]);

        // TCA_U32_ACT (type=7, nested actions) — must come BEFORE TCA_U32_SEL
        let act = b.len();
        b.extend([0u8; 4]);
        // Action tab entry 1 (type=1)
        let tab = b.len();
        b.extend([0u8; 4]);
        push_nla_str(&mut b, 1, "mirred"); // TCA_ACT_KIND
                                           // TCA_ACT_OPTIONS (type=2|NLA_F_NESTED)
        let ao = b.len();
        b.extend([0u8; 4]);
        // TCA_MIRRED_PARMS (type=2): tc_gen(20) + eaction(4) + ifindex(4) = 28 payload
        b.extend(32u16.to_ne_bytes()); // nla_len = 4 + 28
        b.extend(2u16.to_ne_bytes()); // type = 2 (TCA_MIRRED_PARMS)
        b.extend(0u32.to_ne_bytes()); // tc_gen.index
        b.extend(0u32.to_ne_bytes()); // tc_gen.capab
        b.extend(4i32.to_ne_bytes()); // tc_gen.action = TC_ACT_STOLEN
        b.extend(0i32.to_ne_bytes()); // tc_gen.refcnt
        b.extend(0i32.to_ne_bytes()); // tc_gen.bindcnt
        b.extend(1i32.to_ne_bytes()); // eaction = TCA_EGRESS_REDIR
        b.extend(dst_idx.to_ne_bytes()); // ifindex
        close_nested(&mut b, ao, 2); // TCA_ACT_OPTIONS (with NLA_F_NESTED)
                                     // Tab entry: type=1, no NLA_F_NESTED
        let tab_len = b.len() - tab;
        b[tab..tab + 2].copy_from_slice(&(tab_len as u16).to_ne_bytes());
        b[tab + 2..tab + 4].copy_from_slice(&1u16.to_ne_bytes());
        // TCA_U32_ACT: type=7, no NLA_F_NESTED
        let act_len = b.len() - act;
        b[act..act + 2].copy_from_slice(&(act_len as u16).to_ne_bytes());
        b[act + 2..act + 4].copy_from_slice(&7u16.to_ne_bytes());

        // TCA_U32_SEL (type=5): match-all selector with 1 key
        // tc_u32_sel: 16 bytes (flags, offshift, nkeys, offmask, off, offoff, hoff, hmask, pad)
        // tc_u32_key: mask(4) val(4) off(4) offmask(4) = 16 bytes
        let sel = b.len();
        b.extend([0u8; 4]); // NLA header
        let mut sel_hdr = [0u8; 16]; // tc_u32_sel (16 bytes to match kernel struct)
        sel_hdr[0] = 1; // flags = 1 (as tc sends)
        sel_hdr[2] = 1; // nkeys = 1
        b.extend(sel_hdr);
        b.extend([0u8; 16]); // key: mask=0 val=0 = match all
        let sl = b.len() - sel;
        b[sel..sel + 2].copy_from_slice(&(sl as u16).to_ne_bytes());
        b[sel + 2..sel + 4].copy_from_slice(&5u16.to_ne_bytes()); // type=5 (TCA_U32_SEL)

        // Close TCA_OPTIONS (type=2, no NLA_F_NESTED)
        let opts_len = b.len() - opts;
        b[opts..opts + 2].copy_from_slice(&(opts_len as u16).to_ne_bytes());
        b[opts + 2..opts + 4].copy_from_slice(&2u16.to_ne_bytes());

        let b_len = b.len();
        nlhdr(&mut b, b_len, 44, 0x605, self.seq());
        self.request(&b)
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_ne_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn i32_at(b: &[u8], o: usize) -> i32 {
    i32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn nlhdr(buf: &mut [u8], len: usize, ty: u16, flags: u16, seq: u32) {
    buf[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
    buf[4..6].copy_from_slice(&ty.to_ne_bytes());
    buf[6..8].copy_from_slice(&flags.to_ne_bytes());
    buf[8..12].copy_from_slice(&seq.to_ne_bytes());
}

fn put_nla(buf: &mut [u8], ty: u16, payload_len: usize) {
    buf[0..2].copy_from_slice(&((4 + payload_len) as u16).to_ne_bytes());
    buf[2..4].copy_from_slice(&ty.to_ne_bytes());
}

fn push_nla_str(buf: &mut Vec<u8>, ty: u16, s: &str) {
    let p = s.as_bytes();
    let len = 4 + p.len() + 1;
    buf.extend((len as u16).to_ne_bytes());
    buf.extend(ty.to_ne_bytes());
    buf.extend_from_slice(p);
    buf.push(0);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

fn close_nested(buf: &mut [u8], start: usize, ty: u16) {
    let len = buf.len() - start;
    buf[start..start + 2].copy_from_slice(&(len as u16).to_ne_bytes());
    buf[start + 2..start + 4].copy_from_slice(&(ty | 0x8000).to_ne_bytes());
}

fn parse_link_nlas(data: &[u8]) -> (String, String) {
    let mut name = String::new();
    let mut mac = String::new();
    let mut off = 0;
    while off + 4 <= data.len() {
        let len = u16_at(data, off) as usize;
        let ty = u16_at(data, off + 2);
        if len < 4 || off + len > data.len() {
            break;
        }
        let p = &data[off + 4..off + len];
        if ty == 3 {
            name = String::from_utf8_lossy(p)
                .trim_end_matches('\0')
                .to_string();
        }
        if ty == 1 && p.len() == 6 {
            mac = p
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":");
        }
        off += (len + 3) & !3;
    }
    (name, mac)
}

fn find_ipv4_nla(data: &[u8], target: u16) -> Option<Ipv4Addr> {
    let mut off = 0;
    while off + 4 <= data.len() {
        let len = u16_at(data, off) as usize;
        let ty = u16_at(data, off + 2);
        if len < 4 || off + len > data.len() {
            break;
        }
        if ty == target && len >= 8 {
            let p = &data[off + 4..];
            if p.len() >= 4 {
                return Some(Ipv4Addr::new(p[0], p[1], p[2], p[3]));
            }
        }
        off += (len + 3) & !3;
    }
    None
}
