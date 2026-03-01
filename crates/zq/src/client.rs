use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use tracing::{debug, error, info, warn};
use zq_proto::*;

/// Client that connects to the zq daemon over a Unix socket
/// and exchanges length-prefixed JSON messages.
pub struct DaemonClient {
    inner: Option<Framed<UnixStream, LengthPrefixedCodec>>,
    socket_path: String,
}

impl DaemonClient {
    pub fn new(socket_path: String) -> Self {
        Self {
            inner: None,
            socket_path,
        }
    }

    /// Attempt to connect to the daemon's TUI socket.
    /// On success, immediately sends `Subscribe` and `GetState` commands.
    pub async fn connect(&mut self) -> Result<()> {
        info!("connecting to daemon at {}", self.socket_path);

        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("failed to connect to {}", self.socket_path))?;

        let framed = Framed::new(stream, LengthPrefixedCodec::new());
        self.inner = Some(framed);

        // Request subscription and full state dump.
        self.send(TuiCommand::Subscribe).await?;
        self.send(TuiCommand::GetState).await?;

        info!("connected to daemon");
        Ok(())
    }

    /// Read the next message from the daemon. Returns `None` if disconnected
    /// or the stream has ended.
    pub async fn recv(&mut self) -> Option<DaemonToTuiMessage> {
        let framed = self.inner.as_mut()?;

        match framed.next().await {
            Some(Ok(payload)) => match decode_message::<DaemonToTuiMessage>(&payload) {
                Ok(msg) => {
                    debug!("recv: {:?}", msg);
                    Some(msg)
                }
                Err(e) => {
                    error!("failed to decode daemon message: {e}");
                    None
                }
            },
            Some(Err(e)) => {
                error!("socket read error: {e}");
                self.disconnect();
                None
            }
            None => {
                warn!("daemon socket closed");
                self.disconnect();
                None
            }
        }
    }

    /// Send a command to the daemon.
    pub async fn send(&mut self, cmd: TuiCommand) -> Result<()> {
        let framed = self
            .inner
            .as_mut()
            .context("not connected to daemon")?;

        let json = serde_json::to_vec(&cmd)?;
        let bytes = Bytes::from(json);
        framed.send(bytes).await?;
        debug!("sent: {:?}", cmd);
        Ok(())
    }

    /// Whether we currently hold an open connection.
    pub fn is_connected(&self) -> bool {
        self.inner.is_some()
    }

    /// Drop the connection state so reconnect can be attempted.
    fn disconnect(&mut self) {
        if self.inner.take().is_some() {
            warn!("disconnected from daemon");
        }
    }
}
