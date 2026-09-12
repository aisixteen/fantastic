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
    let err = resolve_safe("list", tmp.path(), "../escape").unwrap_err();
    assert_eq!(err["reason"], "transport_error");
    assert!(err["error"].as_str().unwrap().contains("escapes root"));
}

#[test]
fn resolve_safe_keeps_subpath() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = resolve_safe("list", tmp.path(), "sub/inner.txt").unwrap();
    assert!(p.starts_with(tmp.path()));
    assert!(p.ends_with("sub/inner.txt"));
}

#[test]
fn resolve_safe_refuses_absolute_path() {
    // `bad_arg` naming `path` — a root-relative path was wanted; the clamp
    // never silently re-roots (python used to `lstrip("/")`).
    let tmp = tempfile::TempDir::new().unwrap();
    let err = resolve_safe("read_stream", tmp.path(), "/etc/passwd").unwrap_err();
    assert_eq!(err["reason"], "bad_arg");
    assert_eq!(err["arg"], "path");
    assert!(err["error"].as_str().unwrap().contains("root-relative"));
}

#[test]
fn resolve_safe_dot_dot_after_descent_is_ok() {
    // a/b/.. resolves to root/a — still inside.
    let tmp = tempfile::TempDir::new().unwrap();
    let p = resolve_safe("list", tmp.path(), "a/b/../leaf.txt").unwrap();
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

// ─── the 2026-09-02 closing sweep — validation, symlinks, codes, posture ──

use fantastic_kernel::{Agent, Item};
use serde_json::{json, Map};

/// A kernel with ONE open file_bridge (`fb`) rooted at `tmp`'s served dir —
/// the running dir is `tmp` (root loader at `tmp/.fantastic`).
async fn mk_kernel(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let create = json!({
        "type": "create_agent", "parent_id": "kernel",
        "handler_module": HANDLER_MODULE, "id": "fb",
        "root": tmp.path().join("served").display().to_string(),
        "ingress_rule": "allow_all_ingress",
    });
    let r = kernel
        .send(vec![Item::new(AgentId::from("core"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "fb", "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from("fb"), payload)])
        .await
        .remove(0)
        .result
}

fn served(tmp: &tempfile::TempDir) -> PathBuf {
    let p = tmp.path().join("served");
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn negative_offset_and_zero_length_are_refused_not_fatal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("a.txt"), b"abc").unwrap();
    for (verb, header) in [
        ("read_stream", json!({"path": "a.txt", "offset": -1})),
        ("read_stream", json!({"path": "a.txt", "length": 0})),
        ("read_stream", json!({"path": "a.txt", "offset": "x"})),
        ("read_stream", json!({"path": "a.txt", "offset": true})),
        ("read_stream", json!({"path": "a.txt", "length": 1.5})),
        ("write_stream", json!({"path": "a.txt", "offset": -5})),
    ] {
        let r = if verb == "read_stream" {
            read_stream_reply(&root, &header).0
        } else {
            write_stream_reply(&root, &header, b"z")
        };
        assert_eq!(r["reason"], "bad_arg", "{header} → {r}");
        assert!(header.get(r["arg"].as_str().unwrap()).is_some(), "{r}");
    }
    // the legal shape still streams
    let (m, b) = read_stream_reply(&root, &json!({"path": "a.txt", "offset": 1, "length": 1}));
    assert_eq!(b, b"b");
    assert_eq!(m["next_offset"], 2);
    assert!(
        m.get("bytes_len").is_none(),
        "header keys match python: {m}"
    );
}

#[test]
fn huge_length_reads_only_what_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("a.txt"), b"abc").unwrap();
    // 1 PB used to be `vec![0u8; length]` — an allocation abort of the kernel.
    let (m, b) = read_stream_reply(
        &root,
        &json!({"path": "a.txt", "length": 1_000_000_000_000_000u64}),
    );
    assert_eq!(b, b"abc", "{m}");
    assert_eq!(m["eof"], true);
    let (m, b) = read_stream_reply(&root, &json!({"path": "a.txt", "length": i64::MAX}));
    assert_eq!(b, b"abc", "{m}");
}

#[test]
fn an_in_root_symlink_pointing_outside_refuses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("docs")).unwrap();
    // The lexical form is in-root; the canonical form is not.
    let (m, b) = read_stream_reply(&root, &json!({"path": "docs/secret.txt"}));
    assert_eq!(m["reason"], "transport_error", "{m}");
    assert!(m["error"].as_str().unwrap().contains("escapes root"));
    assert!(b.is_empty());
    let l = list_reply(&root, &json!({"path": "docs"}), &[]);
    assert_eq!(l["reason"], "transport_error", "{l}");
    let w = write_stream_reply(&root, &json!({"path": "docs/new.txt"}), b"x");
    assert_eq!(w["reason"], "transport_error", "{w}");
    assert!(!outside.join("new.txt").exists());
    // An in-root symlink still serves.
    std::fs::write(root.join("real.txt"), b"r").unwrap();
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("alias.txt")).unwrap();
    let (m, b) = read_stream_reply(&root, &json!({"path": "alias.txt"}));
    assert!(m.get("error").is_none(), "{m}");
    assert_eq!(b, b"r");
}

