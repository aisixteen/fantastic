//! Unit tests for this bundle crate. Every text pinned here is python's.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;

/// A clamped root, as `handle` hands one to a verb.
/// The agent a reply's `origin` names — any id will do for the pure helpers.
fn fb() -> AgentId {
    AgentId::from("fb")
}

fn at(root: &Path) -> Result<PathBuf, String> {
    Ok(fs::realpath(root))
}

#[test]
fn readme_is_python_verbatim() {
    // A2.8 — byte-for-byte the python bundle's readme.
    let py = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/io/file_bridge/src/file_bridge/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[test]
fn resolve_refuses_escape() {
    let tmp = tempfile::TempDir::new().unwrap();
    let err = fs::resolve(tmp.path(), "../escape").unwrap_err();
    assert_eq!(err, "fs: path '../escape' escapes root");
}

#[test]
fn resolve_keeps_subpath() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = fs::resolve(tmp.path(), "sub/inner.txt").unwrap();
    assert!(p.starts_with(fs::realpath(tmp.path())));
    assert!(p.ends_with("sub/inner.txt"));
}

#[test]
fn resolve_refuses_absolute_path() {
    // python's `fs.resolve` refuses ANY leading '/', re-rooting nothing.
    let tmp = tempfile::TempDir::new().unwrap();
    let err = fs::resolve(tmp.path(), "/etc/passwd").unwrap_err();
    assert_eq!(err, "fs: path '/etc/passwd' escapes root");
    // The agent layer answers the same shape with `bad_arg` BEFORE reaching fs.
    let r = read_stream_reply(
        &fb(),
        &at(tmp.path()),
        &json!({"path": "/etc/passwd"}),
        true,
    )
    .0;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "path");
}

#[test]
fn resolve_dot_dot_after_descent_is_ok() {
    // a/b/.. resolves to root/a — still inside.
    let tmp = tempfile::TempDir::new().unwrap();
    let p = fs::resolve(tmp.path(), "a/b/../leaf.txt").unwrap();
    assert!(p.starts_with(fs::realpath(tmp.path())));
}

#[test]
fn write_stream_then_read_stream_round_trips_raw_bytes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(tmp.path());
    // Non-UTF-8 bytes — proves the channel is raw, not text/base64.
    let payload: Vec<u8> = vec![0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80];
    // SINK: first chunk truncates.
    let w = write_stream_reply(
        &root,
        &json!({"path": "blob.bin", "truncate": true}),
        Some(&payload),
        false,
    );
    assert_eq!(w["written"], payload.len());
    assert_eq!(w["size"], payload.len());
    assert!(w.get("error").is_none(), "{w}");
    // SOURCE: read it all back off the binary channel.
    let (meta, body) = read_stream_reply(&fb(), &root, &json!({"path": "blob.bin"}), false);
    assert!(meta.get("error").is_none(), "{meta}");
    assert_eq!(
        body,
        Some(payload.clone()),
        "bytes must round-trip verbatim"
    );
    assert_eq!(meta["eof"], true);
    assert_eq!(meta["size"], payload.len());
    assert_eq!(meta["next_offset"], payload.len());
}

#[test]
fn read_stream_chunks_with_offset_and_eof() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(tmp.path());
    let payload: Vec<u8> = (0u8..=255).collect(); // 256 bytes
    write_stream_reply(
        &root,
        &json!({"path": "big.bin", "truncate": true}),
        Some(&payload),
        false,
    );
    // First 100 bytes.
    let (m1, b1) = read_stream_reply(
        &fb(),
        &root,
        &json!({"path": "big.bin", "offset": 0, "length": 100}),
        false,
    );
    let b1 = b1.expect("a chunk came back");
    assert_eq!(b1.len(), 100);
    assert_eq!(m1["eof"], false);
    assert_eq!(m1["next_offset"], 100);
    // Resume from next_offset to the end.
    let (m2, b2) = read_stream_reply(
        &fb(),
        &root,
        &json!({"path": "big.bin", "offset": 100, "length": 1000}),
        false,
    );
    let b2 = b2.expect("a chunk came back");
    assert_eq!(b2.len(), 156);
    assert_eq!(m2["eof"], true);
    let mut joined = b1;
    joined.extend(b2);
    assert_eq!(joined, payload);
}

#[test]
fn write_stream_appends_when_offset_omitted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(tmp.path());
    write_stream_reply(
        &root,
        &json!({"path": "a.bin", "truncate": true}),
        Some(b"AAAA"),
        false,
    );
    let w = write_stream_reply(&root, &json!({"path": "a.bin"}), Some(b"BBBB"), false);
    assert_eq!(w["offset"], 4); // appended at end
    assert_eq!(w["size"], 8);
    let (_m, body) = read_stream_reply(&fb(), &root, &json!({"path": "a.bin"}), false);
    assert_eq!(body, Some(b"AAAABBBB".to_vec()));
}

#[test]
fn write_stream_without_bytes_writes_an_empty_chunk() {
    // A2.7 — python's `payload.get("bytes", b"")`: an ABSENT key is an empty
    // chunk (a legal write); an explicit `null` is `missing_arg`.
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let root = at(&served);
    let w = write_stream_reply(
        &root,
        &json!({"path": "e.txt", "truncate": true}),
        Some(b""),
        false,
    );
    assert_eq!(
        w,
        json!({"path": "e.txt", "written": 0, "offset": 0, "size": 0})
    );
    assert!(served.join("e.txt").exists());
    let m = missing_bytes();
    assert_eq!(m["reason"], "missing_arg");
    assert_eq!(m["arg"], "bytes (one raw chunk)");
}

#[test]
fn root_escape_error_is_python_text() {
    // A2.2 — the running-dir law, in python's words, quoting the CONFIGURED root.
    let base = tempfile::TempDir::new().unwrap();
    let b = base.path();
    assert!(fs::resolve_root(".fantastic", b).is_ok());
    assert!(fs::resolve_root("sub/dir", b).is_ok());
    assert!(fs::resolve_root(&b.join("served").display().to_string(), b).is_ok());
    assert_eq!(
        fs::resolve_root("/", b).unwrap_err(),
        "fs: root '/' escapes the running dir"
    );
    assert_eq!(
        fs::resolve_root("../x", b).unwrap_err(),
        "fs: root '../x' escapes the running dir"
    );
}

#[test]
fn root_symlink_pointing_outside_the_running_dir_refuses() {
    // A2.19 — the clamp resolves symlinks on BOTH sides; a lexical compare used
    // to admit an outward symlinked root.
    let base = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), base.path().join("out")).unwrap();
    assert_eq!(
        fs::resolve_root("out", base.path()).unwrap_err(),
        "fs: root 'out' escapes the running dir"
    );
}

