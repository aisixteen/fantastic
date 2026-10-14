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
//! `_binary_path` naming where it lived; the receiver reconstructs by reading the
//! trailing M bytes as the body. A frame with NO bytes is plain UTF-8 JSON. This is
//! how raw bytes ride the wire WITHOUT base64.
//!
//! `_binary_path` is a JSON ARRAY of steps from the envelope root: a STRING step is
//! an object key, taken literally (a key may hold `.`); an INTEGER step is a list
//! index. `["payload", "bytes"]`, `["payload", "a.b"]` (the top-level payload key
//! `"a.b"`), `["data", "items", 0, "data"]`. A dotted string could not tell the key
//! `"a.b"` from the nested `a` → `b`, so it is refused ([`fantastic_bundle::path`]).
//!
//! Rust note vs python: `serde_json::Value` cannot hold a raw `&[u8]`, so here the JSON
//! header and the raw body are carried SEPARATELY (python finds/restores the bytes value
//! inside the dict; rust passes the body alongside, with the path it sits at — the
//! kernel's `Blob`). The **wire layout is identical**, so a rust leg and a python web_ws
//! interoperate byte-for-byte. Python's `set_path` is [`fantastic_bundle::path::set_path`].
//!
//! The text/binary split is carried by the TRANSPORT, not guessed here — every
//! transport today is WS-based (web_ws, ws_bridge, relay_connector over the relay)
//! and uses the WS frame type; the relay forwards the frame kind end-to-end.

use fantastic_bundle::path::{set_path, steps_value, Step};
use serde_json::Value;

/// THE ONE FRAME CAP every wire leg holds: 16 MiB — a WS frame (`ws_bridge`'s
/// `max_size`, the relay's `MAX_FRAME`, the web_ws socket), an exec_bridge
/// control-fd frame, and a web_rest request body (the http face of the same
/// envelope). Larger data moves in chunks (`write_stream`). Python
/// `io_bridge.MAX_FRAME`.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Encode ONE envelope for a TEXT frame — python `encode_frame`'s text half,
/// `json.dumps(envelope, default=str)`: `, ` / `: ` separators and
/// `ensure_ascii`, so `·` leaves as `·` exactly as python sends it. Every
/// server→client text frame on every transport goes through here.
pub fn encode_frame(envelope: &Value) -> String {
    fantastic_bundle::py_json_dumps(envelope, true, None)
}

/// Encode a binary frame `[4B len | header | body]` — python `encode_frame`'s binary
/// half. `header` is the envelope holding `null` where the body belongs; `path` is
/// that slot's steps from the envelope root, written as `_binary_path` (a JSON array,
/// last key of the header, as python's `head_obj["_binary_path"] = path` puts it).
pub fn encode_binary_frame(header: &Value, path: &[Step], body: &[u8]) -> Vec<u8> {
    let mut header = header.clone();
    if let Value::Object(m) = &mut header {
        m.insert("_binary_path".to_string(), steps_value(path));
    }
    let head = encode_frame(&header).into_bytes();
    let mut out = Vec::with_capacity(4 + head.len() + body.len());
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(body);
    out
}

/// JSON off the wire, STRICTLY — python `io_bridge.loads`: only values EVERY
/// runtime can hold. Refused, each as the caller's
/// one "not valid JSON" answer:
///   * `NaN`, `Infinity`, `-Infinity` — not JSON (serde never parses them);
///   * an integer outside int64, or a number that overflows a double — this
///     crate's `arbitrary_precision` would otherwise carry `12345678901234567890`
///     and `1e400` through as text, a value python refuses;
///   * a lone surrogate in a string — not text (serde refuses the escape).
///
/// # Errors
/// The parse error, or the reason the value is not one every runtime holds.
pub fn loads(text: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    strict_numbers(&v)?;
    Ok(v)
}

/// [`loads`] for bytes — a UTF-8 decode first, then the strict parse.
///
/// # Errors
/// A non-UTF-8 body, or anything [`loads`] refuses.
pub fn loads_slice(data: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(data).map_err(|e| e.to_string())?;
    loads(text)
}

