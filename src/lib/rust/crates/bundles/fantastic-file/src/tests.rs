//! Unit tests for this bundle crate. Every text pinned here is python's.

use super::*;
use crate::test_support::{mode_of, own_uid, set_mode};
use fantastic_kernel::test_support::symlink;
use fantastic_kernel::{Agent, Item};
use serde_json::json;

/// A clamped root, as `handle` hands one to a verb.
/// The agent a reply's `origin` names — any id will do for the pure helpers.
fn fb() -> AgentId {
    AgentId::from("fb")
}

fn at(root: &Path) -> Result<PathBuf, String> {
    fantastic_bundle::realpath(root)
}

#[test]
fn resolve_refuses_escape() {
    let tmp = tempfile::TempDir::new().unwrap();
    let err = fs::resolve(tmp.path(), "../escape").unwrap_err();
    assert!(
        err.starts_with("fs: path '../escape' escapes root"),
        "{err}"
    );
}

#[test]
fn resolve_keeps_subpath() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = fs::resolve(tmp.path(), "sub/inner.txt").unwrap();
    assert!(p.starts_with(fantastic_bundle::realpath(tmp.path()).unwrap()));
    assert!(p.ends_with("sub/inner.txt"));
}

#[test]
fn resolve_refuses_absolute_path() {
    // python's `fs.resolve` refuses ANY leading '/', re-rooting nothing.
    let tmp = tempfile::TempDir::new().unwrap();
    let err = fs::resolve(tmp.path(), "/etc/passwd").unwrap_err();
    assert!(
        err.starts_with("fs: path '/etc/passwd' escapes root"),
        "{err}"
    );
    // The agent layer answers the same shape with `bad_arg` BEFORE reaching fs.
    let r = read_stream_reply(&fb(), &at(tmp.path()), &args(json!({"path": "/etc/passwd"}))).0;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "path");
}

#[test]
fn an_atomic_write_with_a_mode_creates_exactly_those_bits() {
    let tmp = tempfile::TempDir::new().unwrap();
    fs::write_bytes(tmp.path(), "d/secret.yaml", b"k: v\n", true, Some(0o600)).unwrap();
    let p = tmp.path().join("d/secret.yaml");
    assert_eq!(std::fs::read(&p).unwrap(), b"k: v\n");
    assert_eq!(mode_of(&p).unwrap() & 0o777, 0o600);
    let names = |d: &std::path::Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&tmp.path().join("d")), ["secret.yaml"], "no tmp left");
    // a rewrite replaces it, the bits held
    fs::write_bytes(tmp.path(), "d/secret.yaml", b"k: w\n", true, Some(0o600)).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"k: w\n");
    assert_eq!(mode_of(&p).unwrap() & 0o777, 0o600);
    // python: `ValueError("fs: `mode` needs an atomic write")`
    assert_eq!(
        fs::write_bytes(tmp.path(), "x", b"", false, Some(0o600)).unwrap_err(),
        "fs: `mode` needs an atomic write"
    );
}

/// `read_private` opens ONCE (`O_NOFOLLOW`) and judges
/// the open descriptor — regular, this user's, no group/other bits — before
/// reading it, so no swap can land between the check and the read. A symlink in
/// the leaf's place, a widened mode and a FIFO ([`read_private_refuses_a_fifo`])
/// are refused (`NotPrivate`, nothing read); an absent file is python's
/// `FileNotFoundError` text. Python
/// `test_read_private_judges_the_open_descriptor_it_reads`.
#[test]
fn read_private_judges_the_open_descriptor_it_reads() {
    let tmp = tempfile::TempDir::new().unwrap();
    let secret = tmp.path().join("s.yaml");
    std::fs::write(&secret, b"k: v\n").unwrap();
    let chmod = |m: u32| set_mode(&secret, m).unwrap();
    chmod(0o600);
    assert_eq!(fs::read_private(tmp.path(), "s.yaml").unwrap(), b"k: v\n");
    chmod(0o644);
    match fs::read_private(tmp.path(), "s.yaml") {
        Err(fs::PrivateReadError::NotPrivate(np)) => {
            assert_eq!((np.mode, np.uid), (0o644, own_uid()));
        }
        other => panic!("a widened mode is refused: {other:?}"),
    }
    chmod(0o600);
    symlink(&secret, tmp.path().join("link.yaml")).unwrap();
    assert!(matches!(
        fs::read_private(tmp.path(), "link.yaml"),
        Err(fs::PrivateReadError::NotPrivate(_))
    ));
    match fs::read_private(tmp.path(), "absent.yaml") {
        Err(fs::PrivateReadError::Os(e)) => {
            assert!(
                e.starts_with("[Errno 2] No such file or directory: "),
                "{e}"
            );
        }
        other => panic!("an absent file: {other:?}"),
    }
}

