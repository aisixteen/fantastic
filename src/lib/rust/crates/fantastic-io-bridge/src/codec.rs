//! codec — the binary-safe frame codec shared by every transport (web_ws WS frames,
//! ws_bridge, relay_connector). Mirrors `python/bundled_agents/io/io_bridge/_codec.py`.
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
//! The text/binary split is carried by the TRANSPORT, not guessed here — every
//! transport today is WS-based (web_ws, ws_bridge, relay_connector over the relay)
//! and uses the WS frame type; the relay forwards the frame kind end-to-end.

use serde_json::Value;

/// Encode ONE envelope for a TEXT frame — python `encode_frame`'s text half,
/// `json.dumps(envelope, default=str)`: `, ` / `: ` separators and
/// `ensure_ascii`, so `·` leaves as `·` exactly as python sends it. Every
/// server→client text frame on every transport goes through here.
pub fn encode_frame(envelope: &Value) -> String {
    fantastic_bundle::py_json_dumps(envelope, true, None)
}

/// Encode a binary frame `[4B len | header | body]`. `header` should already carry the
/// `_binary_path` marker (and `null` at that path) per the convention above so the
/// receiver can place the body.
pub fn encode_binary_frame(header: &Value, body: &[u8]) -> Vec<u8> {
    let head = encode_frame(header).into_bytes();
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

/// Resolve one python list index: `int(p)` plus python's negative indexing.
/// `Err` for a non-int spelling or an out-of-range index — python raises
/// `ValueError` / `IndexError` for both, and `decode_frame` turns either into
/// the one malformed-frame error.
fn list_index(part: &str, len: usize) -> Result<usize, String> {
    let n: i64 = part
        .parse()
        .map_err(|_| format!("list index {part:?} is not an int"))?;
    let i = if n < 0 { n + len as i64 } else { n };
    if i < 0 || i as usize >= len {
        return Err(format!("list index {n} out of range"));
    }
    Ok(i as usize)
}

/// Set `value` at a dotted `path` in a JSON object/array (mirrors py `set_path`). Used to
/// place the body back at `_binary_path` after decoding, or to null it before encoding.
///
/// FALLIBLE, like python's, which raises `KeyError`/`IndexError`/`TypeError`
/// through `decode_frame` into one `ValueError` the read loop answers as a
/// malformed frame. Rust used to return `()` and SILENTLY do nothing on an
/// unplaceable path, so a frame naming `payload.nope.deep` was handled as if it
/// were well-formed and the peer got a reply instead of the frame error.
pub fn set_path(obj: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = obj;
    for p in &parts[..parts.len() - 1] {
        cur = match cur {
            Value::Array(a) => {
                let i = list_index(p, a.len())?;
                &mut a[i]
            }
            Value::Object(m) => m
                .get_mut(*p)
                .ok_or_else(|| format!("no key {p:?} on the way to {path:?}"))?,
            _ => return Err(format!("{p:?} is not a container in {path:?}")),
        };
    }
    let last = parts[parts.len() - 1];
    match cur {
        Value::Array(a) => {
            let i = list_index(last, a.len())?;
            a[i] = value;
            Ok(())
        }
        Value::Object(m) => {
            m.insert(last.to_string(), value);
            Ok(())
        }
        _ => Err(format!(
            "{last:?} does not name a settable slot in {path:?}"
        )),
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
        set_path(&mut env, "data.bytes", json!("RESTORED")).unwrap();
        assert_eq!(env["data"]["bytes"], "RESTORED");
    }

    /// python raises `KeyError`/`IndexError`/`TypeError` out of `set_path`, and
    /// `decode_frame` turns each into the one malformed-frame `ValueError`. A
    /// silent no-op here made a frame with an unplaceable `_binary_path` look
    /// well-formed (A11.7 case f).
    #[test]
    fn set_path_unresolvable_intermediate_errors() {
        // missing intermediate key
        let mut env = json!({"payload": {}});
        assert!(set_path(&mut env, "payload.nope.deep", json!(1)).is_err());
        // the holder is not a container
        let mut env = json!({"payload": "text"});
        assert!(set_path(&mut env, "payload.bytes", json!(1)).is_err());
        // a non-int list index, and one past the end
        let mut env = json!({"items": [1, 2]});
        assert!(set_path(&mut env, "items.x", json!(1)).is_err());
        assert!(set_path(&mut env, "items.5", json!(1)).is_err());
        // python's negative indexing resolves
        set_path(&mut env, "items.-1", json!(9)).unwrap();
        assert_eq!(env["items"][1], 9);
    }

    #[test]
    fn decode_rejects_short_frame() {
        assert!(decode_binary_frame(&[0, 1]).is_err());
    }
}