#[test]
fn tilde_root_is_literal_not_home() {
    // A2.17 — python's `fs` never expands `~`; the name is a directory.
    let base = tempfile::TempDir::new().unwrap();
    let resolved = fs::resolve_root("~/projects", base.path()).unwrap();
    assert_eq!(resolved, fs::realpath(base.path()).join("~/projects"));
}

#[test]
fn gate_seals_by_default_and_opens_with_allow_all() {
    let id = AgentId::from("fb");
    // No ingress_rule ⇒ SEALED: every verb but `reflect` denied — `boot` too.
    let bare = Map::new();
    assert!(gate(&bare, &id, "read_stream").is_some());
    assert!(gate(&bare, &id, "write_stream").is_some());
    assert!(gate(&bare, &id, "list").is_some());
    assert!(gate(&bare, &id, "boot").is_some());
    let denied = gate(&bare, &id, "list").unwrap();
    assert_eq!(denied["reason"], "unauthorized");
    // ingress_rule=allow_all_ingress ⇒ OPEN.
    let mut open = Map::new();
    open.insert("ingress_rule".into(), json!("allow_all_ingress"));
    assert!(gate(&open, &id, "read_stream").is_none());
    assert!(gate(&open, &id, "write_stream").is_none());
    assert!(gate(&open, &id, "boot").is_none());
}

// ─── kernel-driven ───────────────────────────────────────────────────

use serde_json::Map as JsonMap;

/// A kernel with ONE open file_bridge (`fb`) rooted at `tmp`'s served dir —
/// the running dir is `tmp` (root loader at `tmp/.fantastic`).
async fn mk_kernel(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    mk_kernel_with(
        tmp,
        json!({"ingress_rule": "allow_all_ingress", "allow": ["."]}),
    )
    .await
}

/// …with extra record fields (posture, `readonly`, a different `root`).
async fn mk_kernel_with(tmp: &tempfile::TempDir, extra: Value) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        JsonMap::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel",
        "handler_module": HANDLER_MODULE, "id": "fb",
        "root": tmp.path().join("served").display().to_string(),
    });
    for (k, v) in extra.as_object().expect("extra is an object") {
        create[k] = v.clone();
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
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
    let root = at(&served(&tmp));
    std::fs::write(served(&tmp).join("a.txt"), b"abc").unwrap();
    for (verb, header) in [
        ("read_stream", json!({"path": "a.txt", "offset": -1})),
        ("read_stream", json!({"path": "a.txt", "length": 0})),
        ("read_stream", json!({"path": "a.txt", "offset": "x"})),
        ("read_stream", json!({"path": "a.txt", "offset": true})),
        ("read_stream", json!({"path": "a.txt", "length": 1.5})),
        ("write_stream", json!({"path": "a.txt", "offset": -5})),
    ] {
        let r = if verb == "read_stream" {
            read_stream_reply(&fb(), &root, &header, true).0
        } else {
            write_stream_reply(&root, &header, Some(b"z"), false)
        };
        assert_eq!(r["reason"], "bad_arg", "{header} → {r}");
        assert!(header.get(r["arg"].as_str().unwrap()).is_some(), "{r}");
    }
    // the legal shape still streams
    let (m, b) = read_stream_reply(
        &fb(),
        &root,
        &json!({"path": "a.txt", "offset": 1, "length": 1}),
        false,
    );
    assert_eq!(b, Some(b"b".to_vec()));
    assert_eq!(m["next_offset"], 2);
}

#[test]
fn text_read_stream_reply_key_order() {
    // A2.6 — python's key order, measured: path, offset, bytes, next_offset,
    // eof, size, origin (how to get these bytes again — what a caller's memory
    // keeps instead of the body). The binary channel carries the chunk in the
    // body instead.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(&served(&tmp));
    std::fs::write(served(&tmp).join("a.txt"), b"abc").unwrap();
    let (m, _) = read_stream_reply(&fb(), &root, &json!({"path": "a.txt"}), true);
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "path",
            "offset",
            "bytes",
            "next_offset",
            "eof",
            "size",
            "origin"
        ]
    );
    assert_eq!(m["bytes"], "abc");
    assert_eq!(
        m["origin"],
        "fb a.txt bytes 0-3 of 3 — read_stream {path: a.txt, offset: 0, length: 3}"
    );
    // A11.9 — the BINARY channel keeps the same dict, `bytes` nulled IN PLACE:
    // python's codec lifts the value onto the wire and leaves a `null` behind,
    // so the two channels hand a reader the same shape. Omitting the key gave
    // the browser a different dict per channel, and the WS layer had nothing to
    // pick the frame kind from.
    let (m, body) = read_stream_reply(&fb(), &root, &json!({"path": "a.txt"}), false);
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "path",
            "offset",
            "bytes",
            "next_offset",
            "eof",
            "size",
            "origin"
        ]
    );
    assert_eq!(m["bytes"], Value::Null);
    assert_eq!(body, Some(b"abc".to_vec()));
}

#[test]
fn huge_length_reads_only_what_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(&served(&tmp));
    std::fs::write(served(&tmp).join("a.txt"), b"abc").unwrap();
    // 1 PB used to be `vec![0u8; length]` — an allocation abort of the kernel.
    let (m, b) = read_stream_reply(
        &fb(),
        &root,
        &json!({"path": "a.txt", "length": 1_000_000_000_000_000u64}),
        false,
    );
    assert_eq!(b, Some(b"abc".to_vec()), "{m}");
    assert_eq!(m["eof"], true);
    let (m, b) = read_stream_reply(
        &fb(),
        &root,
        &json!({"path": "a.txt", "length": i64::MAX}),
        false,
    );
    assert_eq!(b, Some(b"abc".to_vec()), "{m}");
}

#[test]
fn an_in_root_symlink_pointing_outside_refuses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("docs")).unwrap();
    let r = at(&root);
    // The lexical form is in-root; the canonical form is not.
    let (m, b) = read_stream_reply(&fb(), &r, &json!({"path": "docs/secret.txt"}), false);
    assert_eq!(m["reason"], "transport_error", "{m}");
    assert_eq!(m["error"], "fs: path 'docs/secret.txt' escapes root");
    assert_eq!(b, None);
    let l = list_reply(&fb(), &r, &json!({"path": "docs"}));
    assert_eq!(l["reason"], "transport_error", "{l}");
    let w = write_stream_reply(&r, &json!({"path": "docs/new.txt"}), Some(b"x"), false);
    assert_eq!(w["reason"], "transport_error", "{w}");
    assert!(!outside.join("new.txt").exists());
    // An in-root symlink still serves.
    std::fs::write(root.join("real.txt"), b"r").unwrap();
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("alias.txt")).unwrap();
    let (m, b) = read_stream_reply(&fb(), &r, &json!({"path": "alias.txt"}), false);
    assert!(m.get("error").is_none(), "{m}");
    assert_eq!(b, Some(b"r".to_vec()));
}

