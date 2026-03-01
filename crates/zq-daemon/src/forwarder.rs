//! Upstream proxy integration: health checking and connection forwarding.
//!
//! This module provides two main capabilities:
//!
//! 1. **Health checking** -- `health_check_loop` periodically probes the configured
//!    proxy address with a TCP connect and updates the hub's `ProxyStatus`.
//!
//! 2. **Traffic bridging** -- `ProxyForwarder` manages connections to the upstream
//!    proxy and performs bidirectional data bridging between intercepted flows and
//!    the proxy. For TLS flows it issues an HTTP CONNECT to establish a tunnel;
//!    for plain HTTP it forwards data directly (proxy acts as a forward proxy).

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};

use zq_proto::ProxyStatus;

use crate::hub::Hub;

/// Interval between proxy reachability checks.
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Timeout for each TCP connect attempt during the health check.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for establishing a new proxy connection during bridging.
#[allow(dead_code)]
const BRIDGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Buffer size for reads from the proxy TCP stream.
#[allow(dead_code)]
const PROXY_READ_BUF_SIZE: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Health check loop
// ---------------------------------------------------------------------------

/// Continuously check whether the upstream proxy is reachable and update the hub.
///
/// Runs forever (until the task is cancelled).
pub async fn health_check_loop(hub: Hub, proxy_addr: String) {
    loop {
        let status = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect(&proxy_addr),
        )
        .await
        {
            Ok(Ok(_stream)) => {
                debug!("proxy reachable at {proxy_addr}");
                ProxyStatus::Reachable
            }
            Ok(Err(e)) => {
                debug!("proxy unreachable: {e}");
                ProxyStatus::Unreachable
            }
            Err(_elapsed) => {
                debug!("proxy connect timed out");
                ProxyStatus::Unreachable
            }
        };

        hub.set_proxy_status(status);

        tokio::time::sleep(HEALTH_CHECK_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// ProxyForwarder
// ---------------------------------------------------------------------------

/// Manages connections to the upstream proxy and bridges intercepted flow data
/// through it.
///
/// `ProxyForwarder` is cheap to clone and carries no mutable state of its own --
/// each call to `bridge_flow` establishes an independent TCP connection to the proxy.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProxyForwarder {
    /// Address of the upstream proxy.
    proxy_addr: String,
}

#[allow(dead_code)]
impl ProxyForwarder {
    /// Create a new `ProxyForwarder` targeting the given proxy address.
    pub fn new(proxy_addr: String) -> Self {
        Self { proxy_addr }
    }

    /// Bridge a single flow through the upstream proxy.
    ///
    /// # Arguments
    ///
    /// * `remote_host` -- the original destination hostname or IP
    /// * `remote_port` -- the original destination port
    /// * `is_tls` -- if true, issue an HTTP CONNECT to tunnel through the proxy
    /// * `flow_reader` -- receives data chunks from the intercepted flow (client side)
    /// * `flow_writer` -- sends data chunks back to the intercepted flow (server responses)
    ///
    /// The method runs until either side closes the connection or an error occurs.
    /// Errors are logged and returned, but are not fatal to the daemon.
    #[instrument(
        skip(self, flow_reader, flow_writer),
        fields(proxy = %self.proxy_addr)
    )]
    pub async fn bridge_flow(
        &self,
        remote_host: &str,
        remote_port: u16,
        is_tls: bool,
        mut flow_reader: mpsc::Receiver<Bytes>,
        flow_writer: mpsc::Sender<Bytes>,
    ) -> Result<()> {
        // 1. Connect to the proxy.
        let mut stream = tokio::time::timeout(
            BRIDGE_CONNECT_TIMEOUT,
            TcpStream::connect(&self.proxy_addr),
        )
        .await
        .context("timed out connecting to proxy")?
        .context("failed to connect to proxy")?;

        debug!("connected to proxy");

        // 2. For TLS, establish a CONNECT tunnel.
        if is_tls {
            let target = format!("{remote_host}:{remote_port}");
            let connect_req =
                format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");

            stream
                .write_all(connect_req.as_bytes())
                .await
                .context("failed to send CONNECT request to proxy")?;

            // Read the proxy's response. We only need the status line to confirm
            // the tunnel was established. Read up to 1 KiB -- CONNECT responses
            // are small.
            let mut buf = [0u8; 1024];
            let n = stream
                .read(&mut buf)
                .await
                .context("failed to read CONNECT response from proxy")?;

            if n == 0 {
                anyhow::bail!(
                    "proxy closed connection before sending CONNECT response for {target}"
                );
            }

            let response = String::from_utf8_lossy(&buf[..n]);

            // Minimal validation: the status line must contain "200".
            // A typical response is "HTTP/1.1 200 Connection established\r\n\r\n".
            if !response.contains("200") {
                anyhow::bail!(
                    "proxy rejected CONNECT for {target}: {response}"
                );
            }

            debug!(%target, "CONNECT tunnel established through proxy");
        } else {
            debug!(
                host = %remote_host,
                port = remote_port,
                "plain HTTP bridge to proxy established"
            );
        }

        // 3. Bidirectional bridge: flow_reader -> proxy, proxy -> flow_writer.
        let (mut proxy_read, mut proxy_write) = stream.into_split();

        let bridge_result: Result<()> = async {
            let mut read_buf = vec![0u8; PROXY_READ_BUF_SIZE];

            loop {
                tokio::select! {
                    // Data from the intercepted flow -> write to proxy.
                    chunk = flow_reader.recv() => {
                        match chunk {
                            Some(data) => {
                                proxy_write
                                    .write_all(&data)
                                    .await
                                    .context("failed to write to proxy stream")?;
                            }
                            None => {
                                // flow_reader closed -- the client side is done.
                                debug!("flow reader closed, shutting down proxy write half");
                                proxy_write
                                    .shutdown()
                                    .await
                                    .context("failed to shutdown proxy write half")?;
                                break;
                            }
                        }
                    }

                    // Data from proxy -> send to the intercepted flow.
                    result = proxy_read.read(&mut read_buf) => {
                        match result {
                            Ok(0) => {
                                // Proxy closed its side.
                                debug!("proxy stream EOF, bridge complete");
                                break;
                            }
                            Ok(n) => {
                                let data = Bytes::copy_from_slice(&read_buf[..n]);
                                if flow_writer.send(data).await.is_err() {
                                    // flow_writer receiver dropped -- client gone.
                                    debug!("flow writer receiver dropped, ending bridge");
                                    break;
                                }
                            }
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "error reading from proxy stream: {e}"
                                ));
                            }
                        }
                    }
                }
            }

            Ok(())
        }
        .await;

        match &bridge_result {
            Ok(()) => {
                info!(
                    host = %remote_host,
                    port = remote_port,
                    is_tls,
                    "bridge flow completed"
                );
            }
            Err(e) => {
                warn!(
                    host = %remote_host,
                    port = remote_port,
                    is_tls,
                    error = %e,
                    "bridge flow ended with error"
                );
            }
        }

        bridge_result
    }

    /// One-shot check: can we establish a TCP connection to the proxy?
    pub async fn check_reachable(&self) -> bool {
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.proxy_addr))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    }
}