/// A FIFO in the leaf's place is judged and refused, never a hang on the open
/// (`O_NONBLOCK`). Unix only: Windows has no FIFO in a directory.
#[cfg(unix)]
#[test]
fn read_private_refuses_a_fifo() {
    let tmp = tempfile::TempDir::new().unwrap();
    sys::mkfifo(&tmp.path().join("pipe")).unwrap();
    assert!(matches!(
        fs::read_private(tmp.path(), "pipe"),
        Err(fs::PrivateReadError::NotPrivate(_))
    ));
}

#[test]
fn resolve_dot_dot_after_descent_is_ok() {
    // a/b/.. resolves to root/a — still inside.
    let tmp = tempfile::TempDir::new().unwrap();
    let p = fs::resolve(tmp.path(), "a/b/../leaf.txt").unwrap();
    assert!(p.starts_with(fantastic_bundle::realpath(tmp.path()).unwrap()));
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
        &args(json!({"path": "blob.bin", "truncate": true})),
        Some(&payload),
        false,
    );
    assert_eq!(w["written"], payload.len());
    assert_eq!(w["size"], payload.len());
    assert!(w.get("error").is_none(), "{w}");
    // SOURCE: read it all back off the binary channel.
    let (meta, body) = read_stream_reply(&fb(), &root, &args(json!({"path": "blob.bin"})));
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
        &args(json!({"path": "big.bin", "truncate": true})),
        Some(&payload),
        false,
    );
    // First 100 bytes.
    let (m1, b1) = read_stream_reply(
        &fb(),
        &root,
        &args(json!({"path": "big.bin", "offset": 0, "length": 100})),
    );
    let b1 = b1.expect("a chunk came back");
    assert_eq!(b1.len(), 100);
    assert_eq!(m1["eof"], false);
    assert_eq!(m1["next_offset"], 100);
    // Resume from next_offset to the end.
    let (m2, b2) = read_stream_reply(
        &fb(),
        &root,
        &args(json!({"path": "big.bin", "offset": 100, "length": 1000})),
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
        &args(json!({"path": "a.bin", "truncate": true})),
        Some(b"AAAA"),
        false,
    );
    let w = write_stream_reply(&root, &args(json!({"path": "a.bin"})), Some(b"BBBB"), false);
    assert_eq!(w["offset"], 4); // appended at end
    assert_eq!(w["size"], 8);
    let (_m, body) = read_stream_reply(&fb(), &root, &args(json!({"path": "a.bin"})));
    assert_eq!(body, Some(b"AAAABBBB".to_vec()));
}

#[test]
fn write_stream_without_bytes_writes_an_empty_chunk() {
    // No body at `bytes` is an empty chunk (a legal write).
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let root = at(&served);
    let w = write_stream_reply(
        &root,
        &args(json!({"path": "e.txt", "truncate": true})),
        Some(b""),
        false,
    );
    assert_eq!(
        w,
        json!({"path": "e.txt", "written": 0, "offset": 0, "size": 0})
    );
    assert!(served.join("e.txt").exists());
}

#[test]
fn root_escape_error_is_python_text() {
    // The running-dir law, in python's words, quoting the CONFIGURED root.
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
    // The clamp resolves symlinks on BOTH sides; a lexical compare used
    // to admit an outward symlinked root.
    let base = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    symlink(outside.path(), base.path().join("out")).unwrap();
    assert_eq!(
        fs::resolve_root("out", base.path()).unwrap_err(),
        "fs: root 'out' escapes the running dir"
    );
}

#[test]
fn tilde_root_is_literal_not_home() {
    // Python's `fs` never expands `~`; the name is a directory.
    let base = tempfile::TempDir::new().unwrap();
    let resolved = fs::resolve_root("~/projects", base.path()).unwrap();
    assert_eq!(
        resolved,
        fantastic_bundle::realpath(base.path())
            .unwrap()
            .join("~/projects")
    );
}

