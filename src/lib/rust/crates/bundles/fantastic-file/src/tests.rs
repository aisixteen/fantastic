//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;

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
    // No ingress_rule ⇒ SEALED: stream/list/mutating verbs denied, reflect admitted.
    let bare = serde_json::Map::new();
    assert!(gate(&bare, &id, "read_stream").is_some());
    assert!(gate(&bare, &id, "write_stream").is_some());
    assert!(gate(&bare, &id, "list").is_some());
    assert!(gate(&bare, &id, "reflect").is_none());
    let denied = gate(&bare, &id, "list").unwrap();
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

// ─── patch_text_file ────────────────────────────────────────────
// Mirrors py `bundled_agents/io/file_bridge/tests/test_patch.py`. The happy
// paths + fail-loud + bad-input cases drive `patch_text_file_reply` directly
// (the gate/readonly checks live in `handle_binary`, tested through the kernel
// at the bottom). Files are seeded on disk; the patch rides as raw utf-8 bytes.

/// One SEARCH/REPLACE block (py test's `_block`).
fn patch_block(old: &str, new: &str) -> String {
    format!("<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE")
}

/// Seed a text file under `root` and return its absolute path.
fn seed(root: &Path, rel: &str, text: &str) -> std::path::PathBuf {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, text).unwrap();
    p
}

fn patch(root: &Path, rel: &str, patch_text: &str) -> Value {
    patch_text_file_reply(
        root,
        &serde_json::json!({"path": rel}),
        patch_text.as_bytes(),
    )
}

#[test]
fn patch_single_hunk_applies_and_writes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\nb = 2\nc = 3\n");
    let r = patch(root, "m.py", &patch_block("b = 2", "b = 20"));
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(r["hunks"], 1);
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\nb = 20\nc = 3\n"
    );
}

#[test]
fn patch_multiple_hunks_apply_in_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "x = 1\ny = 2\nz = 3\n");
    let p = format!(
        "{}\n{}",
        patch_block("x = 1", "x = 11"),
        patch_block("z = 3", "z = 33")
    );
    let r = patch(root, "m.py", &p);
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(r["hunks"], 2);
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "x = 11\ny = 2\nz = 33\n"
    );
}

#[test]
fn patch_hunk_matches_content_a_prior_hunk_created() {
    // Hunks apply against the EVOLVING content: hunk 2's SEARCH exists only
    // after hunk 1 has run.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.txt", "one\n");
    let p = format!(
        "{}\n{}",
        patch_block("one", "two"),
        patch_block("two", "three")
    );
    let r = patch(root, "m.txt", &p);
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(r["hunks"], 2);
    assert_eq!(
        std::fs::read_to_string(root.join("m.txt")).unwrap(),
        "three\n"
    );
}

#[test]
fn patch_hunk_with_quotes_backslashes_is_verbatim() {
    // The whole point of bytes-on-the-wire: no escaping. A quote-heavy program
    // applies exactly as written.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let original = "print(\"hi\")\npath = \"c:\\\\tmp\"\n";
    seed(root, "p.py", original);
    let expected = "print(\"bye\", end=\"\")\npath = \"c:\\\\tmp\"\n";
    let r = patch(
        root,
        "p.py",
        &patch_block("print(\"hi\")", "print(\"bye\", end=\"\")"),
    );
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(
        std::fs::read_to_string(root.join("p.py")).unwrap(),
        expected
    );
}

#[test]
fn patch_unicode_content_and_hunk() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "u.txt", "café — over\nnext\n");
    let r = patch(root, "u.txt", &patch_block("café — over", "tea — done"));
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(
        std::fs::read_to_string(root.join("u.txt")).unwrap(),
        "tea — done\nnext\n"
    );
}

#[test]
fn patch_multiline_hunk() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(
        root,
        "m.py",
        "def f():\n    return 1\n\ndef g():\n    return 2\n",
    );
    let r = patch(
        root,
        "m.py",
        &patch_block("def f():\n    return 1", "def f():\n    return 99"),
    );
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "def f():\n    return 99\n\ndef g():\n    return 2\n"
    );
}

#[test]
fn patch_no_match_fails_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let r = patch(root, "m.py", &patch_block("nope = 0", "x = 0"));
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "no_match");
    assert_eq!(r["failed"][0]["index"], 0);
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\n"
    );
}

#[test]
fn patch_ambiguous_match_fails_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "v = 1\nv = 1\n");
    let r = patch(root, "m.py", &patch_block("v = 1", "v = 2"));
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "ambiguous");
    assert!(r["failed"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("2 places"));
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "v = 1\nv = 1\n"
    );
}

#[test]
fn patch_second_hunk_failure_reverts_the_whole_patch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\nb = 2\n");
    let p = format!(
        "{}\n{}",
        patch_block("a = 1", "a = 10"),
        patch_block("missing", "x")
    );
    let r = patch(root, "m.py", &p);
    assert!(r.get("patched").is_none());
    assert_eq!(r["failed"][0]["index"], 1);
    // first hunk WAS valid but nothing is written — all-or-nothing.
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\nb = 2\n"
    );
}

#[test]
fn patch_malformed_unterminated_block_is_bad_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let bad_patch = "<<<<<<< SEARCH\na = 1\n=======\na = 2\n"; // no >>>>>>> REPLACE
    let r = patch(root, "m.py", bad_patch);
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\n"
    );
}

#[test]
fn patch_empty_patch_no_blocks_is_bad_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let r = patch(root, "m.py", "just some prose, no markers");
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "bad_arg");
}

#[test]
fn patch_missing_bytes_is_missing_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let r = patch_text_file_reply(root, &serde_json::json!({"path": "m.py"}), b"");
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "missing_arg");
    assert!(r["error"].as_str().unwrap().contains("bytes"));
}

