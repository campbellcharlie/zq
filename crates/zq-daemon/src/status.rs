//! One-shot status check for the ZQ system.
//!
//! Reports daemon connectivity, pf anchor state, proxy listener,
//! and upstream proxy reachability.

use std::net::TcpStream;
use std::time::Duration;

use anyhow::Result;

use crate::pf;
use zq_proto::config::Config;

/// Timeout for the blocking TCP connect to the upstream proxy.
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Print a human-readable status summary to stdout.
pub fn run() -> Result<()> {
    let config = Config::load();

    println!("zq status");
    println!("=========");

    // Daemon: can we connect to the TUI socket?
    let daemon_running = check_unix_socket(&config.socket_path);
    println!(
        "Daemon:     {}",
        if daemon_running {
            "running"
        } else {
            "not running"
        }
    );

    // PF anchor: are redirect rules loaded?
    let pf_loaded = pf::is_loaded();
    println!(
        "PF anchor:  {}",
        if pf_loaded { "loaded" } else { "not loaded" }
    );

    // Proxy listener: can we connect to the proxy port?
    let proxy_addr = format!("127.0.0.1:{}", config.proxy_port);
    let proxy_listening = TcpStream::connect_timeout(
        &proxy_addr.parse().expect("valid socket addr"),
        Duration::from_millis(500),
    )
    .is_ok();
    println!(
        "Proxy:      {} ({})",
        if proxy_listening {
            "listening"
        } else {
            "not listening"
        },
        proxy_addr,
    );

    // Upstream proxy: blocking TCP connect with a short timeout.
    let upstream_ok = TcpStream::connect_timeout(
        &config
            .proxy_addr
            .parse()
            .expect("proxy_addr is a valid socket addr"),
        PROXY_CONNECT_TIMEOUT,
    )
    .is_ok();
    println!(
        "Upstream:   {} ({})",
        if upstream_ok {
            "reachable"
        } else {
            "unreachable"
        },
        config.proxy_addr,
    );

    Ok(())
}

/// Try connecting to a Unix domain socket. Returns `true` if the connect
/// succeeds (indicating something is listening).
fn check_unix_socket(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}