#[test]
fn gate_seals_by_default_and_opens_with_allow_all() {
    let id = AgentId::from("fb");
    // No ingress_rule ⇒ SEALED: every verb but `reflect` denied.
    let bare = Map::new();
    assert!(gate(&bare, &id, "read_stream").is_some());
    assert!(gate(&bare, &id, "write_stream").is_some());
    assert!(gate(&bare, &id, "list").is_some());
    let denied = gate(&bare, &id, "list").unwrap();
    assert_eq!(denied["reason"], "unauthorized");
    // ingress_rule=allow_all_ingress ⇒ OPEN.
    let mut open = Map::new();
    open.insert("ingress_rule".into(), json!("allow_all_ingress"));
    assert!(gate(&open, &id, "read_stream").is_none());
    assert!(gate(&open, &id, "write_stream").is_none());
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
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
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

/// A call to the kernel itself (`update_agent`, …).
async fn call_root(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), payload)])
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
    // a wrongly typed or out-of-range argument is refused by the agent's entry, by name
    for (verb, header) in [
        ("read_stream", json!({"path": "a.txt", "offset": -1})),
        ("read_stream", json!({"path": "a.txt", "length": 0})),
        ("read_stream", json!({"path": "a.txt", "offset": "x"})),
        ("read_stream", json!({"path": "a.txt", "offset": true})),
        ("read_stream", json!({"path": "a.txt", "length": 1.5})),
        ("write_stream", json!({"path": "a.txt", "offset": -5})),
    ] {
        let mut payload = header.clone();
        payload["type"] = json!(verb);
        let r = match <verbs::Call as fantastic_kernel::verbs::Parse>::parse("fb", &payload, &[]) {
            Err(refused) => refused,
            Ok(routed) => panic!("{payload} was accepted: {routed:?}"),
        };
        assert_eq!(r["reason"], "bad_arg", "{header} → {r}");
        assert!(header.get(r["arg"].as_str().unwrap()).is_some(), "{r}");
    }
    // the legal shape still streams
    let (m, b) = read_stream_reply(
        &fb(),
        &root,
        &args(json!({"path": "a.txt", "offset": 1, "length": 1})),
    );
    assert_eq!(b, Some(b"b".to_vec()));
    assert_eq!(m["next_offset"], 2);
}

#[test]
fn read_stream_reply_key_order() {
    // python's key order: path, offset, bytes, next_offset,
    // eof, size, origin (how to get these bytes again — what a caller's memory
    // keeps instead of the body). `bytes` is nulled IN PLACE: python's
    // codec lifts the value onto the wire and leaves a `null` behind, so every
    // reader gets the same dict shape; the chunk is the reply body beside it.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(&served(&tmp));
    std::fs::write(served(&tmp).join("a.txt"), b"abc").unwrap();
    let (m, body) = read_stream_reply(&fb(), &root, &args(json!({"path": "a.txt"})));
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
    assert_eq!(
        m["origin"],
        "fb a.txt bytes 0-3 of 3 — read_stream {path: a.txt, offset: 0, length: 3}"
    );
}

#[test]
fn huge_length_reads_only_what_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = at(&served(&tmp));
    std::fs::write(served(&tmp).join("a.txt"), b"abc").unwrap();
    // 1 PB must never be `vec![0u8; length]` — an allocation abort of the kernel.
    let (m, b) = read_stream_reply(
        &fb(),
        &root,
        &args(json!({"path": "a.txt", "length": 1_000_000_000_000_000u64})),
    );
    assert_eq!(b, Some(b"abc".to_vec()), "{m}");
    assert_eq!(m["eof"], true);
    let (m, b) = read_stream_reply(&fb(), &root, &args(json!({"path": "a.txt", "length": i64::MAX})));
    assert_eq!(b, Some(b"abc".to_vec()), "{m}");
}

#[test]
fn an_in_root_symlink_pointing_outside_refuses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
    symlink(&outside, root.join("docs")).unwrap();
    let r = at(&root);
    // The lexical form is in-root; the canonical form is not.
    let (m, b) = read_stream_reply(&fb(), &r, &args(json!({"path": "docs/secret.txt"})));
    assert_eq!(m["reason"], "transport_error", "{m}");
    assert!(
        m["error"]
            .as_str()
            .unwrap()
            .starts_with("fs: path 'docs/secret.txt' escapes root"),
        "{m}"
    );
    assert_eq!(b, None);
    let l = list_reply(&fb(), &r, &args(json!({"path": "docs"})));
    assert_eq!(l["reason"], "transport_error", "{l}");
    let w = write_stream_reply(&r, &args(json!({"path": "docs/new.txt"})), Some(b"x"), false);
    assert_eq!(w["reason"], "transport_error", "{w}");
    assert!(!outside.join("new.txt").exists());
    // An in-root symlink still serves.
    std::fs::write(root.join("real.txt"), b"r").unwrap();
    symlink(root.join("real.txt"), root.join("alias.txt")).unwrap();
    let (m, b) = read_stream_reply(&fb(), &r, &args(json!({"path": "alias.txt"})));
    assert!(m.get("error").is_none(), "{m}");
    assert_eq!(b, Some(b"r".to_vec()));
}