#[test]
fn dangling_symlink_pointing_outside_root_refuses() {
    // A2.15 — `realpath(strict=False)` follows a dangling link to where it
    // POINTS; outside the root, that is an escape, and nothing is created
    // through it.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(outside.join("gone.txt"), root.join("dangling")).unwrap();
    let r = at(&root);
    let w = write_stream_reply(&r, &json!({"path": "dangling"}), Some(b"x"), false);
    assert_eq!(w["reason"], "transport_error", "{w}");
    assert_eq!(w["error"], "fs: path 'dangling' escapes root");
    assert!(!outside.join("gone.txt").exists());
    let (m, _) = read_stream_reply(&fb(), &r, &json!({"path": "dangling"}), false);
    assert_eq!(m["error"], "fs: path 'dangling' escapes root", "{m}");
    // A dangling link pointing INSIDE the root is still a deletable name.
    std::os::unix::fs::symlink(root.join("gone.txt"), root.join("inside")).unwrap();
    let d = delete_reply(&r, &json!({"path": "inside"}), false);
    assert_eq!(d, json!({"path": "inside", "deleted": true}), "{d}");
}

#[test]
fn delete_on_symlink_removes_the_link_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/keep.txt"), b"k").unwrap();
    std::os::unix::fs::symlink(root.join("data"), root.join("link")).unwrap();
    let r = at(&root);
    let reply = delete_reply(&r, &json!({"path": "link", "recursive": true}), false);
    assert_eq!(reply["deleted"], true, "{reply}");
    assert!(
        std::fs::symlink_metadata(root.join("link")).is_err(),
        "link must be gone"
    );
    assert_eq!(std::fs::read(root.join("data/keep.txt")).unwrap(), b"k");
}

#[test]
fn bare_delete_refuses_a_populated_dir_recursive_removes_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.txt"), b"x").unwrap();
    let r = at(&root);
    let reply = delete_reply(&r, &json!({"path": "src"}), false);
    assert_eq!(reply["reason"], "transport_error", "{reply}");
    assert!(root.join("src/a.txt").exists());
    let reply = delete_reply(&r, &json!({"path": "src", "recursive": true}), false);
    assert_eq!(reply, json!({"path": "src", "deleted": true}));
    assert!(!root.join("src").exists());
    // an EMPTY dir needs no flag
    std::fs::create_dir_all(root.join("empty")).unwrap();
    assert_eq!(
        delete_reply(&r, &json!({"path": "empty"}), false)["deleted"],
        true
    );
}

#[test]
fn os_error_text_is_python_oserror_str() {
    // A2.12 — every OS failure renders as python's `OSError.__str__`.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.txt"), b"x").unwrap();
    std::fs::write(root.join("f.txt"), b"x").unwrap();
    let r = at(&root);
    let full = fs::realpath(&root);
    let reply = delete_reply(&r, &json!({"path": "src"}), false);
    assert_eq!(
        reply["error"],
        json!(format!(
            "[Errno 66] Directory not empty: '{}'",
            full.join("src").display()
        )),
        "{reply}"
    );
    // mkdir THROUGH a file: python names the path whose `mkdir` failed.
    let reply = mkdir_reply(&r, &json!({"path": "f.txt/x"}), false);
    assert_eq!(
        reply["error"],
        json!(format!(
            "[Errno 20] Not a directory: '{}'",
            full.join("f.txt/x").display()
        )),
        "{reply}"
    );
    // mkdir over an existing FILE keeps python's FileExistsError.
    let reply = mkdir_reply(&r, &json!({"path": "f.txt"}), false);
    assert_eq!(
        reply["error"],
        json!(format!(
            "[Errno 17] File exists: '{}'",
            full.join("f.txt").display()
        )),
        "{reply}"
    );
}

#[test]
fn empty_path_delete_rename_mkdir_are_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("keep.txt"), b"k").unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    let r = at(&root);
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
        let reply = delete_reply(&r, &payload, false);
        assert_eq!(reply["reason"], reason, "{payload} → {reply}");
        assert_eq!(reply["arg"], arg);
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
        let reply = rename_reply(&r, &payload, false);
        assert_eq!(reply["reason"], reason, "{payload} → {reply}");
        assert_eq!(reply["arg"], arg);
    }
    for (payload, reason) in [
        (json!({"path": ""}), "missing_arg"),
        (json!({}), "missing_arg"),
        (json!({"path": "."}), "missing_arg"),
        (json!({"path": "a/.."}), "bad_arg"),
    ] {
        let reply = mkdir_reply(&r, &payload, false);
        assert_eq!(reply["reason"], reason, "{payload} → {reply}");
        assert_eq!(reply["arg"], "path");
    }
    assert!(root.join("keep.txt").exists() && root.join("a").is_dir());
}

#[test]
fn absolute_path_is_refused_with_bad_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("README.md"), b"r").unwrap();
    let r = at(&root);
    let replies = [
        read_stream_reply(&fb(), &r, &json!({"path": "/README.md"}), true).0,
        write_stream_reply(&r, &json!({"path": "/x"}), Some(b"b"), false),
        list_reply(&fb(), &r, &json!({"path": "/"})),
        delete_reply(&r, &json!({"path": "/README.md"}), false),
        mkdir_reply(&r, &json!({"path": "/x"}), false),
        rename_reply(
            &r,
            &json!({"old_path": "/README.md", "new_path": "y"}),
            false,
        ),
    ];
    for reply in replies {
        assert_eq!(reply["reason"], "bad_arg", "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains("root-relative"),
            "{reply}"
        );
    }
    assert_eq!(std::fs::read(root.join("README.md")).unwrap(), b"r");
}

#[test]
fn refusal_texts_use_python_repr() {
    // A2.3 — `{x!r}` on every refusal that names a path or a verb.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("f.txt"), b"f").unwrap();
    let r = at(&root);
    let reply = list_reply(&fb(), &r, &json!({"path": "nope"}));
    assert_eq!(reply["error"], "path 'nope' does not exist");
    assert_eq!(reply["reason"], "not_found");
    assert_eq!(reply["path"], "nope");
    let reply = list_reply(&fb(), &r, &json!({"path": "f.txt"}));
    assert_eq!(
        reply["error"],
        "list: path must be a directory; 'f.txt' is not one"
    );
    assert_eq!(reply["reason"], "bad_arg");
    let reply = rename_reply(&r, &json!({"old_path": "nope", "new_path": "x"}), false);
    assert_eq!(reply["error"], "path 'nope' does not exist");
    assert_eq!(reply["reason"], "not_found");
    let reply = delete_reply(&r, &json!({"path": "nope"}), false);
    assert_eq!(reply["error"], "path 'nope' does not exist");
    let (reply, _) = read_stream_reply(&fb(), &r, &json!({"path": "nope"}), true);
    assert_eq!(reply["error"], "file 'nope' not found");
    assert_eq!(reply["reason"], "not_found");
    // a path holding a quote quotes python's way
    let reply = list_reply(&fb(), &r, &json!({"path": "it's"}));
    assert_eq!(reply["error"], "path \"it's\" does not exist");
}

