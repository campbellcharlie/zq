//! Transparent proxy server.
//!
//! Listens on 127.0.0.1:{proxy_port} and handles connections redirected by pf.
//! For each connection:
//! 1. DIOCNATLOOK -> original destination
//! 2. lsof -> owning PID -> process name + bundle ID
//! 3. Hub -> routing decision (passthrough vs proxy)
//! 4. Connect upstream (direct or via proxy)
//! 5. Bidirectional bridge with live byte counting

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};

use zq_proto::{Proto, RouteAction};

use crate::forwarder;
use crate::hub::Hub;
use crate::pf::PfDevice;
use crate::procinfo;

/// Monotonically increasing flow ID counter.
static NEXT_FLOW_ID: AtomicU64 = AtomicU64::new(1);

/// Buffer size for the counting bridge read loop.
const BRIDGE_BUF_SIZE: usize = 16 * 1024;

/// Interval for reporting live byte counts to the hub.
const BYTE_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Run the transparent proxy. Listens on 127.0.0.1:{proxy_port} and spawns
/// a task per accepted connection.
///
/// This function runs until cancelled.
pub async fn run(hub: Hub, pf: Arc<PfDevice>, proxy_port: u16, proxy_addr: String) -> Result<()> {
    let listen_addr = format!("127.0.0.1:{proxy_port}");
    let listener = TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind proxy on {listen_addr}"))?;

    info!(%listen_addr, "transparent proxy listening");

    loop {
        let (client_stream, client_addr) = listener.accept().await?;

        let hub = hub.clone();
        let pf = pf.clone();
        let proxy_addr = proxy_addr.clone();

        tokio::spawn(async move {
            let flow_id = NEXT_FLOW_ID.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = handle_connection(flow_id, client_stream, client_addr, hub, pf, &proxy_addr).await {
                debug!(flow_id, error = %e, "connection handler error");
            }
        });
    }
}

/// Handle a single redirected connection.
async fn handle_connection(
    flow_id: u64,
    client_stream: TcpStream,
    client_addr: std::net::SocketAddr,
    hub: Hub,
    pf: Arc<PfDevice>,
    proxy_addr: &str,
) -> Result<()> {
    // 1. Recover the original destination via DIOCNATLOOK.
    let client_v4 = match client_addr {
        std::net::SocketAddr::V4(v4) => v4,
        _ => anyhow::bail!("IPv6 not supported"),
    };

    let local_addr = client_stream
        .local_addr()
        .context("failed to get local addr")?;
    let listen_v4 = match local_addr {
        std::net::SocketAddr::V4(v4) => v4,
        _ => anyhow::bail!("IPv6 not supported"),
    };

    let orig_dest = pf
        .natlook(client_v4, listen_v4)
        .context("DIOCNATLOOK failed")?;

    debug!(flow_id, %client_v4, %orig_dest, "resolved original destination");

    // 2. Resolve the owning PID via lsof (blocking -- run on blocking thread).
    let lookup_addr = client_v4;
    let (pid, process_name, bundle_id) =
        tokio::task::spawn_blocking(move || {
            let pid = procinfo::find_pid_for_addr(lookup_addr).unwrap_or(0);
            let name = procinfo::resolve_process_name(pid);
            let bid = procinfo::resolve_bundle_id(pid);
            (pid, name, bid)
        })
        .await
        .context("PID resolution task failed")?;

    debug!(flow_id, pid, %process_name, %bundle_id, "resolved process");

    // 3. Register with hub and get routing decision.
    let is_tls = orig_dest.port() == 443;
    let action = hub.handle_flow_start(
        flow_id,
        pid,
        &process_name,
        &bundle_id,
        &client_v4.to_string(),
        &orig_dest.to_string(),
        Proto::Tcp,
    );

    // 4. Connect upstream.
    let upstream_stream = match action {
        RouteAction::Passthrough => {
            TcpStream::connect(std::net::SocketAddr::V4(orig_dest))
                .await
                .with_context(|| format!("failed to connect to upstream {orig_dest}"))?
        }
        RouteAction::RouteToProxy => {
            forwarder::forward_to_proxy(&orig_dest.to_string(), is_tls, proxy_addr)
                .await
                .with_context(|| format!("failed to forward to proxy for {orig_dest}"))?
        }
    };

    debug!(flow_id, ?action, %orig_dest, "upstream connected");

    // 5. Bidirectional bridge with live byte counting.
    let (bytes_in, bytes_out) = counting_bridge(flow_id, client_stream, upstream_stream, hub.clone()).await;

    // 6. Report flow end.
    hub.handle_flow_end(flow_id, bytes_in, bytes_out);

    debug!(flow_id, bytes_in, bytes_out, "flow complete");
    Ok(())
}

/// Counters shared between the bridge halves and the periodic reporter.
struct FlowCounters {
    bytes_in: Arc<AtomicU64>,
    bytes_out: Arc<AtomicU64>,
}

/// Bidirectional copy between client and upstream with live byte counting.
///
/// Instead of using `tokio::io::copy` (which only reports totals at the end),
/// this uses a manual read/write loop with atomic counters updated on each
/// chunk. A periodic reporter task pushes updates to the hub every second.
///
/// Returns (bytes_in, bytes_out) where:
/// - bytes_in = bytes from upstream to client (download)
/// - bytes_out = bytes from client to upstream (upload)
async fn counting_bridge(flow_id: u64, client: TcpStream, upstream: TcpStream, hub: Hub) -> (u64, u64) {
    let counters = FlowCounters {
        bytes_in: Arc::new(AtomicU64::new(0)),
        bytes_out: Arc::new(AtomicU64::new(0)),
    };

    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();

    // Periodic reporter: pushes byte counts to the hub every second.
    let reporter = {
        let bi = counters.bytes_in.clone();
        let bo = counters.bytes_out.clone();
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(BYTE_REPORT_INTERVAL);
            interval.tick().await; // consume immediate first tick
            loop {
                interval.tick().await;
                let current_in = bi.load(Ordering::Relaxed);
                let current_out = bo.load(Ordering::Relaxed);
                hub.update_flow_bytes(flow_id, current_in, current_out);
            }
        })
    };

    // client -> upstream (outbound)
    let out_counter = counters.bytes_out.clone();
    let out_handle = tokio::spawn(async move {
        let mut cr = client_read;
        let mut uw = upstream_write;
        let mut buf = vec![0u8; BRIDGE_BUF_SIZE];
        let mut total = 0u64;
        loop {
            match cr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if uw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    total += n as u64;
                    out_counter.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
        let _ = uw.shutdown().await;
        total
    });

    // upstream -> client (inbound)
    let in_counter = counters.bytes_in.clone();
    let in_handle = tokio::spawn(async move {
        let mut ur = upstream_read;
        let mut cw = client_write;
        let mut buf = vec![0u8; BRIDGE_BUF_SIZE];
        let mut total = 0u64;
        loop {
            match ur.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    total += n as u64;
                    in_counter.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
        let _ = cw.shutdown().await;
        total
    });

    let (bytes_out, bytes_in) = tokio::join!(out_handle, in_handle);
    let bytes_out = bytes_out.unwrap_or(0);
    let bytes_in = bytes_in.unwrap_or(0);

    // Stop the periodic reporter.
    reporter.abort();

    (bytes_in, bytes_out)
}