#[test]
fn dangling_symlink_pointing_outside_root_refuses() {
    // `realpath(strict=False)` follows a dangling link to where it
    // POINTS; outside the root, that is an escape, and nothing is created
    // through it.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    symlink(outside.join("gone.txt"), root.join("dangling")).unwrap();
    let r = at(&root);
    let w = write_stream_reply(&r, &args(json!({"path": "dangling"})), Some(b"x"), false);
    assert_eq!(w["reason"], "transport_error", "{w}");
    assert!(
        w["error"]
            .as_str()
            .unwrap()
            .starts_with("fs: path 'dangling' escapes root"),
        "{w}"
    );
    assert!(!outside.join("gone.txt").exists());
    let (m, _) = read_stream_reply(&fb(), &r, &args(json!({"path": "dangling"})));
    assert!(
        m["error"]
            .as_str()
            .unwrap()
            .starts_with("fs: path 'dangling' escapes root"),
        "{m}"
    );
    // A dangling link pointing INSIDE the root is still a deletable name.
    symlink(root.join("gone.txt"), root.join("inside")).unwrap();
    let d = delete_reply(&r, &args(json!({"path": "inside"})), false);
    assert_eq!(d, json!({"path": "inside", "deleted": true}), "{d}");
}

#[test]
fn delete_on_symlink_removes_the_link_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/keep.txt"), b"k").unwrap();
    symlink(root.join("data"), root.join("link")).unwrap();
    let r = at(&root);
    let reply = delete_reply(&r, &args(json!({"path": "link", "recursive": true})), false);
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
    let reply = delete_reply(&r, &args(json!({"path": "src"})), false);
    assert_eq!(reply["reason"], "transport_error", "{reply}");
    assert!(root.join("src/a.txt").exists());
    let reply = delete_reply(&r, &args(json!({"path": "src", "recursive": true})), false);
    assert_eq!(reply, json!({"path": "src", "deleted": true}));
    assert!(!root.join("src").exists());
    // an EMPTY dir needs no flag
    std::fs::create_dir_all(root.join("empty")).unwrap();
    assert_eq!(
        delete_reply(&r, &args(json!({"path": "empty"})), false)["deleted"],
        true
    );
}

#[test]
fn os_error_text_is_python_oserror_str() {
    // Every OS failure renders as python's `OSError.__str__` — the path as
    // python's repr (a Windows `\\` doubled).
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.txt"), b"x").unwrap();
    std::fs::write(root.join("f.txt"), b"x").unwrap();
    let r = at(&root);
    let full = fantastic_bundle::realpath(&root).unwrap();
    let reply = delete_reply(&r, &args(json!({"path": "src"})), false);
    // ENOTEMPTY's NUMBER is the OS's (66 on macOS, 39 on Linux, 41 on Windows'
    // C runtime): read it off a real failure, the text is python's on every OS.
    let probe = tmp.path().join("probe_not_empty");
    std::fs::create_dir_all(probe.join("x")).unwrap();
    let not_empty = fantastic_bundle::errno_of(&std::fs::remove_dir(&probe).unwrap_err()).unwrap();
    assert_eq!(
        reply["error"],
        json!(format!(
            "[Errno {not_empty}] Directory not empty: {}",
            fantastic_bundle::py_repr(&full.join("src").to_string_lossy())
        )),
        "{reply}"
    );
    // mkdir THROUGH a file: python names the path whose `mkdir` failed. POSIX
    // fails `f.txt/x` itself (ENOTDIR); Windows says ERROR_PATH_NOT_FOUND
    // (ENOENT), so pathlib's `parents` walk makes the parent and that is the
    // FileExistsError.
    let reply = mkdir_reply(&r, &args(json!({"path": "f.txt/x"})), false);
    let through = if cfg!(windows) {
        format!(
            "[Errno 17] File exists: {}",
            fantastic_bundle::py_repr(&full.join("f.txt").to_string_lossy())
        )
    } else {
        format!(
            "[Errno 20] Not a directory: {}",
            fantastic_bundle::py_repr(&full.join("f.txt").join("x").to_string_lossy())
        )
    };
    assert_eq!(reply["error"], json!(through), "{reply}");
    // mkdir over an existing FILE keeps python's FileExistsError.
    let reply = mkdir_reply(&r, &args(json!({"path": "f.txt"})), false);
    assert_eq!(
        reply["error"],
        json!(format!(
            "[Errno 17] File exists: {}",
            fantastic_bundle::py_repr(&full.join("f.txt").to_string_lossy())
        )),
        "{reply}"
    );
}

/// The refusal the agent's entry gives a call (`Err`), or a panic when it passes.
fn entry_refuses(payload: &Value) -> Value {
    match <verbs::Call as fantastic_kernel::verbs::Parse>::parse("fb", payload, &[]) {
        Err(refused) => refused,
        Ok(routed) => panic!("{payload} was accepted: {routed:?}"),
    }
}

#[test]
fn empty_or_absent_path_delete_rename_mkdir_are_refused_by_the_entry() {
    for payload in [
        json!({"type": "delete", "path": "", "recursive": true}),
        json!({"type": "delete", "recursive": true}),
        json!({"type": "rename", "old_path": "", "new_path": "x"}),
        json!({"type": "rename", "old_path": "a", "new_path": ""}),
        json!({"type": "mkdir", "path": ""}),
        json!({"type": "mkdir"}),
    ] {
        let r = entry_refuses(&payload);
        assert_eq!(r["reason"], "missing_arg", "{payload} → {r}");
    }
}

