#![allow(dead_code)]

//! Wire protocol shared by the relay agent and the relay server.
//!
//! Text messages are JSON with a `type` tag. Video frames are binary WebSocket
//! messages with a fixed 16-byte header:
//!
//! ```text
//! 0..2   magic "RS"
//! 2      protocol version (1)
//! 3      flags (bit 0 = keyframe)
//! 4..8   reserved
//! 8..16  timestamp in microseconds, little-endian u64
//! 16..   Annex-B H.264 access unit
//! ```

use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 1;
pub const FRAME_HEADER_LEN: usize = 16;

/// Agent -> relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToRelay {
    /// First message on an agent connection.
    Register {
        pin: String,
        host: String,
        source: String,
        version: String,
    },
    /// Viewer-facing messages forwarded 1:1 to the browser.
    Config {
        codec: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    Pong {
        ts: i64,
    },
    Reset {
        reason: String,
    },
    Error {
        message: String,
    },
}

/// Relay -> agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayToAgent {
    Registered { code: String },
    ViewerJoined { name: String },
    ViewerLeft,
    Input { event: serde_json::Value },
    Ping { ts: i64 },
}

/// Viewer -> relay. `input` and `ping` are forwarded verbatim to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewerToRelay {
    Connect {
        code: String,
        pin: String,
        #[serde(default)]
        name: String,
    },
    Input {
        event: serde_json::Value,
    },
    Ping {
        ts: i64,
    },
}

/// Relay -> viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayToViewer {
    Paired {
        host: String,
        source: String,
        version: String,
    },
    Denied {
        reason: String,
    },
    Config {
        codec: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    Pong {
        ts: i64,
    },
    Reset {
        reason: String,
    },
    Error {
        message: String,
    },
}

impl AgentToRelay {
    /// Convert agent messages that should reach the browser.
    pub fn to_viewer(&self) -> Option<RelayToViewer> {
        match self {
            AgentToRelay::Config {
                codec,
                width,
                height,
                fps,
            } => Some(RelayToViewer::Config {
                codec: codec.clone(),
                width: *width,
                height: *height,
                fps: *fps,
            }),
            AgentToRelay::Pong { ts } => Some(RelayToViewer::Pong { ts: *ts }),
            AgentToRelay::Reset { reason } => Some(RelayToViewer::Reset {
                reason: reason.clone(),
            }),
            AgentToRelay::Error { message } => Some(RelayToViewer::Error {
                message: message.clone(),
            }),
            AgentToRelay::Register { .. } => None,
        }
    }
}

/// Wrap an Annex-B access unit in the binary frame header.
pub fn encode_frame(data: &[u8], keyframe: bool, ts_us: u64) -> Bytes {
    let mut out = BytesMut::with_capacity(FRAME_HEADER_LEN + data.len());
    out.extend_from_slice(b"RS");
    out.extend_from_slice(&[PROTOCOL_VERSION, if keyframe { 1 } else { 0 }]);
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&ts_us.to_le_bytes());
    out.extend_from_slice(data);
    out.freeze()
}

/// Strip the binary frame header. Returns `None` for malformed frames.
pub fn decode_frame(data: &[u8]) -> Option<(bool, u64, &[u8])> {
    if data.len() < FRAME_HEADER_LEN || &data[0..2] != b"RS" {
        return None;
    }
    let keyframe = data[3] & 1 == 1;
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&data[8..16]);
    Some((keyframe, u64::from_le_bytes(ts), &data[FRAME_HEADER_LEN..]))
}

/// Format a 9-digit code as `123 456 789`.
pub fn pretty_code(code: &str) -> String {
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut parts = Vec::new();
    let chars: Vec<char> = digits.chars().collect();
    for chunk in chars.chunks(3) {
        parts.push(chunk.iter().collect::<String>());
    }
    parts.join(" ")
}

pub fn normalize_code(input: &str) -> String {
    input.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Convert a public relay URL (`https://host`, `wss://host`, optional path)
/// into the agent WebSocket endpoint and the browser-facing base URL.
pub fn relay_endpoints(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/agent").unwrap_or(trimmed);
    let (scheme, rest) = trimmed.split_once("://")?;
    let (ws_scheme, http_scheme) = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => ("wss", "https"),
        "http" | "ws" => ("ws", "http"),
        _ => return None,
    };
    if rest.is_empty() {
        return None;
    }
    Some((
        format!("{ws_scheme}://{rest}/agent"),
        format!("{http_scheme}://{rest}/"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let encoded = encode_frame(b"\x00\x00\x00\x01\x65data", true, 123456);
        let (key, ts, payload) = decode_frame(&encoded).unwrap();
        assert!(key);
        assert_eq!(ts, 123456);
        assert_eq!(payload, b"\x00\x00\x00\x01\x65data");
        assert!(decode_frame(b"nope").is_none());
    }

    #[test]
    fn codes_are_formatted_and_normalized() {
        assert_eq!(pretty_code("123456789"), "123 456 789");
        assert_eq!(normalize_code(" 123-456-789 "), "123456789");
    }

    #[test]
    fn maps_relay_urls() {
        assert_eq!(
            relay_endpoints("https://abc.trycloudflare.com").unwrap(),
            (
                "wss://abc.trycloudflare.com/agent".into(),
                "https://abc.trycloudflare.com/".into()
            )
        );
        assert_eq!(
            relay_endpoints("http://127.0.0.1:8090/").unwrap(),
            (
                "ws://127.0.0.1:8090/agent".into(),
                "http://127.0.0.1:8090/".into()
            )
        );
        assert_eq!(
            relay_endpoints("wss://relay.example.com/agent").unwrap().0,
            "wss://relay.example.com/agent"
        );
        assert!(relay_endpoints("ftp://nope").is_none());
    }
}