#[test]
fn list_paths_are_relative_to_resolved_root() {
    // A2.11 — the entry path is relative to the RESOLVED root, and a stat that
    // fails reports -1.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("x")).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("x/a.txt"), b"ab").unwrap();
    std::os::unix::fs::symlink(root.join("x/gone.txt"), root.join("x/dangling")).unwrap();
    let r = at(&root);
    let reply = list_reply(&fb(), &r, &json!({"path": "sub/../x"}));
    let files = reply["files"].as_array().unwrap();
    assert_eq!(
        files
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["x/a.txt", "x/dangling"]
    );
    assert_eq!(files[0]["size"], 2);
    assert_eq!(files[1]["size"], -1, "a dangling link stats as -1");
    let reply = list_reply(&fb(), &r, &json!({"path": "."}));
    let names: Vec<&str> = reply["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sub", "x"]);
}

#[test]
fn truncate_and_recursive_use_python_truthiness() {
    // A2.13 — `bool(payload.get(...))`, not a JSON bool test.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let r = at(&root);
    write_stream_reply(
        &root_ok(&root),
        &json!({"path": "t.txt"}),
        Some(b"AAAA"),
        false,
    );
    // a truthy non-bool truncates
    let w = write_stream_reply(
        &r,
        &json!({"path": "t.txt", "truncate": "yes"}),
        Some(b"B"),
        false,
    );
    assert_eq!(w["size"], 1, "{w}");
    // a falsy non-bool does not
    let w = write_stream_reply(
        &r,
        &json!({"path": "t.txt", "truncate": 0}),
        Some(b"C"),
        false,
    );
    assert_eq!(w["size"], 2, "{w}");
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/x"), b"x").unwrap();
    let reply = delete_reply(&r, &json!({"path": "d", "recursive": []}), false);
    assert_eq!(reply["reason"], "transport_error", "{reply}");
    let reply = delete_reply(&r, &json!({"path": "d", "recursive": "y"}), false);
    assert_eq!(reply["deleted"], true, "{reply}");
}

fn root_ok(p: &Path) -> Result<PathBuf, String> {
    at(p)
}

#[test]
fn auth_stem_is_not_judged_as_an_ingress_rule() {
    // A2.18 — python judges only a PRESENT `ingress_rule`.
    let b = FileBundle::default();
    let mut auth_only = Map::new();
    auth_only.insert("auth".into(), json!("nonsense"));
    assert!(b.validate_meta(&auth_only, &Map::new()).is_none());
    let mut scoped = Map::new();
    scoped.insert("ingress_rule".into(), json!("nonsense"));
    assert_eq!(
        b.validate_meta(&scoped, &Map::new()).unwrap(),
        "ingress_rule='nonsense' is not a rule this bundle implements — \
         legal values: allow_all_ingress, deny_ingress"
    );
    // A rule OBJECT is reported as python's dict repr.
    let mut obj = Map::new();
    obj.insert("ingress_rule".into(), json!({"type": "nope", "env": "X"}));
    assert_eq!(
        b.validate_meta(&obj, &Map::new()).unwrap(),
        "ingress_rule={'type': 'nope', 'env': 'X'} is not a rule this bundle implements — \
         legal values: allow_all_ingress, deny_ingress"
    );
    // A type-less rule object is a plain seal, not a refusal.
    let mut typeless = Map::new();
    typeless.insert("ingress_rule".into(), json!({"env": "X"}));
    assert!(b.validate_meta(&typeless, &Map::new()).is_none());
}

#[test]
fn password_ingress_is_refused_on_fs_and_process_legs() {
    // The fs half (the process half lives in fantastic-exec-bridge).
    let b = FileBundle::default();
    for (field, spec, shown) in [
        (
            "ingress_rule",
            json!("password_ingress"),
            "'password_ingress'",
        ),
        (
            "ingress_rule",
            json!({"type": "password_ingress", "env": "X"}),
            "{'type': 'password_ingress', 'env': 'X'}",
        ),
        ("auth", json!("password"), "'password'"),
    ] {
        let mut meta = Map::new();
        meta.insert(field.to_string(), spec);
        let e = b
            .validate_meta(&meta, &Map::new())
            .unwrap_or_else(|| panic!("{field} password must be refused"));
        assert_eq!(
            e,
            format!(
                "{field}={shown} is not applicable to the fs edge: a leg with no wire \
                 envelope can never present a token — legal values: \
                 allow_all_ingress, deny_ingress"
            )
        );
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

#[tokio::test]
async fn reflect_key_order_matches_python() {
    // A2.1 + A2.16 + A2.17 — python's key order, docstring verbs and the
    // RESOLVED absolute root.
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let r = call(&kernel, json!({"type": "reflect"})).await;
    // The BUNDLE's keys, in python's order; the kernel appends its own frame
    // (can_call/callable_by/…) after them.
    let keys: Vec<&str> = r
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .take(11)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "sentence",
            "root",
            "readonly",
            "allow_count",
            "edit_grammar",
            "compose",
            "verbs",
            "ingress_rule",
            "egress_rule",
            "sealed",
        ]
    );
    assert_eq!(
        r["root"],
        json!(fs::realpath(&served).display().to_string())
    );
    assert_eq!(r["ingress_rule"], "allow_all_ingress");
    assert_eq!(r["egress_rule"], "silent_egress");
    assert_eq!(r["sealed"], false);
    assert_eq!(
        r["sentence"],
        "Filesystem edge of the io family — sealed by default, running-dir only."
    );
    assert_eq!(call(&kernel, json!({"type": "boot"})).await, Value::Null);
}

#[test]
fn reflect_verbs_are_python_docstring_first_lines() {
    // A2.16 — the first line of each python docstring, in python's VERBS order.
    let reply = reflect_reply(&AgentId::from("fb"), &Map::new(), &Ok(PathBuf::from("/")));
    let verbs = reply["verbs"].as_object().unwrap();
    assert_eq!(
        verbs.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "reflect",
            "boot",
            "list",
            "read",
            "write",
            "edit",
            "append",
            "undo",
            "read_stream",
            "write_stream",
            "pump",
            "delete",
            "rename",
            "mkdir",
            "allow",
            "disallow",
            "allowed",
        ]
    );
    assert_eq!(verbs["boot"], "No-op. Returns None.");
    assert_eq!(
        verbs["reflect"],
        "Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed/see). No args."
    );
    assert_eq!(
        verbs["read_stream"],
        "args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns ONE chunk {path, offset, bytes, next_offset, eof, size}."
    );
}