#[test]
fn the_root_itself_is_no_operand_of_delete_rename_mkdir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("keep.txt"), b"k").unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    let r = at(&root);
    for path in [".", "a/.."] {
        let reply = delete_reply(&r, &args(json!({"path": path, "recursive": true})), false);
        assert_eq!(reply["reason"], "bad_arg", "{path} → {reply}");
        assert_eq!(reply["arg"], "path");
        let reply = mkdir_reply(&r, &args(json!({"path": path})), false);
        assert_eq!(reply["reason"], "bad_arg", "{path} → {reply}");
    }
    for (payload, arg) in [
        (json!({"old_path": ".", "new_path": "x"}), "old_path"),
        (json!({"old_path": "a", "new_path": "a/.."}), "new_path"),
    ] {
        let reply = rename_reply(&r, &args(payload.clone()), false);
        assert_eq!(reply["reason"], "bad_arg", "{payload} → {reply}");
        assert_eq!(reply["arg"], arg);
    }
    assert!(root.join("keep.txt").exists() && root.join("a").is_dir());
}

#[test]
fn absolute_path_is_refused_with_bad_arg() {
    for payload in [
        json!({"type": "read_stream", "path": "/README.md"}),
        json!({"type": "write_stream", "path": "/x"}),
        json!({"type": "list", "path": "/"}),
        json!({"type": "delete", "path": "/README.md"}),
        json!({"type": "mkdir", "path": "/x"}),
        json!({"type": "rename", "old_path": "/README.md", "new_path": "y"}),
        json!({"type": "open_view", "kind": "file", "path": "/etc"}),
    ] {
        let reply = entry_refuses(&payload);
        assert_eq!(reply["reason"], "bad_arg", "{payload} → {reply}");
    }
}

#[test]
fn refusal_texts_use_python_repr() {
    // `{x!r}` on every refusal that names a path or a verb.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::write(root.join("f.txt"), b"f").unwrap();
    let r = at(&root);
    let reply = list_reply(&fb(), &r, &args(json!({"path": "nope"})));
    assert_eq!(reply["error"], "path 'nope' does not exist");
    assert_eq!(reply["reason"], "not_found");
    assert_eq!(reply["path"], "nope");
    let reply = list_reply(&fb(), &r, &args(json!({"path": "f.txt"})));
    assert_eq!(
        reply["error"],
        "list: path must be a directory; 'f.txt' is not one"
    );
    assert_eq!(reply["reason"], "bad_arg");
    let reply = rename_reply(&r, &args(json!({"old_path": "nope", "new_path": "x"})), false);
    assert_eq!(reply["error"], "path 'nope' does not exist");
    assert_eq!(reply["reason"], "not_found");
    let reply = delete_reply(&r, &args(json!({"path": "nope"})), false);
    assert_eq!(reply["error"], "path 'nope' does not exist");
    let (reply, _) = read_stream_reply(&fb(), &r, &args(json!({"path": "nope"})));
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .starts_with("file 'nope' not found"),
        "{reply}"
    );
    assert_eq!(reply["reason"], "not_found");
    // a path holding a quote quotes python's way
    let reply = list_reply(&fb(), &r, &args(json!({"path": "it's"})));
    assert_eq!(reply["error"], "path \"it's\" does not exist");
}

#[test]
fn list_paths_are_relative_to_resolved_root() {
    // The entry path is relative to the RESOLVED root, and a stat that
    // fails reports -1.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    std::fs::create_dir_all(root.join("x")).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("x/a.txt"), b"ab").unwrap();
    symlink(root.join("x/gone.txt"), root.join("x/dangling")).unwrap();
    let r = at(&root);
    let reply = list_reply(&fb(), &r, &args(json!({"path": "sub/../x"})));
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
    let reply = list_reply(&fb(), &r, &args(json!({"path": "."})));
    let names: Vec<&str> = reply["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sub", "x"]);
}

