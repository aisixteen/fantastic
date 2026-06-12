//! codec — the binary-safe frame codec shared by every transport (web_ws WS frames,
//! ws_bridge, cloud_bridge). Mirrors `python/bundled_agents/io/io_bridge/_codec.py`.
//!
//! A frame is a JSON envelope. A frame whose payload carries raw bytes (a `read_stream`
//! chunk, audio) is a **binary frame**:
//!
//! ```text
//! [ 4-byte BE u32 H | H-byte JSON header | M-byte raw body ]
//! ```
//!
//! The header is the envelope with the bytes value replaced by `null` and a
//! `_binary_path: "<dotted.path>"` naming where it lived; the receiver reconstructs by
//! reading the trailing M bytes as the body. A frame with NO bytes is plain UTF-8 JSON.
//! This is how raw bytes ride the wire WITHOUT base64.
//!
//! Rust note vs python: `serde_json::Value` cannot hold a raw `&[u8]`, so here the JSON
//! header and the raw body are carried SEPARATELY (python finds/restores the bytes value
//! inside the dict; rust passes the body alongside). The **wire layout is identical**, so
//! a rust leg and a python web_ws interoperate byte-for-byte.
//!
//! The text/binary split is carried by the TRANSPORT, not guessed here (WS frame type;
//! a 1-byte tag on cloud_bridge's TLS record).

use serde_json::Value;

/// Encode a binary frame `[4B len | header | body]`. `header` should already carry the
/// `_binary_path` marker (and `null` at that path) per the convention above so the
/// receiver can place the body.
pub fn encode_binary_frame(header: &Value, body: &[u8]) -> Vec<u8> {
    let head = serde_json::to_vec(header).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::with_capacity(4 + head.len() + body.len());
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(body);
    out
}

/// Decode a binary frame → `(header, body)`. The header carries `_binary_path`; the body
/// is the raw trailing bytes. Errors on a short / malformed frame.
pub fn decode_binary_frame(data: &[u8]) -> Result<(Value, Vec<u8>), String> {
    if data.len() < 4 {
        return Err("binary frame: shorter than the 4-byte length prefix".into());
    }
    let h = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + h {
        return Err("binary frame: header length exceeds the frame".into());
    }
    let header: Value = serde_json::from_slice(&data[4..4 + h]).map_err(|e| e.to_string())?;
    let body = data[4 + h..].to_vec();
    Ok((header, body))
}

/// Set `value` at a dotted `path` in a JSON object/array (mirrors py `set_path`). Used to
/// place the body back at `_binary_path` after decoding, or to null it before encoding.
pub fn set_path(obj: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = obj;
    for p in &parts[..parts.len().saturating_sub(1)] {
        cur = match cur {
            Value::Array(a) => match p.parse::<usize>() {
                Ok(i) if i < a.len() => &mut a[i],
                _ => return,
            },
            Value::Object(m) => match m.get_mut(*p) {
                Some(v) => v,
                None => return,
            },
            _ => return,
        };
    }
    if let Some(last) = parts.last() {
        match cur {
            Value::Array(a) => {
                if let Ok(i) = last.parse::<usize>() {
                    if i < a.len() {
                        a[i] = value;
                    }
                }
            }
            Value::Object(m) => {
                m.insert((*last).to_string(), value);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn binary_frame_round_trips_raw_bytes() {
        let header =
            json!({"type":"reply","id":"1","data":{"bytes":null},"_binary_path":"data.bytes"});
        let body: Vec<u8> = (0u8..=255).collect();
        let wire = encode_binary_frame(&header, &body);
        // 4-byte BE length prefix
        let h = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        assert_eq!(&wire[4 + h..], &body[..]);
        let (got_header, got_body) = decode_binary_frame(&wire).unwrap();
        assert_eq!(got_header["_binary_path"], "data.bytes");
        assert_eq!(got_body, body);
    }

    #[test]
    fn set_path_places_body_at_binary_path() {
        let mut env = json!({"type":"reply","data":{"bytes":null}});
        set_path(&mut env, "data.bytes", json!("RESTORED"));
        assert_eq!(env["data"]["bytes"], "RESTORED");
    }

    #[test]
    fn decode_rejects_short_frame() {
        assert!(decode_binary_frame(&[0, 1]).is_err());
    }
}