#[tokio::test]
async fn reflect_root_is_resolved_absolute_path() {
    // A2.17 — an escaping root is shown as CONFIGURED, with the violation.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("root".into(), json!("../nope"));
    let r = reflect_reply(&AgentId::from("fb"), &meta, &Ok(tmp.path().to_path_buf()));
    assert_eq!(r["root"], "../nope");
    assert_eq!(
        r["root_error"],
        "fs: root '../nope' escapes the running dir"
    );
    assert_eq!(
        r.as_object()
            .unwrap()
            .keys()
            .next_back()
            .map(String::as_str),
        Some("root_error"),
        "root_error stays last"
    );
    // An ABSENT root serves the workdir base.
    let r = reflect_reply(
        &AgentId::from("fb"),
        &Map::new(),
        &Ok(tmp.path().to_path_buf()),
    );
    assert_eq!(
        r["root"],
        json!(fs::realpath(tmp.path()).display().to_string())
    );
    assert!(r.get("root_error").is_none());
}

#[test]
fn reflect_posture_scopes_an_auth_stem_like_the_gate() {
    // python `test_describe_scopes_like_the_gate`: an already-suffixed `auth`
    // stem is suffixed AGAIN — exactly what the gate resolves (an unknown rule,
    // so the leg is sealed) — and reflect reports that, not a name it would
    // not honour.
    let mut meta = Map::new();
    meta.insert("auth".into(), json!("allow_all_ingress"));
    let r = reflect_reply(&AgentId::from("fb"), &meta, &Ok(PathBuf::from("/")));
    assert_eq!(r["ingress_rule"], "allow_all_ingress_ingress");
    assert_eq!(r["egress_rule"], "allow_all_ingress_egress");
    assert_eq!(r["sealed"], true);
    // `allow_all` / `deny` are the two stems that mean "present nothing" outbound.
    let mut meta = Map::new();
    meta.insert("auth".into(), json!("allow_all"));
    let r = reflect_reply(&AgentId::from("fb"), &meta, &Ok(PathBuf::from("/")));
    assert_eq!(r["ingress_rule"], "allow_all_ingress");
    assert_eq!(r["egress_rule"], "silent_egress");
    // A bare stem is scoped per manifold.
    let mut meta = Map::new();
    meta.insert("auth".into(), json!("password"));
    let r = reflect_reply(&AgentId::from("fb"), &meta, &Ok(PathBuf::from("/")));
    assert_eq!(r["ingress_rule"], "password_ingress");
    assert_eq!(r["egress_rule"], "password_egress");
    assert_eq!(r["sealed"], true);
    // No posture at all ⇒ sealed by default.
    let r = reflect_reply(&AgentId::from("fb"), &Map::new(), &Ok(PathBuf::from("/")));
    assert_eq!(r["ingress_rule"], "deny_ingress");
    assert_eq!(r["egress_rule"], "silent_egress");
    assert_eq!(r["sealed"], true);
}

#[tokio::test]
async fn unknown_verb_on_sealed_bridge_is_unhandled() {
    // A2.4 — the verb lookup runs BEFORE the gate, so a sealed bridge still says
    // "unknown type" instead of "unauthorized".
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    let r = call(&kernel, json!({"type": "frobnicate"})).await;
    assert_eq!(
        r,
        json!({
            "error": "file_bridge: unknown type 'frobnicate'",
            "reason": "unhandled",
            "verb": "frobnicate",
        })
    );
    // A payload with no `type` never reaches a bundle in either runtime (the
    // kernel refuses it), so the bundle's `None` rendering is asserted directly.
    let bundle = FileBundle::default();
    let reply = bundle
        .handle(&AgentId::from("fb"), &json!({}), &kernel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reply,
        json!({
            "error": "file_bridge: unknown type None",
            "reason": "unhandled",
            "verb": null,
        })
    );
}

#[tokio::test]
async fn sealed_boot_is_denied_and_shutdown_is_unhandled() {
    // A2.5 — the gate exemption covers `reflect` only.
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    let r = call(&kernel, json!({"type": "boot"})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    let r = call(&kernel, json!({"type": "shutdown"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "shutdown");
    // reflect still answers on a sealed leg.
    let r = call(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(r["sealed"], true, "{r}");
}

#[tokio::test]
async fn readonly_wins_over_root_escape() {
    // A2.9 — the check order is readonly → arguments → root clamp → path clamp.
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(
        &tmp,
        json!({"ingress_rule": "allow_all_ingress", "readonly": "yes", "root": "../outside"}),
    )
    .await;
    for verb in ["delete", "rename", "mkdir", "write_stream"] {
        let r = call(&kernel, json!({"type": verb})).await;
        assert_eq!(r["reason"], "unauthorized", "{verb} → {r}");
        assert_eq!(r["error"], "agent is readonly");
    }
    // a READ verb on the same bridge reaches the clamp and reports it
    let r = call(&kernel, json!({"type": "list"})).await;
    assert_eq!(r["reason"], "transport_error", "{r}");
    assert_eq!(r["error"], "fs: root '../outside' escapes the running dir");
    // a BAD ARGUMENT is answered before the clamp
    let r = call(&kernel, json!({"type": "list", "path": 7})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["error"], "list: path must be a string");
}

#[tokio::test]
async fn text_write_stream_with_string_bytes_is_missing_arg() {
    // A13.19 — `bytes` IS RAW BYTES, and JSON has none. Python types the
    // argument (`isinstance(data, (bytes, bytearray))`), so a wire client's
    // JSON string or number is `missing_arg` and NOTHING IS WRITTEN; rust used
    // to accept both and write the text. A model's `<bytes>` document still
    // works because ai_core lifts it onto the item's binary channel, which is
    // the bytes-typed carrier — the case below.
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    for doc in [json!("line one\n"), json!(76), json!(null), json!(["a"])] {
        let m = call(
            &kernel,
            json!({"type": "write_stream", "path": "a.txt", "truncate": true, "bytes": doc}),
        )
        .await;
        assert_eq!(m["reason"], "missing_arg", "{doc} → {m}");
        assert_eq!(m["arg"], "bytes (one raw chunk)", "{doc} → {m}");
        assert_eq!(m["error"], "write_stream: bytes (one raw chunk) required");
        assert!(!dir.join("a.txt").exists(), "{doc} wrote anyway");
    }
    // THE RAW CHUNK, on the binary channel: this is the path a model's
    // `<bytes>` document takes, and it writes.
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("fb"),
            json!({"type": "write_stream", "path": "a.txt", "truncate": true}),
        )
        .with_binary(b"line one\n".to_vec())])
        .await
        .remove(0)
        .result;
    assert_eq!(out["written"], 9, "{out}");
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"line one\n");

    // …and it comes back as a CHUNK, in the reply body, with the reply's own
    // `bytes` key nulled in place (A11.9).
    let back = kernel
        .send(vec![Item::new(
            AgentId::from("fb"),
            json!({"type": "read_stream", "path": "a.txt", "offset": 0}),
        )
        .with_binary(Vec::new())])
        .await
        .remove(0);
    assert_eq!(back.result["bytes"], Value::Null, "{}", back.result);
    assert_eq!(back.result["eof"], true, "{}", back.result);
    assert_eq!(back.body, Some(b"line one\n".to_vec()));
}