#[test]
fn truncate_and_recursive_are_booleans() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = served(&tmp);
    let r = at(&root);
    write_stream_reply(
        &root_ok(&root),
        &args(json!({"path": "t.txt"})),
        Some(b"AAAA"),
        false,
    );
    // `truncate: true` starts the file over
    let w = write_stream_reply(
        &r,
        &args(json!({"path": "t.txt", "truncate": true})),
        Some(b"B"),
        false,
    );
    assert_eq!(w["size"], 1, "{w}");
    // `false` appends
    let w = write_stream_reply(
        &r,
        &args(json!({"path": "t.txt", "truncate": false})),
        Some(b"C"),
        false,
    );
    assert_eq!(w["size"], 2, "{w}");
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/x"), b"x").unwrap();
    let reply = delete_reply(&r, &args(json!({"path": "d", "recursive": false})), false);
    assert_eq!(reply["reason"], "transport_error", "{reply}");
    let reply = delete_reply(&r, &args(json!({"path": "d", "recursive": true})), false);
    assert_eq!(reply["deleted"], true, "{reply}");
    // anything but a boolean is refused by the entry, by name
    for (verb, extra, arg) in [
        ("write_stream", json!({"path": "t.txt", "truncate": "yes"}), "truncate"),
        ("write_stream", json!({"path": "t.txt", "truncate": 0}), "truncate"),
        ("delete", json!({"path": "d", "recursive": []}), "recursive"),
        ("delete", json!({"path": "d", "recursive": "y"}), "recursive"),
    ] {
        let mut payload = extra.clone();
        payload["type"] = json!(verb);
        let refused = <verbs::Call as fantastic_kernel::verbs::Parse>::parse("fb", &payload, &[])
            .expect_err("refused");
        assert_eq!(refused["reason"], "bad_arg", "{payload}: {refused}");
        assert_eq!(refused["arg"], arg, "{payload}: {refused}");
    }
}

fn root_ok(p: &Path) -> Result<PathBuf, String> {
    at(p)
}

#[test]
fn auth_stem_is_not_judged_as_an_ingress_rule() {
    // Python judges only a PRESENT `ingress_rule`.
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
    // Python's key order, docstring verbs and the
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
        .take(10)
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
            "ingress_rule",
            "egress_rule",
            "sealed",
        ]
    );
    assert_eq!(
        r["root"],
        json!(fantastic_bundle::realpath(&served)
            .unwrap()
            .display()
            .to_string())
    );
    assert_eq!(r["ingress_rule"], "allow_all_ingress");
    assert_eq!(r["egress_rule"], "silent_egress");
    assert_eq!(r["sealed"], false);
    assert_eq!(
        r["sentence"], crate::MAN
    );
}

#[test]
fn verbs_are_declared_in_order() {
    use fantastic_kernel::bundle::Handler;
    let names: Vec<&str> = FileBundle::default()
        .verbs()
        .iter()
        .map(|v| v.name)
        .collect();
    assert_eq!(
        names,
        [
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
            "watch_path",
            "unwatch_path",
            "open_view",
        ]
    );
}