/// python's `parse_int=_int64, parse_float=_finite`, over a parsed tree.
fn strict_numbers(v: &Value) -> Result<(), String> {
    match v {
        Value::Number(n) => {
            let digits = n.as_str();
            if digits.contains(['.', 'e', 'E']) {
                if !digits.parse::<f64>().is_ok_and(f64::is_finite) {
                    return Err(format!("number {digits} overflows a double"));
                }
            } else if n.as_i64().is_none() {
                return Err(format!("integer {digits} is outside int64"));
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(strict_numbers),
        Value::Object(m) => m.values().try_for_each(strict_numbers),
        _ => Ok(()),
    }
}

/// One decoded binary frame — python `decode_frame`'s result, split the way a
/// `Value` needs it: the envelope with `_binary_path` popped and `null` placed in
/// the body's slot, the slot's steps, and the raw body beside them.
#[derive(Debug, Clone, PartialEq)]
pub struct BinaryFrame {
    /// The envelope; `null` sits where python's dict holds the bytes.
    pub head: Value,
    /// Where the body belongs, from the envelope root. `None` when the header
    /// named no slot (absent or `null`): python places no body then.
    pub path: Option<Vec<Step>>,
    /// The raw trailing bytes.
    pub body: Vec<u8>,
}

/// Decode a binary frame `[4B len | header | body]` — python `decode_frame`. The
/// header is parsed STRICTLY ([`loads`]); when it is an object its `_binary_path`
/// is popped and the slot checked by placing `null` there
/// ([`fantastic_bundle::path::set_path`] — every step must hold, exactly as
/// python's placement of the bytes must).
///
/// # Errors
/// A short / malformed frame, a header [`loads`] refuses, or
/// `bad _binary_path <repr>: <why>` — python's text — for a path that names no
/// slot (the old dotted string included).
pub fn decode_binary_frame(data: &[u8]) -> Result<BinaryFrame, String> {
    if data.len() < 4 {
        return Err("binary frame: shorter than the 4-byte length prefix".into());
    }
    let h = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + h {
        return Err("binary frame: header length exceeds the frame".into());
    }
    let mut head: Value = loads_slice(&data[4..4 + h])?;
    let body = data[4 + h..].to_vec();
    // `head.pop("_binary_path", None)`: JSON null is python's "absent".
    let raw = match &mut head {
        Value::Object(m) => m.shift_remove("_binary_path").filter(|p| !p.is_null()),
        _ => None,
    };
    let path = match raw {
        None => None,
        Some(p) => Some(set_path(&mut head, &p, Value::Null).map_err(|why| {
            format!(
                "bad _binary_path {}: {why}",
                fantastic_bundle::py_repr_value(&p)
            )
        })?),
    };
    Ok(BinaryFrame { head, path, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Python's `io_bridge.loads` refuses what not every runtime can hold.
    #[test]
    fn loads_refuses_what_python_refuses() {
        assert_eq!(
            loads(r#"{"a": 9223372036854775807}"#).unwrap()["a"],
            json!(i64::MAX)
        );
        assert_eq!(loads("[-9223372036854775808]").unwrap()[0], json!(i64::MIN));
        for bad in [
            r#"{"a": 9223372036854775808}"#,
            "[12345678901234567890]",
            "[-9223372036854775809]",
            r#"{"x": 1e400}"#,
            "[-1E400]",
            "[NaN]",
            "[Infinity]",
            "[-Infinity]",
            r#"["\udcff"]"#,
            r#"{"\ud800": 1}"#,
        ] {
            assert!(loads(bad).is_err(), "accepted {bad}");
        }
        // A tiny float underflows to 0.0 — finite, accepted (python agrees).
        assert!(loads("[1e-400, 2.5, 1E3]").is_ok());
        // A strict header refuses the whole frame.
        let head = br#"{"type":"call","n":99999999999999999999}"#;
        let mut wire = (head.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(head);
        assert!(decode_binary_frame(&wire).is_err());
    }

    fn steps(v: Value) -> Vec<Step> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| match s {
                Value::String(k) => Step::Key(k.clone()),
                n => Step::Index(n.as_u64().unwrap() as usize),
            })
            .collect()
    }

    /// The header a frame carries on the wire.
    fn head_of(wire: &[u8]) -> Value {
        let h = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        serde_json::from_slice(&wire[4..4 + h]).unwrap()
    }

    #[test]
    fn binary_frame_round_trips_raw_bytes() {
        let header = json!({"type":"reply","id":"1","data":{"bytes":null}});
        let body: Vec<u8> = (0u8..=255).collect();
        let wire = encode_binary_frame(&header, &steps(json!(["data", "bytes"])), &body);
        // 4-byte BE length prefix
        let h = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        assert_eq!(&wire[4 + h..], &body[..]);
        assert_eq!(head_of(&wire)["_binary_path"], json!(["data", "bytes"]));
        let got = decode_binary_frame(&wire).unwrap();
        assert_eq!(got.head, header, "the path is popped, the slot holds null");
        assert_eq!(got.path, Some(steps(json!(["data", "bytes"]))));
        assert_eq!(got.body, body);
    }

    /// python `test_a_literal_dotted_key_round_trips_beside_its_nested_twin`.
    #[test]
    fn a_literal_dotted_key_round_trips_beside_its_nested_twin() {
        let env = json!({"type": "call", "id": "1", "target": "echo",
                         "payload": {"type": "take", "a": {"b": "kept"}, "a.b": null}});
        let wire = encode_binary_frame(&env, &steps(json!(["payload", "a.b"])), b"\x00raw\xff");
        let head = head_of(&wire);
        assert_eq!(head["_binary_path"], json!(["payload", "a.b"]));
        assert_eq!(
            head["payload"],
            json!({"type": "take", "a": {"b": "kept"}, "a.b": null}),
            "the null sits at the literal key; the nested twin is untouched"
        );
        let got = decode_binary_frame(&wire).unwrap();
        assert_eq!(got.head, env);
        assert_eq!(
            got.path,
            Some(vec![Step::from("payload"), Step::from("a.b")])
        );
    }

    /// python `test_nested_list_and_odd_key_paths_round_trip`.
    #[test]
    fn nested_list_and_odd_key_paths_round_trip() {
        for (env, path) in [
            (json!({"data": {"bytes": null}}), json!(["data", "bytes"])),
            (
                json!({"data": {"items": [{"n": 1}, {"data": null}]}}),
                json!(["data", "items", 1, "data"]),
            ),
            (json!({"payload": [null, "tail"]}), json!(["payload", 0])),
            (json!({"payload": {"0": null}}), json!(["payload", "0"])),
            (json!({"payload": {"": null}}), json!(["payload", ""])),
            (
                json!({"payload": {"a..b.": {"c.": null}}}),
                json!(["payload", "a..b.", "c."]),
            ),
        ] {
            let wire = encode_binary_frame(&env, &steps(path.clone()), b"\x01");
            assert_eq!(head_of(&wire)["_binary_path"], path);
            let got = decode_binary_frame(&wire).unwrap();
            assert_eq!(got.head, env, "{path}");
            assert_eq!(got.path, Some(steps(path)));
            assert_eq!(got.body, b"\x01");
        }
    }

    fn bin(head: &Value) -> Vec<u8> {
        let h = serde_json::to_vec(head).unwrap();
        let mut wire = (h.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&h);
        wire.extend_from_slice(b"zz");
        wire
    }

    /// python `test_a_path_with_no_slot_or_no_path_places_nothing`.
    #[test]
    fn a_frame_with_no_path_places_nothing() {
        for head in [
            json!({"type": "x"}),
            json!({"type": "x", "_binary_path": null}),
        ] {
            let got = decode_binary_frame(&bin(&head)).unwrap();
            assert_eq!(got.head, json!({"type": "x"}));
            assert_eq!(got.path, None);
        }
    }

    /// python `test_a_malformed_path_off_the_wire_is_one_value_error`: every bad
    /// path is ONE error, `bad _binary_path <repr>: <why>`.
    #[test]
    fn a_malformed_path_off_the_wire_is_one_error() {
        let cases: [(Value, &str, &str); 13] = [
            (
                json!("payload.bytes"),
                "'payload.bytes'",
                "not a list of steps",
            ),
            (json!([]), "[]", "an empty path names no slot"),
            (
                json!({"payload": "bytes"}),
                "{'payload': 'bytes'}",
                "not a list of steps",
            ),
            (json!([0]), "[0]", "step 0 at an object is not a string key"),
            (
                json!(["items", "0"]),
                "['items', '0']",
                "step '0' at a list is not a non-negative integer index",
            ),
            (
                json!(["items", -1]),
                "['items', -1]",
                "step -1 at a list is not a non-negative integer index",
            ),
            (
                json!(["items", true]),
                "['items', True]",
                "step True at a list is not a non-negative integer index",
            ),
            (
                serde_json::from_str(r#"["items", 0.0]"#).unwrap(),
                "['items', 0.0]",
                "step 0.0 at a list is not a non-negative integer index",
            ),
            (
                json!(["items", 2]),
                "['items', 2]",
                "index 2 is past the end of a list of 2",
            ),
            (json!(["nope", "x"]), "['nope', 'x']", "no key 'nope'"),
            (
                json!(["type", "x"]),
                "['type', 'x']",
                "step 'x' goes into a string, not a container",
            ),
            (
                json!(["items", 0, "x"]),
                "['items', 0, 'x']",
                "step 'x' goes into a null, not a container",
            ),
            (
                json!(["n", "x"]),
                "['n', 'x']",
                "step 'x' goes into a number, not a container",
            ),
        ];
        for (path, repr, why) in cases {
            let head =
                json!({"type": "reply", "items": [null, null], "n": 1, "_binary_path": path});
            assert_eq!(
                decode_binary_frame(&bin(&head)).unwrap_err(),
                format!("bad _binary_path {repr}: {why}")
            );
        }
    }

    #[test]
    fn decode_rejects_short_frame() {
        assert!(decode_binary_frame(&[0, 1]).is_err());
    }
}
