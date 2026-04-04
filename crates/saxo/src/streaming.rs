//! Saxo Bank WebSocket binary frame parser.
//!
//! Saxo's streaming API sends a proprietary binary envelope over WebSocket.
//! Each WebSocket binary message contains one or more concatenated messages
//! with the following per-message layout:
//!
//! | Offset | Size    | Field                              |
//! |--------|---------|------------------------------------|
//! | 0      | 8 bytes | Message ID (u64 LE)                |
//! | 8      | 2 bytes | Reserved (skip)                    |
//! | 10     | 1 byte  | Reference ID length S (u8)         |
//! | 11     | S bytes | Reference ID (ASCII)               |
//! | 11+S   | 1 byte  | Payload format (0 = JSON)          |
//! | 12+S   | 4 bytes | Payload size P (u32 LE)            |
//! | 16+S   | P bytes | Payload bytes                      |
//!
//! Total message size: 16 + S + P bytes.

use super::error::SaxoError;

/// A single parsed message from a Saxo streaming frame.
#[derive(Debug, Clone)]
pub struct SaxoMessage {
    /// Server-assigned monotonic message ID.
    #[allow(dead_code)]
    pub message_id: u64,
    /// Reference ID identifying the subscription or control channel.
    pub reference_id: String,
    /// Raw payload bytes (JSON when payload format is 0).
    pub payload: Vec<u8>,
}

impl SaxoMessage {
    /// Returns `true` if this is a control message (reference ID starts with `_`).
    pub fn is_control(&self) -> bool {
        self.reference_id.starts_with('_')
    }

    /// Deserialize the JSON payload into `T`.
    pub fn deserialize_json<T: serde::de::DeserializeOwned>(&self) -> Result<T, SaxoError> {
        Ok(serde_json::from_slice(&self.payload)?)
    }
}

/// Parse a binary WebSocket frame into zero or more `SaxoMessage`s.
///
/// An empty frame (`data.is_empty()`) returns `Ok(vec![])`.
/// Truncated or malformed frames return `SaxoError::InvalidFrame`.
/// Unsupported payload format bytes return `SaxoError::UnsupportedPayloadFormat`.
pub fn parse_frame(data: &[u8]) -> Result<Vec<SaxoMessage>, SaxoError> {
    let mut messages = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        // Message ID: 8 bytes LE
        let message_id = read_u64_le(data, offset, "message_id")?;
        offset += 8;

        // Reserved: 2 bytes (skip)
        if offset + 2 > data.len() {
            return Err(SaxoError::InvalidFrame(
                "truncated at reserved bytes".to_string(),
            ));
        }
        offset += 2;

        // Reference ID length: 1 byte
        if offset >= data.len() {
            return Err(SaxoError::InvalidFrame(
                "truncated at reference ID length".to_string(),
            ));
        }
        let ref_id_len = data[offset] as usize;
        offset += 1;

        // Reference ID: ref_id_len bytes
        if offset + ref_id_len > data.len() {
            return Err(SaxoError::InvalidFrame(
                "truncated at reference ID".to_string(),
            ));
        }
        let ref_id_bytes = &data[offset..offset + ref_id_len];
        let reference_id = std::str::from_utf8(ref_id_bytes)
            .map_err(|e| SaxoError::InvalidFrame(format!("invalid UTF-8 in reference ID: {e}")))?;
        offset += ref_id_len;

        // Payload format: 1 byte (0 = JSON)
        if offset >= data.len() {
            return Err(SaxoError::InvalidFrame(
                "truncated at payload format".to_string(),
            ));
        }
        let payload_format = data[offset];
        offset += 1;
        if payload_format != 0 {
            return Err(SaxoError::UnsupportedPayloadFormat(payload_format));
        }

        // Payload size: 4 bytes LE
        let payload_size = read_u32_le(data, offset, "payload_size")? as usize;
        offset += 4;

        // Payload: payload_size bytes
        if offset + payload_size > data.len() {
            return Err(SaxoError::InvalidFrame("truncated at payload".to_string()));
        }
        let payload = data[offset..offset + payload_size].to_vec();
        offset += payload_size;

        messages.push(SaxoMessage {
            message_id,
            reference_id: reference_id.to_string(),
            payload,
        });
    }

    Ok(messages)
}

/// Read a little-endian u64 from `data` at `offset`, with bounds checking.
fn read_u64_le(data: &[u8], offset: usize, field: &str) -> Result<u64, SaxoError> {
    if offset + 8 > data.len() {
        return Err(SaxoError::InvalidFrame(format!("truncated at {field}")));
    }
    let bytes: [u8; 8] = data[offset..offset + 8]
        .try_into()
        .expect("slice length verified above");
    Ok(u64::from_le_bytes(bytes))
}

/// Read a little-endian u32 from `data` at `offset`, with bounds checking.
fn read_u32_le(data: &[u8], offset: usize, field: &str) -> Result<u32, SaxoError> {
    if offset + 4 > data.len() {
        return Err(SaxoError::InvalidFrame(format!("truncated at {field}")));
    }
    let bytes: [u8; 4] = data[offset..offset + 4]
        .try_into()
        .expect("slice length verified above");
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a single binary message with the given fields.
    fn build_message(message_id: u64, reference_id: &str, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&message_id.to_le_bytes()); // 8 bytes
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.push(reference_id.len() as u8); // ref ID length
        buf.extend_from_slice(reference_id.as_bytes()); // ref ID
        buf.push(0); // payload format = JSON
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // payload size
        buf.extend_from_slice(payload); // payload
        buf
    }

    #[test]
    fn single_json_message() {
        let payload = br#"{"price":1.23}"#;
        let frame = build_message(42, "quotes_1", payload);

        let msgs = parse_frame(&frame).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_id, 42);
        assert_eq!(msgs[0].reference_id, "quotes_1");
        assert_eq!(msgs[0].payload, payload);
    }

    #[test]
    fn multiple_concatenated_messages() {
        let mut frame = build_message(1, "a", b"{}");
        frame.extend(build_message(2, "b", b"[]"));
        frame.extend(build_message(3, "c", b"null"));

        let msgs = parse_frame(&frame).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].reference_id, "a");
        assert_eq!(msgs[1].reference_id, "b");
        assert_eq!(msgs[2].reference_id, "c");
    }

    #[test]
    fn empty_frame_returns_empty_vec() {
        let msgs = parse_frame(&[]).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn heartbeat_is_control() {
        let frame = build_message(1, "_heartbeat", b"{}");
        let msgs = parse_frame(&frame).unwrap();
        assert!(msgs[0].is_control());
    }

    #[test]
    fn regular_message_not_control() {
        let frame = build_message(4, "quotes_1", b"{}");
        let msgs = parse_frame(&frame).unwrap();
        assert!(!msgs[0].is_control());
    }

    #[test]
    fn truncated_at_message_id() {
        let err = parse_frame(&[0u8; 5]).unwrap_err();
        assert!(
            err.to_string().contains("truncated at message_id"),
            "got: {err}"
        );
    }

    #[test]
    fn unsupported_payload_format() {
        let mut data = vec![0u8; 10]; // msg_id + reserved
        data.push(1); // ref_id_len
        data.push(b'x'); // ref_id
        data.push(1); // format = 1 (protobuf - unsupported)
        data.extend_from_slice(&0u32.to_le_bytes()); // payload_size = 0
        let err = parse_frame(&data).unwrap_err();
        assert!(
            err.to_string()
                .contains("Unsupported payload format byte: 1"),
            "got: {err}"
        );
    }
}
