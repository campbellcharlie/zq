//! Unix socket server for TUI clients.
//!
//! Listens on the configured socket path and accepts multiple simultaneous TUI
//! connections. Each client can subscribe to live updates and send commands
//! to control routing behaviour.

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::prelude::*;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

use zq_proto::{decode_message, DaemonToTuiMessage, LengthPrefixedCodec, TuiCommand};

use crate::hub::Hub;

/// Start the TUI-facing server. Runs until cancelled.
pub async fn run(hub: Hub, socket_path: &str) -> Result<()> {
    // Remove a stale socket file if it exists from a previous run.
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .context("failed to bind TUI socket")?;

    // Allow non-root TUI clients to connect when the daemon runs as root.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o777))
        .context("failed to chmod TUI socket")?;

    info!(path = socket_path, "tui_server listening");

    loop {
        let (stream, _addr) = listener.accept().await?;
        info!("TUI client connected");

        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tui_connection(hub, stream).await {
                warn!("TUI connection error: {e}");
            }
            info!("TUI client disconnected");
        });
    }
}

/// Handle a single TUI client connection.
///
/// The client can send `TuiCommand` messages. Upon receiving a `Subscribe`
/// command the server begins forwarding live `DaemonToTuiMessage` updates
/// via a channel registered with the hub.
async fn handle_tui_connection(hub: Hub, stream: tokio::net::UnixStream) -> Result<()> {
    let mut framed = Framed::new(stream, LengthPrefixedCodec::new());

    // The subscription channel is created lazily on the first Subscribe
    // command. We keep it as an Option so we can set it up once.
    let mut update_rx: Option<mpsc::UnboundedReceiver<DaemonToTuiMessage>> = None;

    loop {
        tokio::select! {
            // Branch 1: incoming command from the TUI client.
            frame = framed.next() => {
                let frame = match frame {
                    Some(Ok(f)) => f,
                    Some(Err(e)) => {
                        warn!("TUI read error: {e}");
                        break;
                    }
                    None => break, // client disconnected
                };

                let cmd: TuiCommand = match decode_message(&frame) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("failed to decode TuiCommand: {e}");
                        continue;
                    }
                };

                debug!(?cmd, "received from TUI");

                match cmd {
                    TuiCommand::Subscribe => {
                        // Register for live updates if not already subscribed.
                        if update_rx.is_none() {
                            update_rx = Some(hub.subscribe_tui());
                            debug!("TUI client subscribed to updates");
                        }

                        // Always send the current full state on subscribe.
                        let state = hub.get_full_state();
                        send_message(&mut framed, &state).await?;
                    }

                    TuiCommand::GetState => {
                        let state = hub.get_full_state();
                        send_message(&mut framed, &state).await?;
                    }

                    TuiCommand::Shutdown => {
                        hub.handle_tui_command(TuiCommand::Shutdown);
                    }

                    other => {
                        if let Some(response) = hub.handle_tui_command(other) {
                            send_message(&mut framed, &response).await?;
                        }
                    }
                }
            }

            // Branch 2: outbound update from the hub (only if subscribed).
            msg = async {
                match update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => {
                        // No subscription yet -- park this branch forever.
                        std::future::pending::<Option<DaemonToTuiMessage>>().await
                    }
                }
            } => {
                match msg {
                    Some(update) => {
                        if let Err(e) = send_message(&mut framed, &update).await {
                            warn!("failed to send update to TUI client: {e}");
                            break;
                        }
                    }
                    None => {
                        // Hub dropped the sender -- should not happen in practice.
                        debug!("update channel closed, disconnecting TUI client");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Serialize a `DaemonToTuiMessage` and send it over the framed connection.
async fn send_message(
    framed: &mut Framed<tokio::net::UnixStream, LengthPrefixedCodec>,
    msg: &DaemonToTuiMessage,
) -> Result<()> {
    let json = serde_json::to_vec(msg).context("failed to serialize DaemonToTuiMessage")?;
    let payload = Bytes::from(json);
    framed.send(payload).await.context("failed to send frame")?;
    Ok(())
}
