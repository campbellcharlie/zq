//! Process identification via libproc and lsof.
//!
//! Uses `lsof` to resolve socket ownership (local_addr → PID), and
//! macOS `libproc` for PID → process name / bundle ID resolution.

use std::ffi::CStr;
use std::net::SocketAddrV4;
use std::process::Command;

use tracing::{debug, trace};

// ---------------------------------------------------------------------------
// libproc FFI bindings (for name/path resolution only)
// ---------------------------------------------------------------------------

extern "C" {
    fn proc_name(pid: i32, buffer: *mut libc::c_char, buffersize: u32) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut libc::c_char, buffersize: u32) -> i32;
}

// ---------------------------------------------------------------------------
// PID lookup via lsof
// ---------------------------------------------------------------------------

/// Find the PID that owns a TCP socket with the given local address.
///
/// Uses `lsof -i TCP@<ip>:<port> -n -P -F p` which is reliable across
/// macOS versions, though slower than a direct proc_pidinfo approach.
///
/// Excludes our own PID from the results, since the proxy's accepted
/// socket also matches the address (as its remote end).
pub fn find_pid_for_addr(addr: SocketAddrV4) -> Option<u32> {
    let addr_str = format!("{}:{}", addr.ip(), addr.port());
    let our_pid = std::process::id();

    let output = Command::new("lsof")
        .args([
            "-i", &format!("TCP@{addr_str}"),
            "-n", "-P",
            "-F", "p",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    // lsof -F p outputs lines like "p1234\n" — the 'p' prefix followed by PID.
    for line in text.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid == our_pid {
                    continue; // skip proxy's own socket
                }
                trace!(%addr, pid, "lsof resolved PID");
                return Some(pid);
            }
        }
    }

    debug!(%addr, "lsof found no owning PID");
    None
}

// ---------------------------------------------------------------------------
// Process resolution
// ---------------------------------------------------------------------------

/// Resolve a PID to its process name using `proc_name()`.
pub fn resolve_process_name(pid: u32) -> String {
    let mut buf = [0u8; 256];
    let ret = unsafe {
        proc_name(
            pid as i32,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len() as u32,
        )
    };

    if ret > 0 {
        let name = unsafe { CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
        name.to_string_lossy().into_owned()
    } else {
        "unknown".to_string()
    }
}

/// Resolve a PID to a bundle-like identifier using `proc_pidpath()`.
///
/// Extracts the `.app` bundle name from the executable path and produces
/// a synthetic bundle ID like `app.safari`. Falls back to `proc.<name>`.
pub fn resolve_bundle_id(pid: u32) -> String {
    let mut buf = [0u8; 4096];
    let ret = unsafe {
        proc_pidpath(
            pid as i32,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len() as u32,
        )
    };

    if ret > 0 {
        let path = unsafe { CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
        let path_str = path.to_string_lossy();

        // Look for ".app/" in the path.
        if let Some(app_idx) = path_str.find(".app/") {
            let bundle_path = &path_str[..app_idx + 4]; // include ".app"
            let app_name = bundle_path
                .rsplit('/')
                .next()
                .unwrap_or(bundle_path)
                .strip_suffix(".app")
                .unwrap_or(bundle_path);

            if !app_name.is_empty() {
                return format!("app.{}", app_name.to_lowercase());
            }
        }
    }

    // Fallback: use process name.
    let name = resolve_process_name(pid);
    format!("proc.{name}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_process_name_for_self() {
        let name = resolve_process_name(std::process::id());
        assert!(!name.is_empty());
        assert_ne!(name, "unknown");
    }

    #[test]
    fn test_resolve_process_name_nonexistent() {
        let name = resolve_process_name(4_000_000_000);
        assert_eq!(name, "unknown");
    }

    #[test]
    fn test_resolve_bundle_id_fallback() {
        let bid = resolve_bundle_id(std::process::id());
        assert!(bid.starts_with("proc."));
    }

    #[test]
    fn test_find_pid_nonexistent_addr() {
        // Random address nobody is listening on.
        let result = find_pid_for_addr(SocketAddrV4::new(
            std::net::Ipv4Addr::LOCALHOST,
            59999,
        ));
        assert!(result.is_none());
    }
}