#[test]
fn delete_on_symlink_removes_the_link_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/keep.txt"), b"k").unwrap();
    std::os::unix::fs::symlink(root.join("data"), root.join("link")).unwrap();
    let r = delete_reply(&root, &json!({"path": "link", "recursive": true}));
    assert_eq!(r["deleted"], true, "{r}");
    assert!(
        std::fs::symlink_metadata(root.join("link")).is_err(),
        "link must be gone"
    );
    assert_eq!(std::fs::read(root.join("data/keep.txt")).unwrap(), b"k");
    // A DANGLING symlink is still a deletable name.
    std::os::unix::fs::symlink(root.join("gone.txt"), root.join("dangling")).unwrap();
    let r = delete_reply(&root, &json!({"path": "dangling"}));
    assert_eq!(r["deleted"], true, "{r}");
    assert!(std::fs::symlink_metadata(root.join("dangling")).is_err());
}

#[test]
fn bare_delete_refuses_a_populated_dir_recursive_removes_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.txt"), b"x").unwrap();
    let r = delete_reply(&root, &json!({"path": "src"}));
    assert_eq!(r["reason"], "transport_error", "{r}");
    assert!(root.join("src/a.txt").exists());
    let r = delete_reply(&root, &json!({"path": "src", "recursive": true}));
    assert_eq!(r, json!({"path": "src", "deleted": true}));
    assert!(!root.join("src").exists());
    // an EMPTY dir needs no flag
    std::fs::create_dir_all(root.join("empty")).unwrap();
    assert_eq!(
        delete_reply(&root, &json!({"path": "empty"}))["deleted"],
        true
    );
}

#[test]
fn empty_path_delete_rename_mkdir_are_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("keep.txt"), b"k").unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    for (payload, arg, reason) in [
        (
            json!({"path": "", "recursive": true}),
            "path",
            "missing_arg",
        ),
        (json!({"recursive": true}), "path", "missing_arg"),
        (
            json!({"path": ".", "recursive": true}),
            "path",
            "missing_arg",
        ),
        (
            json!({"path": "a/..", "recursive": true}),
            "path",
            "bad_arg",
        ),
    ] {
        let r = delete_reply(&root, &payload);
        assert_eq!(r["reason"], reason, "{payload} → {r}");
        assert_eq!(r["arg"], arg);
    }
    for (payload, arg, reason) in [
        (
            json!({"old_path": "", "new_path": "x"}),
            "old_path",
            "missing_arg",
        ),
        (
            json!({"old_path": "a", "new_path": ""}),
            "new_path",
            "missing_arg",
        ),
        (
            json!({"old_path": ".", "new_path": "x"}),
            "old_path",
            "missing_arg",
        ),
        (
            json!({"old_path": "a", "new_path": "a/.."}),
            "new_path",
            "bad_arg",
        ),
    ] {
        let r = rename_reply(&root, &payload);
        assert_eq!(r["reason"], reason, "{payload} → {r}");
        assert_eq!(r["arg"], arg);
    }
    for (payload, reason) in [
        (json!({"path": ""}), "missing_arg"),
        (json!({}), "missing_arg"),
        (json!({"path": "."}), "missing_arg"),
        (json!({"path": "a/.."}), "bad_arg"),
    ] {
        let r = mkdir_reply(&root, &payload);
        assert_eq!(r["reason"], reason, "{payload} → {r}");
        assert_eq!(r["arg"], "path");
    }
    assert!(root.join("keep.txt").exists() && root.join("a").is_dir());
}

#[test]
fn absolute_path_is_refused_with_bad_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("README.md"), b"r").unwrap();
    let replies = [
        read_stream_reply(&root, &json!({"path": "/README.md"})).0,
        write_stream_reply(&root, &json!({"path": "/x"}), b"b"),
        list_reply(&root, &json!({"path": "/"}), &[]),
        delete_reply(&root, &json!({"path": "/README.md"})),
        mkdir_reply(&root, &json!({"path": "/x"})),
        rename_reply(&root, &json!({"old_path": "/README.md", "new_path": "y"})),
    ];
    for r in replies {
        assert_eq!(r["reason"], "bad_arg", "{r}");
        assert!(
            r["error"].as_str().unwrap().contains("root-relative"),
            "{r}"
        );
    }
    assert_eq!(std::fs::read(root.join("README.md")).unwrap(), b"r");
}

