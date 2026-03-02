//! PF (packet filter) device interaction and anchor management.
//!
//! Uses macOS `/dev/pf` and the `DIOCNATLOOK` ioctl to recover the original
//! destination address for connections redirected by a `rdr` rule.
//!
//! Anchor `zq` is loaded/unloaded via `pfctl` to avoid hand-rolling rule
//! compilation.

use std::mem;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Constants from XNU pfvar.h
// ---------------------------------------------------------------------------

/// `DIOCNATLOOK` — `_IOWR('D', 23, struct pfioc_natlook)`
///
/// Encoding: direction bits (11 = read+write = 0xC000_0000)
///           | size (84 = 0x54 << 16)
///           | group 'D' (0x44 << 8)
///           | number 23 (0x17)
const DIOCNATLOOK: libc::c_ulong = 0xC054_4417;

const PF_OUT: u8 = 2;
const AF_INET: u8 = libc::AF_INET as u8;
const IPPROTO_TCP: u8 = libc::IPPROTO_TCP as u8;

// ---------------------------------------------------------------------------
// pfioc_natlook — matches XNU struct layout
// ---------------------------------------------------------------------------

/// A single PF address (matches `struct pf_addr` — a union, only IPv4 used).
/// 16 bytes: we store the IPv4 address in the first 4 bytes, rest zeroed.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PfAddr {
    v4: [u8; 4],
    _pad: [u8; 12],
}

impl PfAddr {
    fn from_ipv4(ip: Ipv4Addr) -> Self {
        Self {
            v4: ip.octets(),
            _pad: [0u8; 12],
        }
    }

    fn to_ipv4(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.v4)
    }
}

/// A `union pf_state_xport` — 4 bytes. We only use the `port` member (u16),
/// but the union is sized by its largest variant (`spi: u32`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PfStateXport {
    port: u16, // network byte order
    _pad: u16,
}

impl PfStateXport {
    fn from_port(port: u16) -> Self {
        Self {
            port: port.to_be(),
            _pad: 0,
        }
    }

    fn port_value(&self) -> u16 {
        u16::from_be(self.port)
    }
}

/// Matches `struct pfioc_natlook` from XNU `net/pfvar.h`.
///
/// Layout (84 bytes total):
///   saddr:    PfAddr (16)
///   daddr:    PfAddr (16)
///   rsaddr:   PfAddr (16)  — result: real source
///   rdaddr:   PfAddr (16)  — result: real destination
///   sxport:   pf_state_xport (4)
///   dxport:   pf_state_xport (4)
///   rsxport:  pf_state_xport (4)
///   rdxport:  pf_state_xport (4)
///   af:       u8
///   proto:    u8
///   direction: u8
///   _pad:     u8
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PfiocNatlook {
    saddr: PfAddr,
    daddr: PfAddr,
    rsaddr: PfAddr,
    rdaddr: PfAddr,
    sxport: PfStateXport,
    dxport: PfStateXport,
    rsxport: PfStateXport,
    rdxport: PfStateXport,
    af: u8,
    proto: u8,
    direction: u8,
    _pad: u8,
}

// Compile-time size assertion.
const _: () = assert!(mem::size_of::<PfiocNatlook>() == 84);