#[tokio::test]
async fn pump_falsy_args_fall_back_like_python() {
    // A2.10 — `payload.get(k) or <fallback>` on every pump argument.
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    std::fs::write(served.join("src.txt"), b"hello pump").unwrap();
    // Every falsy argument falls back: source/sink to THIS agent, `sink_path` to
    // `source_path` — so this pumps the file onto itself in one chunk.
    let r = call(
        &kernel,
        json!({"type": "pump", "source": "", "sink": null, "source_path": "src.txt",
               "sink_path": "", "chunk": 0}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "a chunk floor of 1: {r}");
    let r = call(
        &kernel,
        json!({"type": "pump", "source": "", "sink": null, "source_path": "src.txt",
               "sink_path": ""}),
    )
    .await;
    assert_eq!(
        r,
        json!({"source": "src.txt", "sink": "src.txt", "bytes": 10, "chunks": 1}),
        "{r}"
    );
    assert_eq!(
        std::fs::read(served.join("src.txt")).unwrap(),
        b"hello pump"
    );
    // A real copy, chunk by chunk, to another path.
    let r = call(
        &kernel,
        json!({"type": "pump", "path": "src.txt", "sink_path": "copy.txt", "chunk": 4}),
    )
    .await;
    assert_eq!(
        r,
        json!({"source": "src.txt", "sink": "copy.txt", "bytes": 10, "chunks": 3}),
        "{r}"
    );
    assert_eq!(
        std::fs::read(served.join("copy.txt")).unwrap(),
        b"hello pump"
    );
    // `path` stands in for `source_path`, and a failed read names the SOURCE
    // with python's repr and prints the reply as python prints a dict.
    let r = call(
        &kernel,
        json!({"type": "pump", "source": "nobody", "path": "src.txt"}),
    )
    .await;
    assert_eq!(r["reason"], "transport_error", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("pump: read from 'nobody' failed: {'error':"),
        "{r}"
    );
}

// ─── the allow list — python `file_bridge/tests/test_allow_list.py` ───────────

fn site(tmp: &tempfile::TempDir) {
    let s = served(tmp);
    std::fs::create_dir_all(s.join("docs")).unwrap();
    std::fs::write(s.join("docs/a.pdf"), b"%PDF-a").unwrap();
    std::fs::write(s.join("docs/b.pdf"), b"%PDF-b").unwrap();
    std::fs::write(s.join("secret.txt"), b"private\n").unwrap();
}

async fn bridge(tmp: &tempfile::TempDir, allow: Option<Value>) -> Arc<Kernel> {
    site(tmp);
    let mut meta = json!({"ingress_rule": "allow_all_ingress"});
    if let Some(a) = allow {
        meta["allow"] = a;
    }
    mk_kernel_with(tmp, meta).await
}

async fn read(kernel: &Arc<Kernel>, path: &str) -> Value {
    call(kernel, json!({"type": "read_stream", "path": path})).await
}

#[tokio::test]
async fn a_bridge_with_no_list_exposes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, None).await;
    for path in ["docs/a.pdf", "secret.txt", "missing.txt"] {
        let r = read(&k, path).await;
        assert_eq!(r["reason"], "not_found", "{path}: {r}");
        assert!(r["error"].as_str().unwrap().contains("not exposed"), "{r}");
    }
    assert_eq!(
        call(&k, json!({"type": "list"})).await["reason"],
        "not_found"
    );
    let w = call(&k, json!({"type": "write_stream", "path": "new.txt"})).await;
    assert_eq!(w["reason"], "not_found", "{w}");
    assert!(!served(&tmp).join("new.txt").exists());
}

#[tokio::test]
async fn one_file_exposes_that_file_and_the_way_to_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, Some(json!(["docs/a.pdf"]))).await;
    assert_eq!(read(&k, "docs/a.pdf").await["size"], 6);
    assert_eq!(read(&k, "docs/b.pdf").await["reason"], "not_found");
    assert_eq!(read(&k, "secret.txt").await["reason"], "not_found");
    let top = call(&k, json!({"type": "list"})).await;
    let names: Vec<&str> = top["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["docs"]);
    let docs = call(&k, json!({"type": "list", "path": "docs"})).await;
    let names: Vec<&str> = docs["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a.pdf"]);
}

#[tokio::test]
async fn a_directory_exposes_everything_under_it_and_dot_the_whole_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, Some(json!(["docs"]))).await;
    assert_eq!(read(&k, "docs/b.pdf").await["size"], 6);
    assert_eq!(read(&k, "secret.txt").await["reason"], "not_found");
    let tmp2 = tempfile::TempDir::new().unwrap();
    let k2 = bridge(&tmp2, Some(json!(["."]))).await;
    assert_eq!(read(&k2, "secret.txt").await["size"], 8);
}

#[tokio::test]
async fn allow_disallow_and_allowed_edit_the_record_one_path_at_a_time() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, Some(json!([]))).await;
    assert_eq!(
        call(&k, json!({"type": "allow", "path": "docs/a.pdf"})).await,
        json!({"path": "docs/a.pdf", "allowed": true, "count": 1})
    );
    let r = call(&k, json!({"type": "allow", "path": "./docs//a.pdf"})).await;
    assert_eq!(r["count"], 1, "idempotent, normalised: {r}");
    call(&k, json!({"type": "allow", "path": "docs/b.pdf"})).await;
    let rec = k
        .agents
        .get(&AgentId::from("fb"))
        .unwrap()
        .meta
        .read()
        .unwrap()
        .clone();
    assert_eq!(rec["allow"], json!(["docs/a.pdf", "docs/b.pdf"]));
    assert_eq!(read(&k, "docs/b.pdf").await["size"], 6);
    assert_eq!(
        call(&k, json!({"type": "allowed", "limit": 1})).await,
        json!({"paths": ["docs/a.pdf"], "next": "docs/a.pdf", "total": 2})
    );
    let rest = call(&k, json!({"type": "allowed", "after": "docs/a.pdf"})).await;
    assert_eq!(rest["paths"], json!(["docs/b.pdf"]));
    assert_eq!(rest["next"], Value::Null);
    assert_eq!(
        call(&k, json!({"type": "disallow", "path": "docs/b.pdf"})).await["count"],
        1
    );
    assert_eq!(read(&k, "docs/b.pdf").await["reason"], "not_found");
    assert_eq!(
        call(&k, json!({"type": "disallow", "path": "nope"})).await["reason"],
        "not_found"
    );
}

#[tokio::test]
async fn nothing_outside_the_root_can_be_allowed_or_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, Some(json!(["."]))).await;
    for bad in ["../x", "docs/../../x", "/etc/hosts"] {
        let r = call(&k, json!({"type": "allow", "path": bad})).await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
    }
    let r = read(&k, "../../etc/passwd").await;
    assert!(
        r["error"].as_str().unwrap().contains("escape"),
        "the clamp answers first: {r}"
    );
}