/// Convenience function: one-shot reachability check using a given proxy address.
#[allow(dead_code)]
pub async fn check_reachable(proxy_addr: &str) -> bool {
    ProxyForwarder::new(proxy_addr.to_string())
        .check_reachable()
        .await
}

/// Establish a TCP connection to the upstream proxy for forwarding traffic.
///
/// For TLS connections (identified by `is_tls`), this sends an HTTP CONNECT
/// request to tunnel through the proxy. For plain HTTP, it simply returns
/// the connected stream.
///
/// Returns the connected `TcpStream` to the proxy. Callers that need
/// full bidirectional bridging should prefer `ProxyForwarder::bridge_flow`.
pub async fn forward_to_proxy(remote_addr: &str, is_tls: bool, proxy_addr: &str) -> Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .context("failed to connect to proxy")?;

    if is_tls {
        // Send an HTTP CONNECT request so the proxy can tunnel the TLS connection.
        let connect_req = format!(
            "CONNECT {remote_addr} HTTP/1.1\r\nHost: {remote_addr}\r\n\r\n"
        );
        stream
            .write_all(connect_req.as_bytes())
            .await
            .context("failed to send CONNECT request")?;

        // Read the proxy's response. We only need the status line to confirm
        // the tunnel was established. A full implementation would parse the
        // complete HTTP response; for now we read up to 1 KiB and check for
        // a 200 status.
        let mut buf = [0u8; 1024];
        let n = stream
            .read(&mut buf)
            .await
            .context("failed to read CONNECT response")?;

        let response = String::from_utf8_lossy(&buf[..n]);
        if !response.contains("200") {
            anyhow::bail!(
                "proxy rejected CONNECT for {remote_addr}: {response}"
            );
        }

        debug!(%remote_addr, "CONNECT tunnel established through proxy");
    } else {
        debug!(%remote_addr, "plain HTTP connection to proxy established");
    }

    Ok(stream)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Helper: spin up a mock proxy on a random port that accepts a
    /// CONNECT request, replies 200, then echoes data back.
    async fn mock_proxy_connect_echo() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();

            // Read the CONNECT request.
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("CONNECT "), "expected CONNECT, got: {req}");

            // Reply 200.
            sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();

            // Echo loop: read data and send it back.
            let mut echo_buf = [0u8; 4096];
            loop {
                match sock.read(&mut echo_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&echo_buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        (addr, handle)
    }

    /// Helper: spin up a mock proxy for plain HTTP that echoes data.
    async fn mock_proxy_plain_echo() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();

            // Echo loop.
            let mut echo_buf = [0u8; 4096];
            loop {
                match sock.read(&mut echo_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&echo_buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        (addr, handle)
    }

    #[tokio::test]
    async fn test_bridge_flow_tls_connect() {
        let (addr, server) = mock_proxy_connect_echo().await;
        let fwd = ProxyForwarder::new(addr);

        let (flow_tx, flow_rx) = mpsc::channel::<Bytes>(16);
        let (resp_tx, mut resp_rx) = mpsc::channel::<Bytes>(16);

        let bridge = tokio::spawn(async move {
            fwd.bridge_flow("example.com", 443, true, flow_rx, resp_tx)
                .await
        });

        // Send some data through the bridge.
        flow_tx.send(Bytes::from("hello from client")).await.unwrap();

        // Should get echoed back.
        let echoed = tokio::time::timeout(Duration::from_secs(2), resp_rx.recv())
            .await
            .expect("timeout waiting for echo")
            .expect("channel closed");

        assert_eq!(&echoed[..], b"hello from client");

        // Close the flow reader to signal the client is done.
        drop(flow_tx);

        // Bridge should complete.
        let result = tokio::time::timeout(Duration::from_secs(2), bridge)
            .await
            .expect("timeout waiting for bridge")
            .expect("bridge task panicked");

        assert!(result.is_ok(), "bridge returned error: {:?}", result.err());

        server.abort();
    }

    #[tokio::test]
    async fn test_bridge_flow_plain_http() {
        let (addr, server) = mock_proxy_plain_echo().await;
        let fwd = ProxyForwarder::new(addr);

        let (flow_tx, flow_rx) = mpsc::channel::<Bytes>(16);
        let (resp_tx, mut resp_rx) = mpsc::channel::<Bytes>(16);

        let bridge = tokio::spawn(async move {
            fwd.bridge_flow("example.com", 80, false, flow_rx, resp_tx)
                .await
        });

        flow_tx
            .send(Bytes::from("GET / HTTP/1.1\r\n\r\n"))
            .await
            .unwrap();

        let echoed = tokio::time::timeout(Duration::from_secs(2), resp_rx.recv())
            .await
            .expect("timeout waiting for echo")
            .expect("channel closed");

        assert_eq!(&echoed[..], b"GET / HTTP/1.1\r\n\r\n");

        drop(flow_tx);

        let result = tokio::time::timeout(Duration::from_secs(2), bridge)
            .await
            .expect("timeout waiting for bridge")
            .expect("bridge task panicked");

        assert!(result.is_ok(), "bridge returned error: {:?}", result.err());

        server.abort();
    }

    #[tokio::test]
    async fn test_check_reachable_returns_false_when_nothing_listens() {
        // Use a port that is extremely unlikely to be in use.
        let fwd = ProxyForwarder::new("127.0.0.1:19999".to_string());
        assert!(!fwd.check_reachable().await);
    }

    #[tokio::test]
    async fn test_check_reachable_returns_true_when_listening() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let fwd = ProxyForwarder::new(addr);
        assert!(fwd.check_reachable().await);

        drop(listener);
    }

    #[tokio::test]
    async fn test_bridge_flow_connect_rejected() {
        // Mock proxy that rejects CONNECT with 403.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _n = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .await
                .unwrap();
        });

        let fwd = ProxyForwarder::new(addr);
        let (_flow_tx, flow_rx) = mpsc::channel::<Bytes>(16);
        let (resp_tx, _resp_rx) = mpsc::channel::<Bytes>(16);

        let result = fwd
            .bridge_flow("evil.com", 443, true, flow_rx, resp_tx)
            .await;

        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("rejected CONNECT"),
            "unexpected error: {err_msg}"
        );

        server.abort();
    }
}