impl PfiocNatlook {
    /// Prepare a natlook query for a TCP connection from `client` to `listen`
    /// (the proxy's listen address).
    fn new(client: SocketAddrV4, listen: SocketAddrV4) -> Self {
        Self {
            saddr: PfAddr::from_ipv4(*client.ip()),
            daddr: PfAddr::from_ipv4(*listen.ip()),
            rsaddr: PfAddr { v4: [0; 4], _pad: [0; 12] },
            rdaddr: PfAddr { v4: [0; 4], _pad: [0; 12] },
            sxport: PfStateXport::from_port(client.port()),
            dxport: PfStateXport::from_port(listen.port()),
            rsxport: PfStateXport { port: 0, _pad: 0 },
            rdxport: PfStateXport { port: 0, _pad: 0 },
            af: AF_INET,
            proto: IPPROTO_TCP,
            direction: PF_OUT,
            _pad: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// PfDevice — /dev/pf file descriptor wrapper
// ---------------------------------------------------------------------------

/// Handle to `/dev/pf` for issuing DIOCNATLOOK ioctls.
pub struct PfDevice {
    fd: RawFd,
}

impl PfDevice {
    /// Open `/dev/pf` for reading. Requires root or appropriate permissions.
    pub fn open() -> Result<Self> {
        let path = c"/dev/pf";
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            bail!(
                "failed to open /dev/pf: {}",
                std::io::Error::last_os_error()
            );
        }
        debug!("opened /dev/pf (fd={fd})");
        Ok(Self { fd })
    }

    /// Look up the original destination for a connection that was redirected
    /// by a pf `rdr` rule.
    ///
    /// - `client`: the remote peer address (source IP:port of the incoming connection)
    /// - `listen`: the local address the proxy accepted on (127.0.0.1:8888)
    ///
    /// Returns the original destination `SocketAddrV4` before the `rdr` rewrite.
    pub fn natlook(&self, client: SocketAddrV4, listen: SocketAddrV4) -> Result<SocketAddrV4> {
        let mut nl = PfiocNatlook::new(client, listen);

        let ret = unsafe { libc::ioctl(self.fd, DIOCNATLOOK, &mut nl as *mut PfiocNatlook) };
        if ret != 0 {
            bail!(
                "DIOCNATLOOK failed for {}→{}: {}",
                client,
                listen,
                std::io::Error::last_os_error()
            );
        }

        let orig_ip = nl.rdaddr.to_ipv4();
        let orig_port = nl.rdxport.port_value();
        let orig = SocketAddrV4::new(orig_ip, orig_port);

        debug!(%client, %listen, %orig, "natlook resolved original destination");
        Ok(orig)
    }
}

impl AsRawFd for PfDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for PfDevice {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ---------------------------------------------------------------------------
// Anchor management via pfctl
// ---------------------------------------------------------------------------

/// Load pf redirect rules into anchor `zq`.
///
/// Two steps:
/// 1. Reload the main ruleset (`/etc/pf.conf` + anchor references for `zq`)
///    so pf knows to evaluate our anchor.
/// 2. Load the actual redirect + route-to rules into the `zq` anchor.
///
/// The `excluded_uid` prevents the daemon's own upstream connections from
/// being re-redirected (loop avoidance).
pub fn load_rules(port: u16, excluded_uid: u32) -> Result<()> {
    use std::io::Write;

    info!("loading pf rules into anchor 'zq' (proxy port {port}, excluded uid {excluded_uid})");

    // Step 1: Add anchor references to the main ruleset.
    // Read /etc/pf.conf and insert our anchor refs in the correct order
    // (rdr-anchor must appear with other translation rules, anchor with
    // filtering rules). Write to a temp file because macOS pfctl refuses
    // `-f -` on stdin.
    let base_conf =
        std::fs::read_to_string("/etc/pf.conf").context("failed to read /etc/pf.conf")?;

    let mut main_conf = String::new();
    let mut inserted_rdr = false;
    let mut inserted_anchor = false;

    for line in base_conf.lines() {
        main_conf.push_str(line);
        main_conf.push('\n');

        // Insert our rdr-anchor right after the last existing rdr-anchor line.
        if line.trim().starts_with("rdr-anchor") && !inserted_rdr {
            main_conf.push_str("rdr-anchor \"zq\"\n");
            inserted_rdr = true;
        }

        // Insert our anchor right after the last existing anchor line
        // (but not rdr-anchor, nat-anchor, etc.).
        if line.trim().starts_with("anchor ") && !inserted_anchor {
            main_conf.push_str("anchor \"zq\"\n");
            inserted_anchor = true;
        }
    }

    // Fallback: if /etc/pf.conf didn't have the expected lines, append.
    if !inserted_rdr {
        main_conf.push_str("rdr-anchor \"zq\"\n");
    }
    if !inserted_anchor {
        main_conf.push_str("anchor \"zq\"\n");
    }

    debug!("main pf config:\n{main_conf}");

    let tmp_conf = "/tmp/zq-pf.conf";
    std::fs::write(tmp_conf, &main_conf).context("failed to write temp pf config")?;

    let output = Command::new("pfctl")
        .args(["-f", tmp_conf])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to run pfctl -f for main config")?;

    let _ = std::fs::remove_file(tmp_conf);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("pfctl -f {tmp_conf} failed: {stderr}");
    }

    // Step 2: Load rules into the zq anchor.
    let anchor_rules = format!(
        "rdr pass on lo0 proto tcp from any to !127.0.0.1 port {{80, 443}} -> 127.0.0.1 port {port}\n\
         pass out route-to (lo0 127.0.0.1) proto tcp from any to any port {{80, 443}} keep state user != {excluded_uid}\n"
    );

    debug!("anchor rules:\n{anchor_rules}");

    let mut child = Command::new("pfctl")
        .args(["-a", "zq", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn pfctl for anchor rules")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(anchor_rules.as_bytes())
            .context("failed to write anchor rules to pfctl")?;
    }

    let output = child.wait_with_output().context("failed to wait for pfctl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("pfctl -a zq -f - failed: {stderr}");
    }

    // Enable pf if not already enabled (idempotent).
    let enable_output = Command::new("pfctl")
        .args(["-e"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to run pfctl -e")?;

    let stderr = String::from_utf8_lossy(&enable_output.stderr);
    if enable_output.status.success() {
        info!("pf enabled");
    } else if stderr.contains("already enabled") {
        debug!("pf was already enabled");
    } else {
        bail!("pfctl -e failed: {stderr}");
    }

    info!("pf anchor 'zq' loaded successfully");
    Ok(())
}

/// Unload (flush) all rules from anchor `zq` and restore the original
/// `/etc/pf.conf` as the main ruleset (removing our anchor references).
pub fn unload_rules() -> Result<()> {
    info!("unloading pf anchor 'zq'");

    // Flush the anchor.
    let output = Command::new("pfctl")
        .args(["-a", "zq", "-F", "all"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to run pfctl -a zq -F all")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("No such") {
            bail!("pfctl -a zq -F all failed: {stderr}");
        }
    }

    // Restore the original /etc/pf.conf (removes our anchor references).
    let restore = Command::new("pfctl")
        .args(["-f", "/etc/pf.conf"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to restore /etc/pf.conf")?;

    if !restore.status.success() {
        let stderr = String::from_utf8_lossy(&restore.stderr);
        // Non-fatal — the anchor is already flushed.
        info!("warning: pfctl -f /etc/pf.conf returned: {stderr}");
    }

    info!("pf anchor 'zq' unloaded");
    Ok(())
}

/// Check whether the `zq` pf anchor has any rules loaded.
pub fn is_loaded() -> bool {
    let output = Command::new("pfctl")
        .args(["-a", "zq", "-sr"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // If there are any non-empty lines, rules are loaded.
            stdout.lines().any(|line| !line.trim().is_empty())
        }
        Err(_) => false,
    }
}
