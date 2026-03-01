//! zq-proto: Shared IPC protocol types and framing for zq.
//!
//! All messages are length-prefixed: 4-byte LE u32 length, then JSON payload.

pub mod config;

use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder};

// ---------------------------------------------------------------------------
// Core domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAction {
    Passthrough,
    RouteToProxy,
}

impl Default for RouteAction {
    fn default() -> Self {
        Self::Passthrough
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyStatus {
    Unknown,
    Reachable,
    Unreachable,
}

impl Default for ProxyStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

// ---------------------------------------------------------------------------
// TUI ↔ Daemon messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiCommand {
    Subscribe,
    SetAppRouting { bundle_id: String, action: RouteAction },
    SetGlobalRouting { action: RouteAction },
    GetState,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    pub pids: Vec<u32>,
    pub flow_count: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub routing: RouteAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowInfo {
    pub flow_id: u64,
    pub pid: u32,
    pub process_name: String,
    pub bundle_id: String,
    pub local_addr: String,
    pub remote_addr: String,
    pub proto: Proto,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub routing: RouteAction,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToTuiMessage {
    FullState {
        apps: Vec<AppInfo>,
        flows: Vec<FlowInfo>,
        proxy_status: ProxyStatus,
    },
    FlowUpdate {
        flow: FlowInfo,
    },
    FlowRemoved {
        flow_id: u64,
    },
    AppUpdate {
        app: AppInfo,
    },
    ProxyStatusUpdate {
        status: ProxyStatus,
    },
}

// ---------------------------------------------------------------------------
// Length-prefixed codec
// ---------------------------------------------------------------------------

/// Length-prefixed JSON codec. Wire format:
/// - 4 bytes: little-endian u32 payload length
/// - N bytes: JSON payload
///
/// Generic over the message type for reuse across different socket pairs.
#[derive(Debug, Default)]
pub struct LengthPrefixedCodec {
    /// Maximum allowed message size (default 16 MiB).
    max_length: usize,
}

impl LengthPrefixedCodec {
    pub fn new() -> Self {
        Self {
            max_length: 16 * 1024 * 1024,
        }
    }

    pub fn with_max_length(max_length: usize) -> Self {
        Self { max_length }
    }
}

const LENGTH_PREFIX_SIZE: usize = 4;

impl Decoder for LengthPrefixedCodec {
    type Item = BytesMut;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < LENGTH_PREFIX_SIZE {
            return Ok(None);
        }

        let mut length_bytes = [0u8; 4];
        length_bytes.copy_from_slice(&src[..4]);
        let length = u32::from_le_bytes(length_bytes) as usize;

        if length > self.max_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame too large: {length} > {}", self.max_length),
            ));
        }

        let total = LENGTH_PREFIX_SIZE + length;
        if src.len() < total {
            // Reserve space for the rest of the frame.
            src.reserve(total - src.len());
            return Ok(None);
        }

        // Skip length prefix, take payload.
        src.advance(LENGTH_PREFIX_SIZE);
        let payload = src.split_to(length);
        Ok(Some(payload))
    }
}

impl Encoder<bytes::Bytes> for LengthPrefixedCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: bytes::Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len = item.len();
        if len > self.max_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("payload too large: {len} > {}", self.max_length),
            ));
        }
        dst.reserve(LENGTH_PREFIX_SIZE + len);
        dst.put_u32_le(len as u32);
        dst.extend_from_slice(&item);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers: typed encode/decode over the raw codec
// ---------------------------------------------------------------------------

/// Serialize a message to length-prefixed bytes.
pub fn encode_message<T: Serialize>(msg: &T) -> Result<bytes::Bytes, serde_json::Error> {
    let json = serde_json::to_vec(msg)?;
    let mut buf = BytesMut::with_capacity(LENGTH_PREFIX_SIZE + json.len());
    buf.put_u32_le(json.len() as u32);
    buf.extend_from_slice(&json);
    Ok(buf.freeze())
}

/// Deserialize a message from a raw payload (no length prefix).
pub fn decode_message<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(payload)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_tui_command_roundtrip() {
        let msg = TuiCommand::SetAppRouting {
            bundle_id: "com.apple.Safari".to_string(),
            action: RouteAction::RouteToProxy,
        };

        let encoded = encode_message(&msg).unwrap();
        let payload = &encoded[4..];
        let decoded: TuiCommand = decode_message(payload).unwrap();

        match decoded {
            TuiCommand::SetAppRouting { bundle_id, action } => {
                assert_eq!(bundle_id, "com.apple.Safari");
                assert_eq!(action, RouteAction::RouteToProxy);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_daemon_to_tui_full_state_roundtrip() {
        let msg = DaemonToTuiMessage::FullState {
            apps: vec![AppInfo {
                bundle_id: "com.apple.Safari".to_string(),
                name: "Safari".to_string(),
                pids: vec![1234],
                flow_count: 5,
                bytes_in: 1024,
                bytes_out: 512,
                routing: RouteAction::Passthrough,
            }],
            flows: vec![],
            proxy_status: ProxyStatus::Reachable,
        };

        let encoded = encode_message(&msg).unwrap();
        let payload = &encoded[4..];
        let decoded: DaemonToTuiMessage = decode_message(payload).unwrap();

        match decoded {
            DaemonToTuiMessage::FullState {
                apps,
                flows,
                proxy_status,
            } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].name, "Safari");
                assert!(flows.is_empty());
                assert_eq!(proxy_status, ProxyStatus::Reachable);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_shutdown_command_roundtrip() {
        let msg = TuiCommand::Shutdown;
        let encoded = encode_message(&msg).unwrap();
        let payload = &encoded[4..];
        let decoded: TuiCommand = decode_message(payload).unwrap();
        assert!(matches!(decoded, TuiCommand::Shutdown));
    }

    #[test]
    fn test_codec_encode_decode() {
        let mut codec = LengthPrefixedCodec::new();
        let payload = b"hello world";
        let bytes = Bytes::from_static(payload);

        let mut buf = BytesMut::new();
        codec.encode(bytes, &mut buf).unwrap();

        // Should have 4 byte prefix + payload.
        assert_eq!(buf.len(), 4 + payload.len());

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(&decoded[..], payload);
    }

    #[test]
    fn test_codec_partial_read() {
        let mut codec = LengthPrefixedCodec::new();
        let payload = b"hello";
        let bytes = Bytes::from_static(payload);

        let mut buf = BytesMut::new();
        codec.encode(bytes, &mut buf).unwrap();

        // Split the buffer to simulate partial reads.
        let mut partial = buf.split_to(3); // Only 3 of 4 header bytes
        assert!(codec.decode(&mut partial).unwrap().is_none());

        // Feed the rest.
        partial.extend_from_slice(&buf);
        let decoded = codec.decode(&mut partial).unwrap().unwrap();
        assert_eq!(&decoded[..], payload);
    }

    #[test]
    fn test_codec_rejects_oversized_frame() {
        let mut codec = LengthPrefixedCodec::with_max_length(10);
        let mut buf = BytesMut::new();
        buf.put_u32_le(100); // Claim 100 bytes
        buf.extend_from_slice(&[0u8; 100]);

        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_route_action_default() {
        assert_eq!(RouteAction::default(), RouteAction::Passthrough);
    }
}