#[test]
fn the_list_is_validated_at_write() {
    for bad in [
        json!("docs"),
        json!(["../up"]),
        json!([""]),
        json!(["/abs"]),
    ] {
        let mut m = JsonMap::new();
        m.insert("allow".to_string(), bad.clone());
        assert!(
            FileBundle::default()
                .validate_meta(&m, &JsonMap::new())
                .is_some(),
            "{bad}"
        );
    }
    let mut ok = JsonMap::new();
    ok.insert("allow".to_string(), json!([".", "docs/a.pdf"]));
    assert!(FileBundle::default()
        .validate_meta(&ok, &JsonMap::new())
        .is_none());
}

#[tokio::test]
async fn writes_stay_inside_the_list_and_readonly_still_holds() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, Some(json!(["docs"]))).await;
    let out = call(
        &k,
        json!({"type": "rename", "old_path": "docs/a.pdf", "new_path": "a.pdf"}),
    )
    .await;
    assert_eq!(
        out["reason"], "not_found",
        "the target is outside the list: {out}"
    );
    assert_eq!(
        call(&k, json!({"type": "delete", "path": "secret.txt"})).await["reason"],
        "not_found"
    );
    assert!(served(&tmp).join("secret.txt").exists());
    assert_eq!(
        call(&k, json!({"type": "mkdir", "path": "docs/sub"})).await["created"],
        true
    );
}

async fn call_as(kernel: &Arc<Kernel>, sender: &str, payload: Value) -> Value {
    let k = Arc::clone(kernel);
    fantastic_kernel::send::with_sender(AgentId::from(sender), async move {
        k.send(vec![Item::new(AgentId::from("fb"), payload)])
            .await
            .remove(0)
            .result
    })
    .await
}

/// A handler-less agent under the root holding `can_call` — python
/// `kernel.create(None, id=…, can_call=…)`.
fn bare(kernel: &Arc<Kernel>, id: &str, can_call: Value) {
    let mut meta = JsonMap::new();
    meta.insert("can_call".into(), can_call);
    kernel.register(Agent::new(
        AgentId::from(id),
        None,
        Some(AgentId::from("kernel_state")),
        meta,
        PathBuf::new(),
        true,
    ));
}

/// Python `test_an_agent_granted_allow_publishes_a_file`: the exposure verbs are
/// PRIVILEGED — an agent granted them by name publishes a file; one holding only
/// a `*` on the bridge is refused at the gate.
#[tokio::test]
async fn an_agent_granted_allow_publishes_a_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    site(&tmp);
    let k = mk_kernel_with(
        &tmp,
        json!({"ingress_rule": "allow_all_ingress", "allow": [],
               "callable_by": {"ai": ["allow", "disallow", "allowed"]}}),
    )
    .await;
    bare(&k, "ai", json!({"fb": ["allow", "disallow", "allowed"]}));
    let r = call_as(&k, "ai", json!({"type": "allow", "path": "docs/a.pdf"})).await;
    assert_eq!(r["allowed"], true, "{r}");
    bare(&k, "stranger", json!({"fb": "*"}));
    let r = call_as(&k, "stranger", json!({"type": "allow", "path": "."})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
}

// ─── symlinks: BOTH ends must be exposed (audit 2026-09-28) ──────────────────
// The list was checked on the path AS ASKED while the fs follows links, so a link
// inside an exposed dir served whatever in-root file it pointed at. Python
// `test_allow_list.py`'s symlink twins.

async fn linked(tmp: &tempfile::TempDir, allow: Value) -> Arc<Kernel> {
    let k = bridge(tmp, Some(allow)).await;
    let s = served(tmp);
    std::os::unix::fs::symlink("../secret.txt", s.join("docs/leak")).unwrap();
    std::os::unix::fs::symlink("b.pdf", s.join("docs/sib")).unwrap();
    std::fs::create_dir_all(s.join("shared")).unwrap();
    std::fs::write(s.join("shared/c.txt"), b"c\n").unwrap();
    std::os::unix::fs::symlink("../shared", s.join("docs/shared")).unwrap();
    k
}

#[tokio::test]
async fn a_link_inside_the_list_to_a_file_outside_it_serves_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = linked(&tmp, json!(["docs"])).await;
    let r = read(&k, "docs/leak").await;
    assert_eq!(r["reason"], "not_found", "{r}");
    let names: Vec<Value> = call(&k, json!({"type": "list", "path": "docs"})).await["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].clone())
        .collect();
    assert!(
        !names.contains(&json!("leak")) && !names.contains(&json!("shared")),
        "{names:?}"
    );
    let out = call(
        &k,
        json!({"type": "rename", "old_path": "docs/leak", "new_path": "docs/moved"}),
    )
    .await;
    assert_eq!(out["reason"], "not_found", "{out}");
    assert!(served(&tmp).join("secret.txt").exists());
}

#[tokio::test]
async fn a_link_that_stays_inside_the_list_still_serves() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = linked(&tmp, json!(["docs"])).await;
    assert_eq!(read(&k, "docs/sib").await["size"], json!(6));
}

#[tokio::test]
async fn an_exposed_linked_dir_lists_what_it_serves() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = linked(&tmp, json!(["docs", "shared"])).await;
    let listed = call(&k, json!({"type": "list", "path": "docs/shared"})).await;
    let paths: Vec<Value> = listed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].clone())
        .collect();
    assert_eq!(paths, vec![json!("docs/shared/c.txt")], "{listed}");
    assert_eq!(read(&k, "docs/shared/c.txt").await["size"], json!(2));
}

#[tokio::test]
async fn a_readonly_leg_refuses_a_binary_write_before_judging_the_chunk() {
    // PYTHON'S ORDER on the binary channel too: `readonly` first, whatever the
    // header holds — a header carrying a JSON `bytes` answered `missing_arg`
    // here while python said `agent is readonly` (parity bundle_verbs,
    // readonly_write, 2026-09-30).
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = served(&tmp);
    let kernel = mk_kernel_with(
        &tmp,
        json!({"ingress_rule": "allow_all_ingress", "allow": ["."], "readonly": true}),
    )
    .await;
    for header in [
        json!({"type": "write_stream", "path": "a.txt", "bytes": "x"}),
        json!({"type": "write_stream", "path": "a.txt"}),
    ] {
        let out = kernel
            .send(vec![
                Item::new(AgentId::from("fb"), header.clone()).with_binary(b"data".to_vec())
            ])
            .await
            .remove(0)
            .result;
        assert_eq!(out["error"], "agent is readonly", "{header} → {out}");
        assert_eq!(out["reason"], "unauthorized", "{header} → {out}");
    }
    assert!(!dir.join("a.txt").exists());
}

