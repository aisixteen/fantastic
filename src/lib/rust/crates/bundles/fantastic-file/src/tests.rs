//! Unit tests for this bundle crate.

use super::*;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("file_bridge — the gated filesystem edge"));
    assert!(README.contains("read_stream, write_stream, pump"));
}

#[test]
fn resolve_safe_refuses_escape() {
    let tmp = tempfile::TempDir::new().unwrap();
    let err = resolve_safe(tmp.path(), "../escape").unwrap_err();
    assert!(err.contains("escapes root"));
}

#[test]
fn resolve_safe_keeps_subpath() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = resolve_safe(tmp.path(), "sub/inner.txt").unwrap();
    assert!(p.starts_with(tmp.path()));
    assert!(p.ends_with("sub/inner.txt"));
}

#[test]
fn resolve_safe_refuses_absolute_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let err = resolve_safe(tmp.path(), "/etc/passwd").unwrap_err();
    assert!(err.contains("escapes root"));
}

#[test]
fn resolve_safe_dot_dot_after_descent_is_ok() {
    // a/b/.. resolves to root/a — still inside.
    let tmp = tempfile::TempDir::new().unwrap();
    let p = resolve_safe(tmp.path(), "a/b/../leaf.txt").unwrap();
    assert!(p.starts_with(tmp.path()));
}

#[test]
fn write_stream_then_read_stream_round_trips_raw_bytes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    // Non-UTF-8 bytes — proves the channel is raw, not text/base64.
    let payload: Vec<u8> = vec![0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80];
    // SINK: first chunk truncates.
    let w = write_stream_reply(
        root,
        &serde_json::json!({"path": "blob.bin", "truncate": true}),
        &payload,
    );
    assert_eq!(w["written"], payload.len());
    assert_eq!(w["size"], payload.len());
    assert!(w.get("error").is_none(), "{w}");
    // SOURCE: read it all back.
    let (meta, body) = read_stream_reply(root, &serde_json::json!({"path": "blob.bin"}));
    assert!(meta.get("error").is_none(), "{meta}");
    assert_eq!(body, payload, "bytes must round-trip verbatim");
    assert_eq!(meta["eof"], true);
    assert_eq!(meta["size"], payload.len());
    assert_eq!(meta["next_offset"], payload.len());
}

#[test]
fn read_stream_chunks_with_offset_and_eof() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let payload: Vec<u8> = (0u8..=255).collect(); // 256 bytes
    write_stream_reply(
        root,
        &serde_json::json!({"path": "big.bin", "truncate": true}),
        &payload,
    );
    // First 100 bytes.
    let (m1, b1) = read_stream_reply(
        root,
        &serde_json::json!({"path": "big.bin", "offset": 0, "length": 100}),
    );
    assert_eq!(b1.len(), 100);
    assert_eq!(m1["eof"], false);
    assert_eq!(m1["next_offset"], 100);
    // Resume from next_offset to the end.
    let (m2, b2) = read_stream_reply(
        root,
        &serde_json::json!({"path": "big.bin", "offset": 100, "length": 1000}),
    );
    assert_eq!(b2.len(), 156);
    assert_eq!(m2["eof"], true);
    let mut joined = b1;
    joined.extend(b2);
    assert_eq!(joined, payload);
}

#[test]
fn write_stream_appends_when_offset_omitted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    write_stream_reply(
        root,
        &serde_json::json!({"path": "a.bin", "truncate": true}),
        b"AAAA",
    );
    let w = write_stream_reply(root, &serde_json::json!({"path": "a.bin"}), b"BBBB");
    assert_eq!(w["offset"], 4); // appended at end
    assert_eq!(w["size"], 8);
    let (_m, body) = read_stream_reply(root, &serde_json::json!({"path": "a.bin"}));
    assert_eq!(body, b"AAAABBBB");
}

#[test]
fn clamp_root_refuses_outside_base_and_allows_relative() {
    let base = tempfile::TempDir::new().unwrap();
    let b = &base.path().canonicalize().unwrap();
    // Relative roots resolve under the base and pass.
    assert!(clamp_root(Path::new(".fantastic"), b).is_ok());
    assert!(clamp_root(Path::new("sub/dir"), b).is_ok());
    // An absolute root inside the base passes.
    assert!(clamp_root(&b.join("served"), b).is_ok());
    // An absolute root outside the base refuses (the running-dir law).
    let err = clamp_root(Path::new("/"), b).unwrap_err();
    assert!(err.contains("escapes the running dir"), "{err}");
    // A `..` climb above the base refuses.
    assert!(clamp_root(Path::new("../../../etc"), b).is_err());
}

#[test]
fn gate_seals_by_default_and_opens_with_allow_all() {
    let id = AgentId::from("fb");
    // No ingress_rule ⇒ SEALED: read/write denied, reflect admitted.
    let bare = serde_json::Map::new();
    assert!(gate(&bare, &id, "read_stream").is_some());
    assert!(gate(&bare, &id, "write_stream").is_some());
    assert!(gate(&bare, &id, "read").is_some());
    assert!(gate(&bare, &id, "reflect").is_none());
    let denied = gate(&bare, &id, "read").unwrap();
    assert_eq!(denied["reason"], "unauthorized");
    // ingress_rule=allow_all_ingress ⇒ OPEN.
    let mut open = serde_json::Map::new();
    open.insert(
        "ingress_rule".into(),
        serde_json::json!("allow_all_ingress"),
    );
    assert!(gate(&open, &id, "read_stream").is_none());
    assert!(gate(&open, &id, "write_stream").is_none());
}

#[test]
fn windowed_read_pages_and_says_how_to_continue() {
    // Py parity (`_window_text`, measured 2026-08-07): an unbounded read once put
    // 86 KB into an LLM conversation and the next turn died mid-prefill. A read
    // under the cap is untouched (no windowing keys); an over-cap read carries
    // total/offset/truncated/next_offset and a hint naming the continuation.
    let meta = serde_json::Map::new();
    let small = window_text(&meta, &serde_json::json!({}), "abc");
    assert_eq!(small.0, "abc");
    assert!(
        small.1.is_empty(),
        "small read must carry NO windowing keys"
    );

    let big = "x".repeat(DEFAULT_MAX_READ_CHARS + 100);
    let (chunk, extras) = window_text(&meta, &serde_json::json!({}), &big);
    assert_eq!(chunk.len(), DEFAULT_MAX_READ_CHARS);
    assert_eq!(extras["truncated"], serde_json::json!(true));
    assert_eq!(
        extras["next_offset"],
        serde_json::json!(DEFAULT_MAX_READ_CHARS)
    );
    assert!(extras["hint"].as_str().unwrap().contains("offset:16384"));

    // Explicit paging: the offset walk reaches the tail and the tail is final.
    let (tail, tail_extras) = window_text(
        &meta,
        &serde_json::json!({"offset": DEFAULT_MAX_READ_CHARS}),
        &big,
    );
    assert_eq!(tail.len(), 100);
    assert_eq!(tail_extras["truncated"], serde_json::json!(false));
    assert!(tail_extras.get("next_offset").is_none());

    // The per-record override widens the cap.
    let mut wide = serde_json::Map::new();
    wide.insert("max_read_chars".into(), serde_json::json!(100_000));
    let (all, none) = window_text(&wide, &serde_json::json!({}), &big);
    assert_eq!(all.len(), big.len());
    assert!(none.is_empty());
}