#[tokio::test]
async fn reflect_root_is_resolved_absolute_path() {
    // An escaping root is shown as CONFIGURED, with the violation.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("root".into(), json!("../nope"));
    let k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    let r = reflect_reply(
        &AgentId::from("fb"),
        &meta,
        &clamp_root(&meta, &k, Path::new("")),
    );
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
        &clamp_root(&Map::new(), &k, Path::new("")),
    );
    assert_eq!(
        r["root"],
        json!(fantastic_bundle::realpath(tmp.path())
            .unwrap()
            .display()
            .to_string())
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
    // The verb lookup runs BEFORE the gate, so a sealed bridge still says
    // "unknown verb" instead of "unauthorized".
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    let r = call(&kernel, json!({"type": "frobnicate"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "frobnicate", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("unknown verb 'frobnicate'; fb answers: list, read, write,"),
        "{r}"
    );
}

#[tokio::test]
async fn a_sealed_leg_denies_every_verb_but_reflect() {
    // The gate exemption covers `reflect` only.
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    let r = call(&kernel, json!({"type": "list"})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    // reflect still answers on a sealed leg.
    let r = call(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(r["sealed"], true, "{r}");
}

#[tokio::test]
async fn readonly_wins_over_root_escape() {
    // The check order is arguments (the entry) → readonly → root clamp → path clamp.
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(
        &tmp,
        json!({"ingress_rule": "allow_all_ingress", "readonly": true, "root": "../outside"}),
    )
    .await;
    for payload in [
        json!({"type": "delete", "path": "a"}),
        json!({"type": "rename", "old_path": "a", "new_path": "b"}),
        json!({"type": "mkdir", "path": "a"}),
        json!({"type": "write_stream", "path": "a"}),
    ] {
        let verb = payload["type"].clone();
        let r = call(&kernel, payload).await;
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
    assert_eq!(r["arg"], "path", "{r}");
}

#[tokio::test]
async fn text_write_stream_with_a_json_bytes_is_an_unknown_argument() {
    // `bytes` IS RAW BYTES, and JSON has none: `write_stream` declares no `bytes`
    // argument, so any JSON value there is refused by name and NOTHING IS WRITTEN. A
    // model's `<bytes>` document still works because ai_core lifts it onto the item's
    // binary channel, which is the bytes-typed carrier — the case below.
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    for doc in [json!("line one\n"), json!(76), json!(null), json!(["a"])] {
        let m = call(
            &kernel,
            json!({"type": "write_stream", "path": "a.txt", "truncate": true, "bytes": doc}),
        )
        .await;
        assert_eq!(m["reason"], "bad_arg", "{doc} → {m}");
        assert_eq!(m["arg"], "bytes", "{doc} → {m}");
        assert_eq!(m["error"], "write_stream: unknown argument 'bytes'");
        assert!(!dir.join("a.txt").exists(), "{doc} wrote anyway");
    }
    // THE RAW CHUNK, on the binary channel: this is the path a model's
    // `<bytes>` document takes, and it writes.
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("fb"),
            json!({"type": "write_stream", "path": "a.txt", "truncate": true}),
        )
        .with_binary_key("bytes", b"line one\n".to_vec())])
        .await
        .remove(0)
        .result;
    assert_eq!(out["written"], 9, "{out}");
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"line one\n");

    // …and it comes back as a CHUNK, in the reply body, with the reply's own
    // `bytes` key nulled in place.
    let back = kernel
        .send(vec![Item::new(
            AgentId::from("fb"),
            json!({"type": "read_stream", "path": "a.txt", "offset": 0}),
        )
        .with_binary_key("bytes", Vec::new())])
        .await
        .remove(0);
    assert_eq!(back.result["bytes"], Value::Null, "{}", back.result);
    assert_eq!(back.result["eof"], true, "{}", back.result);
    assert_eq!(
        back.body,
        Some(fantastic_kernel::Blob::key("bytes", b"line one\n".to_vec())),
        "the chunk sits at the reply's `bytes`"
    );
}

#[tokio::test]
async fn pump_ends_default_to_this_bridge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    std::fs::write(served.join("src.txt"), b"hello pump").unwrap();
    // A chunk floor of 1, and an empty end is no end: the entry refuses both.
    let r = call(
        &kernel,
        json!({"type": "pump", "source_path": "src.txt", "sink_path": "copy.txt", "chunk": 0}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "a chunk floor of 1: {r}");
    let r = call(
        &kernel,
        json!({"type": "pump", "source": "", "source_path": "src.txt", "sink_path": "copy.txt"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "an empty end: {r}");
    // `sink_path` left out is `source_path`: a pump onto its own source.
    let r = call(&kernel, json!({"type": "pump", "source_path": "src.txt"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    // A real copy, chunk by chunk, to another path.
    let r = call(
        &kernel,
        json!({"type": "pump", "source_path": "src.txt", "sink_path": "copy.txt", "chunk": 4}),
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
    // A failed read names the SOURCE with python's repr and prints the reply as
    // python prints a dict.
    let r = call(
        &kernel,
        json!({"type": "pump", "source": "nobody", "source_path": "src.txt"}),
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

/// Python `test_a_pump_onto_its_own_source_is_refused`. With only
/// `source_path`, sink and sink_path default to the source, and the first write's
/// `truncate` cut the file the next read comes from — 200 KiB became 64 KiB under
/// a success reply. Refused, the path judged normalised (`./x` is `x`); the file
/// is untouched.
#[tokio::test]
async fn a_pump_onto_its_own_source_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let data: Vec<u8> = (0..800).flat_map(|_| 0u8..=255).collect();
    std::fs::write(served.join("self.bin"), &data).unwrap();
    for extra in [
        json!({}),
        json!({"sink_path": "self.bin"}),
        json!({"sink_path": "./self.bin"}),
        json!({"sink": "fb", "sink_path": "sub/../self.bin"}),
    ] {
        let mut p = json!({"type": "pump", "source_path": "self.bin"});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        let out = call(&kernel, p).await;
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            "{\"error\":\"pump: sink_path must be another file than source_path when the \
             sink is the source — a pump onto its own source truncates it after the first \
             chunk\",\"reason\":\"bad_arg\",\"arg\":\"sink_path\"}",
            "{extra}"
        );
    }
    assert_eq!(std::fs::read(served.join("self.bin")).unwrap(), data);
}

/// `path` is no alias for `source_path`: the verb takes only `source_path`, so a
/// `path` is an unknown argument, refused by name before any end runs.
#[tokio::test]
async fn pump_takes_source_path_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let served = served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    std::fs::write(served.join("a.txt"), b"hello").unwrap();
    let out = call(
        &kernel,
        json!({"type": "pump", "path": "a.txt", "sink_path": "b.txt"}),
    )
    .await;
    assert_eq!(out["reason"], "bad_arg", "{out}");
    assert_eq!(out["arg"], "path", "{out}");
    assert!(!served.join("b.txt").exists());
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
async fn nothing_outside_the_root_can_be_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, Some(json!(["."]))).await;
    let r = read(&k, "../../etc/passwd").await;
    assert!(
        r["error"].as_str().unwrap().contains("escape"),
        "the clamp answers first: {r}"
    );
}

#[tokio::test]
async fn the_list_is_validated_at_write() {
    // the declared record states the whole rule: a list of non-empty paths relative to the root
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    for bad in [json!(["../up"]), json!([""]), json!(["/abs"]), json!("docs"), json!([1])] {
        let r = call_root(
            &kernel,
            json!({"type": "update_agent", "id": "fb", "allow": bad}),
        )
        .await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "allow", "{bad}: {r}");
    }
    let r = call_root(
        &kernel,
        json!({"type": "update_agent", "id": "fb", "allow": [".", "docs/a.pdf", "a..b/"]}),
    )
    .await;
    assert!(r.get("reason").is_none(), "{r}");
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

// ─── symlinks: BOTH ends must be exposed ─────────────────────────────────────
// The fs follows links, so checking the list on the path AS ASKED alone would
// let a link inside an exposed dir serve whatever in-root file it points at.
// Python `test_allow_list.py`'s symlink twins.

async fn linked(tmp: &tempfile::TempDir, allow: Value) -> Arc<Kernel> {
    let k = bridge(tmp, Some(allow)).await;
    let s = served(tmp);
    symlink("../secret.txt", s.join("docs/leak")).unwrap();
    symlink("b.pdf", s.join("docs/sib")).unwrap();
    std::fs::create_dir_all(s.join("shared")).unwrap();
    std::fs::write(s.join("shared/c.txt"), b"c\n").unwrap();
    symlink("../shared", s.join("docs/shared")).unwrap();
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
    // header holds — a header carrying a JSON `bytes` answers `agent is readonly`
    // as python does, never `missing_arg`.
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
            .send(vec![Item::new(AgentId::from("fb"), header.clone())
                .with_binary_key("bytes", b"data".to_vec())])
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
        json!({"type": "pump", "source_path": "src.txt", "sink_path": "copy.txt"}),
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
        ["type", "path", "offset", "truncate", "bytes"],
        "{write}"
    );
    assert_eq!(write["bytes"], json!({"len": 5}));
}

/// The file leg's unknown verb, as TEXT — key order is part of the wire
/// (`preserve_order`) and `Value` equality ignores it.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel_with(&tmp, json!({})).await;
    let r = call(&kernel, json!({"type": "frobnicate"})).await;
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["error", "reason", "hint", "verb"], "{r}");
    assert_eq!(r["verb"], "frobnicate");
    let r = call(&kernel, json!({"type": "it's"})).await;
    assert_eq!(r["verb"], "it's");
    assert!(
        r["error"].as_str().unwrap().starts_with("unknown verb \"it's\"; "),
        "{r}"
    );
}

// ─── one prologue, two channels ─────────────────────────────────────────────
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
            Item::new(AgentId::from("fb"), header).with_binary_key("bytes", blob.to_vec())
        ])
        .await
        .remove(0);
    (
        serde_json::to_string(&txt).unwrap(),
        serde_json::to_string(&bin.result).unwrap(),
        bin.body.map(|b| b.bytes),
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
        json!({"type": "write", "path": "a.txt"}),
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
    let v: Value = serde_json::from_str(&bin).unwrap();
    assert_eq!((v["reason"].as_str(), v["verb"].as_str()), (Some("unhandled"), Some("frob")));
    assert_eq!(body, None);
}