#[test]
fn list_refusals_carry_codes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("f.txt"), b"f").unwrap();
    let r = list_reply(&root, &json!({"path": "nope"}), &[]);
    assert_eq!(r["reason"], "not_found", "{r}");
    assert_eq!(r["path"], "nope");
    let r = list_reply(&root, &json!({"path": "f.txt"}), &[]);
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "path");
    let r = rename_reply(&root, &json!({"old_path": "nope", "new_path": "x"}));
    assert_eq!(r["reason"], "not_found", "{r}");
    assert_eq!(r["path"], "nope");
    let r = delete_reply(&root, &json!({"path": "nope"}));
    assert_eq!(r["reason"], "not_found", "{r}");
}

#[tokio::test]
async fn reflect_carries_posture_and_boot_is_null() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let r = call(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(r["ingress_rule"], "allow_all_ingress", "{r}");
    assert_eq!(r["egress_rule"], "silent_egress");
    assert_eq!(r["sealed"], false);
    assert!(r.get("root_error").is_none());
    assert!(r["sentence"]
        .as_str()
        .unwrap()
        .starts_with("Filesystem edge of the io family"));
    assert_eq!(r["hidden_count"], DEFAULT_HIDDEN.len());
    assert_eq!(call(&kernel, json!({"type": "boot"})).await, Value::Null);
    // A root outside the running dir: reflect never refuses, it REPORTS.
    let mut meta = Map::new();
    meta.insert("root".into(), json!("/"));
    let r = reflect_reply(
        &AgentId::from("x"),
        &meta,
        Path::new("/"),
        false,
        &[],
        Some("root / escapes".into()),
    );
    assert_eq!(r["root_error"], "root / escapes");
    assert_eq!(r["ingress_rule"], "deny_ingress");
    assert_eq!(r["sealed"], true);
}

#[tokio::test]
async fn the_stream_verbs_serve_a_document_on_the_text_channel() {
    // PYTHON'S SHAPE. A model writes a document VERBATIM between `<bytes>` and
    // `</bytes>`; python's parser hands that to `write_stream` as utf-8 bytes on
    // the ordinary channel, and `read_stream` answers the chunk in `bytes`.
    // Rust refused both on the text channel until 2026-09-12 — 43 e2e logs
    // carried "raw bytes on the binary channel" against python's one, and the
    // `subai_graph` routine died on it five times in a run.
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let w = call(
        &kernel,
        json!({"type": "write_stream", "path": "a.txt", "truncate": true,
        "bytes": "line one\nline two, with \"quotes\"\n"}),
    )
    .await;
    assert!(w.get("error").is_none(), "{w}");
    assert_eq!(w["written"], 33, "{w}");
    let r = call(
        &kernel,
        json!({"type": "read_stream", "path": "a.txt", "offset": 0}),
    )
    .await;
    assert_eq!(r["bytes"], "line one\nline two, with \"quotes\"\n", "{r}");
    assert_eq!(r["eof"], true, "{r}");
    // A number is a document too (the tag grammar types a bare number).
    let n = call(
        &kernel,
        json!({"type": "write_stream", "path": "n.txt", "truncate": true, "bytes": 76}),
    )
    .await;
    assert!(n.get("error").is_none(), "{n}");
    // No document at all is still a coded refusal, naming the argument.
    let m = call(&kernel, json!({"type": "write_stream", "path": "b.txt"})).await;
    assert_eq!(m["reason"], "missing_arg", "{m}");
    assert_eq!(m["arg"], "bytes (one raw chunk)", "{m}"); // python names the chunk
    let r = call(&kernel, json!({"type": "frobnicate"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "frobnicate");
}

#[test]
fn password_ingress_is_refused_on_fs_and_process_legs() {
    // The fs half (the process half lives in fantastic-exec-bridge).
    let b = FileBundle;
    for (field, spec) in [
        ("ingress_rule", json!("password_ingress")),
        (
            "ingress_rule",
            json!({"type": "password_ingress", "env": "X"}),
        ),
        ("auth", json!("password")),
    ] {
        let mut meta = Map::new();
        meta.insert(field.to_string(), spec);
        let e = b
            .validate_meta(&meta, &Map::new())
            .unwrap_or_else(|| panic!("{field} password must be refused"));
        assert!(e.contains("wire envelope"), "{e}");
        assert!(e.contains("allow_all_ingress"), "{e}");
    }
    let mut ok = Map::new();
    ok.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    assert!(b.validate_meta(&ok, &Map::new()).is_none());
    // and the gate presents NO record token: a persisted `auth_token` opens nothing.
    let mut pw = Map::new();
    pw.insert(
        "ingress_rule".into(),
        json!({"type": "password_ingress", "env": "FANTASTIC_GROUP_TOKEN_FILE_UNIT"}),
    );
    pw.insert("auth_token".into(), json!("s3cret"));
    std::env::set_var("FANTASTIC_GROUP_TOKEN_FILE_UNIT", "s3cret");
    let denied =
        gate(&pw, &AgentId::from("fb"), "list").expect("record token must not open the leg");
    assert_eq!(denied["reason"], "unauthorized");
    std::env::remove_var("FANTASTIC_GROUP_TOKEN_FILE_UNIT");
}