#[test]
fn patch_missing_path_is_missing_arg() {
    // py `test_missing_path`: no `path` in the header → missing-arg refusal.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let r = patch_text_file_reply(root, &serde_json::json!({}), b"whatever");
    assert_eq!(r["reason"], "missing_arg");
    assert!(r["error"].as_str().unwrap().contains("path"));
}

#[test]
fn patch_bad_utf8_patch_is_bad_arg() {
    // py `test_bad_utf8_patch`: patch bytes that are not utf-8 → bad_arg, file untouched.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let r = patch_text_file_reply(
        root,
        &serde_json::json!({"path": "m.py"}),
        &[0xFF, 0xFE, 0xFF],
    );
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\n"
    );
}

#[test]
fn patch_malformed_missing_divider_is_bad_arg() {
    // py `test_malformed_missing_divider`: a SEARCH block that reaches `>>>>>>> REPLACE`
    // without ever hitting the `=======` divider → bad_arg, file untouched.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let bad_patch = "<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE\n"; // no =======
    let r = patch(root, "m.py", bad_patch);
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\n"
    );
}

#[test]
fn patch_file_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let r = patch(root, "nope.py", &patch_block("a", "b"));
    assert!(r.get("patched").is_none());
    assert_eq!(r["reason"], "not_found");
}

#[test]
fn patch_sealed_gate_denies() {
    // No ingress_rule ⇒ sealed by default; patch_text_file is gated like any verb.
    let id = AgentId::from("fb");
    let bare = serde_json::Map::new();
    let denied = gate(&bare, &id, "patch_text_file").unwrap();
    assert_eq!(denied["reason"], "unauthorized");
    // Opening the leg admits it.
    let mut open = serde_json::Map::new();
    open.insert(
        "ingress_rule".into(),
        serde_json::json!("allow_all_ingress"),
    );
    assert!(gate(&open, &id, "patch_text_file").is_none());
}

// ── through the kernel: readonly + sealed on the BINARY channel ──

/// A kernel with a `file_bridge` agent (id "fb") whose meta = `meta`, rooted at a
/// tempdir INSIDE the running dir the clamp derives from the root agent.
fn mk_bridge(root_dir: &Path, mut meta: Map<String, Value>) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root_dir.join(".fantastic"),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    meta.insert(
        "root".to_string(),
        serde_json::json!(root_dir.to_string_lossy()),
    );
    let fb = Agent::new(
        AgentId::from("fb"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("core")),
        meta,
        root_dir.to_path_buf(),
        false,
    );
    let _ = kernel.register(Arc::clone(&fb));
    (kernel, AgentId::from("fb"))
}

#[tokio::test]
async fn patch_readonly_leg_refuses_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let mut meta = Map::new();
    meta.insert(
        "ingress_rule".to_string(),
        serde_json::json!("allow_all_ingress"),
    );
    meta.insert("readonly".to_string(), serde_json::json!(true));
    let (kernel, id) = mk_bridge(root, meta);
    let header = serde_json::json!({"type": "patch_text_file", "path": "m.py"});
    let (reply, body) = FileBundle
        .handle_binary(
            &id,
            header,
            patch_block("a = 1", "a = 2").into_bytes(),
            &kernel,
        )
        .await
        .unwrap();
    assert!(body.is_empty());
    assert_eq!(reply.unwrap()["reason"], "unauthorized");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\n"
    );
}

#[tokio::test]
async fn patch_sealed_bridge_denies_and_writes_nothing() {
    // No ingress_rule ⇒ sealed; the binary channel gates exactly like text.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\n");
    let (kernel, id) = mk_bridge(root, Map::new());
    let header = serde_json::json!({"type": "patch_text_file", "path": "m.py"});
    let (reply, _body) = FileBundle
        .handle_binary(
            &id,
            header,
            patch_block("a = 1", "a = 2").into_bytes(),
            &kernel,
        )
        .await
        .unwrap();
    assert_eq!(reply.unwrap()["reason"], "unauthorized");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\n"
    );
}

#[tokio::test]
async fn patch_through_binary_channel_applies() {
    // The happy path end-to-end over `handle_binary` (the send_with_binary path).
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    seed(root, "m.py", "a = 1\nb = 2\n");
    let mut meta = Map::new();
    meta.insert(
        "ingress_rule".to_string(),
        serde_json::json!("allow_all_ingress"),
    );
    let (kernel, id) = mk_bridge(root, meta);
    let header = serde_json::json!({"type": "patch_text_file", "path": "m.py"});
    let (reply, body) = FileBundle
        .handle_binary(
            &id,
            header,
            patch_block("b = 2", "b = 22").into_bytes(),
            &kernel,
        )
        .await
        .unwrap();
    assert!(body.is_empty());
    let reply = reply.unwrap();
    assert_eq!(reply["patched"], true, "{reply}");
    assert_eq!(
        std::fs::read_to_string(root.join("m.py")).unwrap(),
        "a = 1\nb = 22\n"
    );
}

#[tokio::test]
async fn patch_text_send_is_redirected_to_binary_channel() {
    // Calling patch_text_file on the TEXT channel refuses with the binary-channel
    // guidance (it carries raw bytes), mirroring read_stream/write_stream.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let mut meta = Map::new();
    meta.insert(
        "ingress_rule".to_string(),
        serde_json::json!("allow_all_ingress"),
    );
    let (kernel, id) = mk_bridge(root, meta);
    let reply = FileBundle
        .handle(
            &id,
            &serde_json::json!({"type": "patch_text_file", "path": "m.py"}),
            &kernel,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(reply["error"].as_str().unwrap().contains("binary channel"));
}
