//! Shared Netlink (NETLINK_ROUTE) helpers used by both the shim and the agent.
//!
//! Provides link queries, address management, routing, and TC operations via
//! raw libc syscalls — zero external dependencies beyond libc.

use std::net::Ipv4Addr;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result};

// ── Netlink socket ──────────────────────────────────────────────────────────

pub struct Netlink {
    fd: OwnedFd,
    seq: std::cell::Cell<u32>,
}

impl Netlink {
    pub fn open() -> Result<Self> {
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

    pub fn seq(&self) -> u32 {
        let s = self.seq.get();
        self.seq.set(s + 1);
        s
    }

    pub fn send(&self, buf: &[u8]) -> Result<()> {
        if unsafe { libc::send(self.fd.as_raw_fd(), buf.as_ptr() as _, buf.len(), 0) } < 0 {
            Err(std::io::Error::last_os_error()).context("nl send")
        } else {
            Ok(())
        }
    }

    pub fn recv_buf(&self, buf: &mut [u8]) -> Result<usize> {
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr() as _, buf.len(), 0) };
        if n < 0 {
            Err(std::io::Error::last_os_error()).context("nl recv")
        } else {
            Ok(n as usize)
        }
    }

    pub fn request(&self, buf: &[u8]) -> Result<()> {
        self.send(buf)?;
        let mut r = [0u8; 4096];
        let n = self.recv_buf(&mut r)?;
        if n >= 20 && u16_at(&r, 4) == 2 {
            let e = i32_at(&r, 16);
            if e != 0 {
                return Err(std::io::Error::from_raw_os_error(-e)).context("netlink request");
            }
        }
        Ok(())
    }

    pub fn dump(&self, buf: &[u8]) -> Result<Vec<Vec<u8>>> {
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

    pub fn get_link_index(&self, name: &str) -> Result<u32> {
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

    pub fn set_link_up(&self, idx: u32) -> Result<()> {
        let mut m = vec![0u8; 32];
        nlhdr(&mut m, 32, 16, 5, self.seq());
        m[20..24].copy_from_slice(&(idx as i32).to_ne_bytes());
        let up = (libc::IFF_UP as u32).to_ne_bytes();
        m[24..28].copy_from_slice(&up);
        m[28..32].copy_from_slice(&up);
        self.request(&m)
    }

    pub fn del_link(&self, name: &str) -> Result<()> {
        let idx = self.get_link_index(name)?;
        let mut m = vec![0u8; 32];
        nlhdr(&mut m, 32, 17, 5, self.seq());
        m[20..24].copy_from_slice(&(idx as i32).to_ne_bytes());
        self.request(&m)
    }

    pub fn dump_links(&self) -> Result<Vec<(u32, String, String)>> {
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

    /// Find a network interface by its MAC address.
    /// Returns `(name, index)` or an error if not found.
    pub fn find_by_mac(&self, target_mac: &str) -> Result<Option<(String, u32)>> {
        let target = target_mac.to_lowercase();
        for (idx, name, mac) in self.dump_links()? {
            if mac.to_lowercase() == target {
                return Ok(Some((name, idx)));
            }
        }
        Ok(None)
    }

    pub fn find_veth(&self, tap: &str) -> Result<Option<(String, u32, String, String)>> {
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

    pub fn get_ipv4(&self, ifindex: u32) -> Result<Option<String>> {
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

    pub fn flush_addrs(&self, ifindex: u32) -> Result<()> {
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

    pub fn get_default_gw(&self) -> Result<Option<String>> {
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

    /// Delete all IPv4 routes associated with an interface.
    pub fn flush_routes(&self, ifindex: u32) -> Result<()> {
        let mut m = vec![0u8; 28];
        nlhdr(&mut m, 28, 26, 0x301, self.seq()); // RTM_GETROUTE, NLM_F_ROOT|NLM_F_MATCH|NLM_F_REQUEST
        m[16] = libc::AF_INET as u8;
        for msg in self.dump(&m)? {
            if msg.len() < 28 || u16_at(&msg, 4) != 24 {
                continue;
            }
            // Check if this route is for our interface (RTA_OIF = type 4)
            let nla_data = &msg[28..];
            let mut found = false;
            let mut offset = 0;
            while offset + 4 <= nla_data.len() {
                let nla_len = u16_at(nla_data, offset) as usize;
                if nla_len < 4 {
                    break;
                }
                let nla_type = u16_at(nla_data, offset + 2);
                if nla_type == 4 && nla_len >= 8 {
                    let oif = u32_at(nla_data, offset + 4);
                    if oif == ifindex {
                        found = true;
                    }
                }
                offset += (nla_len + 3) & !3;
            }
            if found {
                let mut del = msg.clone();
                let del_len = del.len();
                nlhdr(&mut del, del_len, 25, 5, self.seq()); // RTM_DELROUTE, NLM_F_REQUEST|NLM_F_ACK
                let _ = self.request(&del); // best-effort
            }
        }
        Ok(())
    }

    pub fn add_ingress_qdisc(&self, ifindex: u32) -> Result<()> {
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

    pub fn del_ingress_qdisc(&self, ifindex: u32) -> Result<()> {
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

    pub fn add_redirect_filter(&self, src_idx: u32, dst_idx: u32) -> Result<()> {
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

    /// Add an IPv4 address to a network interface (RTM_NEWADDR).
    pub fn add_address(&self, ifindex: u32, ip: Ipv4Addr, prefix_len: u8) -> Result<()> {
        // nlmsghdr(16) + ifaddrmsg(8) + IFA_LOCAL(8) + IFA_ADDRESS(8) = 40
        let total = 40;
        let mut m = vec![0u8; total];
        // RTM_NEWADDR=20, NLM_F_REQUEST|NLM_F_ACK|NLM_F_CREATE|NLM_F_EXCL = 0x0605
        nlhdr(&mut m, total, 20, 0x0605, self.seq());
        // ifaddrmsg: family, prefixlen, flags, scope, index
        m[16] = libc::AF_INET as u8;
        m[17] = prefix_len;
        m[18] = 0; // flags
        m[19] = 0; // scope
        m[20..24].copy_from_slice(&ifindex.to_ne_bytes());
        // IFA_LOCAL (type=2)
        let oct = ip.octets();
        put_nla(&mut m[24..], 2, 4);
        m[28..32].copy_from_slice(&oct);
        // IFA_ADDRESS (type=1)
        put_nla(&mut m[32..], 1, 4);
        m[36..40].copy_from_slice(&oct);
        self.request(&m)
    }

    /// Add a default route via the given gateway (RTM_NEWROUTE).
    pub fn add_default_route(&self, gateway: Ipv4Addr, ifindex: u32) -> Result<()> {
        // nlmsghdr(16) + rtmsg(12) + RTA_GATEWAY(8) + RTA_OIF(8) = 44
        let total = 44;
        let mut m = vec![0u8; total];
        // RTM_NEWROUTE=24, NLM_F_REQUEST|NLM_F_ACK|NLM_F_CREATE|NLM_F_EXCL = 0x0605
        nlhdr(&mut m, total, 24, 0x0605, self.seq());
        // rtmsg
        m[16] = libc::AF_INET as u8; // family
        m[17] = 0; // dst_len (0 = default route)
        m[18] = 0; // src_len
        m[19] = 0; // tos
        m[20] = 254; // table = RT_TABLE_MAIN
        m[21] = 3; // protocol = RTPROT_BOOT
        m[22] = 0; // scope = RT_SCOPE_UNIVERSE
        m[23] = 1; // type = RTN_UNICAST
                   // RTNH_F_ONLINK (0x4) — required for link-local gateways like 169.254.1.1
                   // that are not within the interface's subnet.
        m[24..28].copy_from_slice(&4u32.to_ne_bytes()); // flags = RTNH_F_ONLINK
                                                        // RTA_GATEWAY (type=5)
        put_nla(&mut m[28..], 5, 4);
        m[32..36].copy_from_slice(&gateway.octets());
        // RTA_OIF (type=4)
        put_nla(&mut m[36..], 4, 4);
        m[40..44].copy_from_slice(&ifindex.to_ne_bytes());
        self.request(&m)
    }
}

// ── Public convenience API ──────────────────────────────────────────────────

/// Configure a network interface: resolve link index (with retry for
/// hot-plugged devices), add an IPv4 address, bring the link up, and
/// optionally add a default route.
///
/// If `mac` is provided, the device is found by MAC address instead of by
/// name. This is essential for warm-restored VMs where the snapshot leaves
/// a stale eth0 in guest memory, and the hot-plugged TAP gets a different
/// name (e.g. eth1). The MAC always matches the hot-plugged device.
pub fn configure_interface(
    device: &str,
    ip: Ipv4Addr,
    prefix_len: u8,
    gateway: Option<Ipv4Addr>,
    mac: Option<&str>,
) -> Result<()> {
    let nl = Netlink::open()?;

    let (resolved_name, idx) = if let Some(mac_addr) = mac {
        // MAC-based lookup: retry until the hot-plugged device appears.
        // Hot-plugged virtio-net devices in a restored VM can take several
        // seconds to enumerate through the PCI bus, so we allow up to 10s.
        retry_find_by_mac(&nl, mac_addr, 50, 200)?
    } else {
        // Name-based lookup (cold boot path)
        let idx = retry_get_link_index(&nl, device, 20, 100)?;
        (device.to_string(), idx)
    };

    log::info!(
        "configure_interface: resolved device={} idx={} (requested={}, mac={:?})",
        resolved_name,
        idx,
        device,
        mac
    );

    // Flush existing addresses and routes — essential for snapshot restore
    // where eth0 still has the old pod's IP and routes from snapshot memory.
    if let Err(e) = nl.flush_routes(idx) {
        log::debug!("configure_interface: flush_routes failed (non-fatal): {e}");
    }
    if let Err(e) = nl.flush_addrs(idx) {
        log::debug!("configure_interface: flush_addrs failed (non-fatal): {e}");
    }

    if let Err(e) = nl.add_address(idx, ip, prefix_len) {
        let msg = e.to_string();
        // Treat "already exists" as success to make this idempotent.
        if msg.contains("EEXIST") || msg.contains("File exists") {
            log::debug!(
                "configure_interface: address {}/{} already present on {}: {}",
                ip,
                prefix_len,
                resolved_name,
                msg
            );
        } else {
            return Err(e);
        }
    }
    nl.set_link_up(idx)?;
    if let Some(gw) = gateway {
        if let Err(e) = nl.add_default_route(gw, idx) {
            let msg = e.to_string();
            // Treat "already exists" as success to make this idempotent.
            if msg.contains("EEXIST") || msg.contains("File exists") {
                log::debug!(
                    "configure_interface: default route via {} on {} already present: {}",
                    gw,
                    resolved_name,
                    msg
                );
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

fn retry_get_link_index(
    nl: &Netlink,
    device: &str,
    max_attempts: u32,
    delay_ms: u64,
) -> Result<u32> {
    for attempt in 0..max_attempts {
        match nl.get_link_index(device) {
            Ok(idx) => {
                if attempt > 0 {
                    log::info!("device {device} appeared after {attempt} retries");
                }
                return Ok(idx);
            }
            Err(e) => {
                if attempt < max_attempts - 1 {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                } else {
                    return Err(e).context(format!(
                        "device {device} not found after {max_attempts} retries"
                    ));
                }
            }
        }
    }
    unreachable!()
}

/// Retry finding a network interface by MAC address. Hot-plugged devices
/// may take a moment to appear in the guest kernel's interface list.
fn retry_find_by_mac(
    nl: &Netlink,
    mac: &str,
    max_attempts: u32,
    delay_ms: u64,
) -> Result<(String, u32)> {
    for attempt in 0..max_attempts {
        match nl.find_by_mac(mac)? {
            Some((name, idx)) => {
                if attempt > 0 {
                    log::info!("device with mac {mac} appeared as {name} after {attempt} retries");
                }
                return Ok((name, idx));
            }
            None => {
                if attempt == 0 || attempt == max_attempts / 2 {
                    // Log visible devices on first and midpoint attempts for debugging
                    if let Ok(links) = nl.dump_links() {
                        let devs: Vec<String> = links
                            .iter()
                            .map(|(idx, name, m)| format!("{name}(idx={idx},mac={m})"))
                            .collect();
                        log::info!(
                            "find_by_mac: attempt {attempt}, looking for {mac}, visible: [{}]",
                            devs.join(", ")
                        );
                    }
                }
                if attempt < max_attempts - 1 {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                } else {
                    // Final attempt: log all visible devices for diagnosis
                    let devs = nl
                        .dump_links()
                        .map(|links| {
                            links
                                .iter()
                                .map(|(idx, name, m)| format!("{name}(idx={idx},mac={m})"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    return Err(anyhow::anyhow!(
                        "device with mac {mac} not found after {max_attempts} retries; visible: [{devs}]"
                    ));
                }
            }
        }
    }
    unreachable!()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_ne_bytes([b[o], b[o + 1]])
}
pub fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub fn i32_at(b: &[u8], o: usize) -> i32 {
    i32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn nlhdr(buf: &mut [u8], len: usize, ty: u16, flags: u16, seq: u32) {
    buf[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
    buf[4..6].copy_from_slice(&ty.to_ne_bytes());
    buf[6..8].copy_from_slice(&flags.to_ne_bytes());
    buf[8..12].copy_from_slice(&seq.to_ne_bytes());
}

pub fn put_nla(buf: &mut [u8], ty: u16, payload_len: usize) {
    buf[0..2].copy_from_slice(&((4 + payload_len) as u16).to_ne_bytes());
    buf[2..4].copy_from_slice(&ty.to_ne_bytes());
}

pub fn push_nla_str(buf: &mut Vec<u8>, ty: u16, s: &str) {
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

pub fn close_nested(buf: &mut [u8], start: usize, ty: u16) {
    let len = buf.len() - start;
    buf[start..start + 2].copy_from_slice(&(len as u16).to_ne_bytes());
    buf[start + 2..start + 4].copy_from_slice(&(ty | 0x8000).to_ne_bytes());
}

pub fn parse_link_nlas(data: &[u8]) -> (String, String) {
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

pub fn find_ipv4_nla(data: &[u8], target: u16) -> Option<Ipv4Addr> {
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