#[tokio::test]
async fn a_leg_that_is_not_registered_says_no_agent_on_both_channels() {
    let tmp = tempfile::TempDir::new().unwrap();
    served(&tmp);
    let kernel = mk_kernel(&tmp).await;
    let ghost = AgentId::from("ghost_fb");
    let b = FileBundle::default();
    for verb in ["read_stream", "write", "list"] {
        let call = || {
            let routed = <verbs::Call as fantastic_kernel::verbs::Parse>::parse(
                "ghost_fb",
                &json!({"type": verb, "path": "a"}),
                &[],
            )
            .expect("a call");
            match routed {
                fantastic_kernel::verbs::Routed::Call(c) => c,
                other => panic!("{other:?}"),
            }
        };
        let (bin, body) = b
            .handle_binary(
                &ghost,
                call(),
                Some(fantastic_kernel::Blob::key("bytes", vec![1])),
                &kernel,
            )
            .await
            .unwrap();
        let txt = b.handle(&ghost, call(), &kernel).await.unwrap();
        assert_eq!(
            serde_json::to_string(&bin).unwrap(),
            serde_json::to_string(&txt).unwrap(),
            "{verb}"
        );
        assert_eq!(body, None, "{verb}");
    }
}

#[test]
fn an_atomic_write_never_follows_a_planted_tmp_link() {
    // A symlink at `<name>.tmp` pointing outside the root is never written
    // through: each write stages in a fresh, exclusively created tmp.
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"keep").unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    symlink(&outside, root.join("f.txt.tmp")).unwrap();
    for mode in [None, Some(0o600)] {
        fs::write_bytes(&root, "f.txt", b"new", true, mode).unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"new");
    }
}