#[tokio::test]
async fn pump_ends_are_ordinary_executions_in_python_shape() {
    // python's `_pump` does two `kernel.send([...])` per chunk: each end is a
    // ledgered call (pid, call row) — the read a plain `{type, path, offset,
    // length}`, the write `{type, path, bytes, offset, truncate}` with the chunk
    // counted at `bytes` in that position.
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    std::fs::write(served.join("src.txt"), b"hello").unwrap();
    let r = call(
        &kernel,
        json!({"type": "pump", "path": "src.txt", "sink_path": "copy.txt"}),
    )
    .await;
    assert_eq!(r["chunks"], 1, "{r}");
    let calls: Vec<Value> = kernel
        .rows
        .all()
        .into_iter()
        .filter(|row| row.get("kind") == Some(&json!("call")))
        .map(|row| row["payload"].clone())
        .collect();
    let read = calls
        .iter()
        .find(|p| p["type"] == "read_stream")
        .expect("the read is a call row");
    let keys: Vec<&String> = read.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["type", "path", "offset", "length"], "{read}");
    let write = calls
        .iter()
        .find(|p| p["type"] == "write_stream")
        .expect("the write is a call row");
    let keys: Vec<&String> = write.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["type", "path", "bytes", "offset", "truncate"],
        "{write}"
    );
    assert_eq!(write["bytes"], json!({"len": 5}));
}

/// STEP-4 PIN (2026-10-01): the file leg's unknown type, as TEXT — key order is
/// part of the wire (`preserve_order`) and `Value` equality ignores it.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    let r = call(&kernel, json!({"type": "frobnicate"})).await;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"file_bridge: unknown type 'frobnicate'","reason":"unhandled","verb":"frobnicate"}"#
    );
    let r = FileBundle::default()
        .handle(&AgentId::from("fb"), &json!({}), &kernel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"file_bridge: unknown type None","reason":"unhandled","verb":null}"#
    );
    let r = FileBundle::default()
        .handle(&AgentId::from("fb"), &json!({"type": 5}), &kernel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"file_bridge: unknown type 5","reason":"unhandled","verb":5}"#
    );
}

// ─── STEP-4 PINS (2026-10-01): one prologue, two channels ───────────────────
//
// `handle` and `handle_binary` each run the agent lookup → meta snapshot → gate
// → clamp → readonly/exposure prologue. These pin, per refusal, that the BINARY
// channel answers byte-for-byte what the TEXT channel answers (and no body).

async fn both(
    kernel: &Arc<Kernel>,
    header: Value,
    blob: &[u8],
) -> (String, String, Option<Vec<u8>>) {
    let txt = call(kernel, header.clone()).await;
    let bin = kernel
        .send(vec![
            Item::new(AgentId::from("fb"), header).with_binary(blob.to_vec())
        ])
        .await
        .remove(0);
    (
        serde_json::to_string(&txt).unwrap(),
        serde_json::to_string(&bin.result).unwrap(),
        bin.body,
    )
}

#[tokio::test]
async fn a_sealed_leg_denies_both_channels_identically() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({"allow": ["."]})).await;
    for header in [
        json!({"type": "read_stream", "path": "a.txt"}),
        json!({"type": "read", "path": "a.txt"}),
        json!({"type": "list"}),
    ] {
        let (txt, bin, body) = both(&kernel, header.clone(), b"x").await;
        assert_eq!(bin, txt, "{header}");
        assert!(bin.contains(r#""reason":"unauthorized""#), "{bin}");
        assert_eq!(body, None, "{header}");
    }
    // the exact denial, once
    let (_, bin, _) = both(
        &kernel,
        json!({"type": "read_stream", "path": "a.txt"}),
        b"",
    )
    .await;
    assert_eq!(
        bin,
        call(&kernel, json!({"type": "read_stream", "path": "a.txt"}))
            .await
            .to_string()
    );
}

#[tokio::test]
async fn a_path_outside_the_allow_list_is_refused_identically_on_both_channels() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = served(&tmp);
    std::fs::create_dir_all(dir.join("pub")).unwrap();
    std::fs::write(dir.join("secret.txt"), b"s").unwrap();
    let kernel = mk_kernel_with(
        &tmp,
        json!({"ingress_rule": "allow_all_ingress", "allow": ["pub"]}),
    )
    .await;
    let (txt, bin, body) = both(
        &kernel,
        json!({"type": "read_stream", "path": "secret.txt"}),
        b"",
    )
    .await;
    assert_eq!(bin, txt);
    assert_eq!(body, None);
    let v: Value = serde_json::from_str(&bin).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(&keys[..2], ["error", "reason"], "{bin}");
    assert!(v.get("error").is_some() && v["reason"].is_string(), "{bin}");
}

#[tokio::test]
async fn a_readonly_leg_refuses_writes_identically_on_both_channels() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(
        &tmp,
        json!({"ingress_rule": "allow_all_ingress", "allow": ["."], "readonly": true}),
    )
    .await;
    let (txt, bin, body) = both(
        &kernel,
        json!({"type": "write_stream", "path": "a.txt"}),
        b"data",
    )
    .await;
    assert_eq!(bin, txt);
    assert_eq!(
        bin,
        r#"{"error":"agent is readonly","reason":"unauthorized","hint":"this leg accepts reads only; an operator may clear `readonly`"}"#
    );
    assert_eq!(body, None);
    // a TEXT verb on the binary channel (the `<bytes>` document) too
    let (txt, bin, body) = both(
        &kernel,
        json!({"type": "write", "path": "a.txt", "content": "x"}),
        b"x",
    )
    .await;
    assert_eq!(bin, txt, "{bin}");
    assert_eq!(body, None);
}

#[tokio::test]
async fn a_binary_verb_the_leg_does_not_own_takes_the_text_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let (txt, bin, body) = both(&kernel, json!({"type": "frob"}), b"x").await;
    assert_eq!(bin, txt);
    assert_eq!(
        bin,
        r#"{"error":"file_bridge: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#
    );
    assert_eq!(body, None);
    let (txt, bin, body) = both(&kernel, json!({"type": "boot"}), b"x").await;
    assert_eq!((txt.as_str(), bin.as_str(), body), ("null", "null", None));
}

#[tokio::test]
async fn a_leg_that_is_not_registered_says_no_agent_on_both_channels() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let ghost = AgentId::from("ghost_fb");
    let b = FileBundle::default();
    for verb in ["read_stream", "write", "list"] {
        let (bin, body) = b
            .handle_binary(&ghost, json!({"type": verb, "path": "a"}), vec![1], &kernel)
            .await
            .unwrap();
        let txt = b
            .handle(&ghost, &json!({"type": verb, "path": "a"}), &kernel)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_string(&bin).unwrap(),
            serde_json::to_string(&txt).unwrap(),
            "{verb}"
        );
        assert_eq!(body, None, "{verb}");
    }
}
