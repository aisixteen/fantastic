//! file_bridge's TEXT verbs — `read` / `write` / `edit` / `append` / `undo`.
//! Ported from python `file_bridge/tests/test_text_verbs.py`.
//!
//! A caller edits with its own grants on the bridge, so there is no deputy. The
//! posture cases (readonly, sealed) are answered by the bridge's own gate.
//! `undo` is one RAM slot per path — a second undo is redo.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::Map as JsonMap;

/// A kernel with ONE file_bridge `fb` rooted at `tmp/served` (running dir =
/// `tmp`), open, exposing everything — `extra` overrides (a `null` removes).
async fn mk(tmp: &tempfile::TempDir, extra: Value) -> Arc<Kernel> {
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
        "allow": ["."], "ingress_rule": "allow_all_ingress",
    });
    for (k, v) in extra.as_object().expect("extra is an object") {
        if v.is_null() {
            create.as_object_mut().unwrap().remove(k);
        } else {
            create[k] = v.clone();
        }
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "fb", "{r}");
    kernel
}

async fn call(k: &Arc<Kernel>, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from("fb"), payload)])
        .await
        .remove(0)
        .result
}

/// On the BINARY channel: the blob IS the `bytes` argument.
async fn call_bin(k: &Arc<Kernel>, payload: Value, blob: &[u8]) -> Value {
    k.send(vec![
        Item::new(AgentId::from("fb"), payload).with_binary_key("bytes", blob.to_vec())
    ])
    .await
    .remove(0)
    .result
}

fn block(old: &str, new: &str) -> String {
    format!("<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE")
}

async fn seed(k: &Arc<Kernel>, path: &str, text: &str) -> Value {
    call(k, json!({"type": "write", "path": path, "bytes": text})).await
}

/// One-file edit through the TRANSACTION shape: even a single file rides the
/// `edits` array.
async fn patch(k: &Arc<Kernel>, path: &str, blob: Option<&str>, extra: Value) -> Value {
    let hunks = blob.map_or(1, |b| b.matches("<<<<<<< SEARCH").count().max(1));
    let mut edit = json!({"path": path, "hunks": hunks});
    if let Some(m) = extra.get("if_match") {
        edit["if_match"] = m.clone();
    }
    let mut payload = json!({"type": "edit", "edits": [edit]});
    if let Some(d) = extra.get("dry_run") {
        payload["dry_run"] = d.clone();
    }
    if let Some(b) = blob {
        payload["bytes"] = json!(b);
    }
    call(k, payload).await
}

fn sd(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("served")
}

fn text_of(tmp: &tempfile::TempDir, name: &str) -> String {
    std::fs::read_to_string(sd(tmp).join(name)).unwrap()
}

fn put(tmp: &tempfile::TempDir, name: &str, text: &str) {
    std::fs::create_dir_all(sd(tmp)).unwrap();
    std::fs::write(sd(tmp).join(name), text).unwrap();
}

// ─── edit: happy paths ──────────────────────────────────────────

#[tokio::test]
async fn single_hunk_applies_and_writes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\nb = 2\nc = 3\n").await;
    let r = patch(&k, "m.py", Some(&block("b = 2", "b = 20")), json!({})).await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(r["edits"][0]["hunks"], 1);
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\nb = 20\nc = 3\n");
}

#[tokio::test]
async fn multiple_hunks_apply_in_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "x = 1\ny = 2\nz = 3\n").await;
    let p = block("x = 1", "x = 11") + "\n" + &block("z = 3", "z = 33");
    let r = patch(&k, "m.py", Some(&p), json!({})).await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(r["edits"][0]["hunks"], 2);
    assert_eq!(text_of(&tmp, "m.py"), "x = 11\ny = 2\nz = 33\n");
}

#[tokio::test]
async fn hunk_matches_content_a_prior_hunk_created() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.txt", "one\n").await;
    let p = block("one", "two") + "\n" + &block("two", "three");
    let r = patch(&k, "m.txt", Some(&p), json!({})).await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(text_of(&tmp, "m.txt"), "three\n");
}

#[tokio::test]
async fn hunk_with_quotes_backslashes_newlines_is_verbatim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "p.py", "print(\"hi\")\npath = \"c:\\\\tmp\"\n").await;
    let r = patch(
        &k,
        "p.py",
        Some(&block("print(\"hi\")", "print(\"bye\", end=\"\")")),
        json!({}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(
        text_of(&tmp, "p.py"),
        "print(\"bye\", end=\"\")\npath = \"c:\\\\tmp\"\n"
    );
}

#[tokio::test]
async fn unicode_content_and_hunk() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "u.txt", "café — over\nnext\n").await;
    let r = patch(
        &k,
        "u.txt",
        Some(&block("café — over", "tea — done")),
        json!({}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(text_of(&tmp, "u.txt"), "tea — done\nnext\n");
}

#[tokio::test]
async fn multiline_hunk() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(
        &k,
        "m.py",
        "def f():\n    return 1\n\ndef g():\n    return 2\n",
    )
    .await;
    let r = patch(
        &k,
        "m.py",
        Some(&block("def f():\n    return 1", "def f():\n    return 99")),
        json!({}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(
        text_of(&tmp, "m.py"),
        "def f():\n    return 99\n\ndef g():\n    return 2\n"
    );
}

// ─── edit: fail-loud, all-or-nothing ────────────────────────────

#[tokio::test]
async fn no_match_fails_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let r = patch(&k, "m.py", Some(&block("nope = 0", "x = 0")), json!({})).await;
    assert!(r.get("edited").is_none(), "{r}");
    assert_eq!(r["reason"], "no_match");
    assert_eq!(r["failed"][0]["index"], 0);
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

#[tokio::test]
async fn ambiguous_match_fails_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "v = 1\nv = 1\n").await;
    let r = patch(&k, "m.py", Some(&block("v = 1", "v = 2")), json!({})).await;
    assert_eq!(r["reason"], "ambiguous", "{r}");
    assert!(r["failed"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("2 places"));
    assert_eq!(text_of(&tmp, "m.py"), "v = 1\nv = 1\n");
}

#[tokio::test]
async fn second_hunk_failure_reverts_the_whole_edit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\nb = 2\n").await;
    let p = block("a = 1", "a = 10") + "\n" + &block("missing", "x");
    let r = patch(&k, "m.py", Some(&p), json!({})).await;
    assert!(r.get("edited").is_none(), "{r}");
    assert_eq!(r["failed"][0]["index"], 1);
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\nb = 2\n");
}

// ─── edit: bad input ────────────────────────────────────────────

#[tokio::test]
async fn malformed_patch_unterminated_block() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let r = patch(
        &k,
        "m.py",
        Some("<<<<<<< SEARCH\na = 1\n=======\na = 2\n"),
        json!({}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

#[tokio::test]
async fn empty_patch_no_blocks() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let r = patch(&k, "m.py", Some("just some prose, no markers"), json!({})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
}

#[tokio::test]
async fn missing_bytes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let r = patch(&k, "m.py", None, json!({})).await;
    assert!(r.get("edited").is_none(), "{r}");
    assert!(r["error"].as_str().unwrap().contains("bytes"), "{r}");
}

#[tokio::test]
async fn file_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r = patch(&k, "nope.py", Some(&block("a", "b")), json!({})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
}

#[tokio::test]
async fn missing_edits() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r = call(&k, json!({"type": "edit", "bytes": block("a", "b")})).await;
    assert!(r.get("edited").is_none(), "{r}");
    assert!(r["error"].as_str().unwrap().contains("edits"), "{r}");
}

#[tokio::test]
async fn bad_utf8_patch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let r = call_bin(
        &k,
        json!({"type": "edit", "edits": [{"path": "m.py", "hunks": 1}]}),
        b"\xff\xfe\xff",
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

#[tokio::test]
async fn malformed_missing_divider() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let r = patch(
        &k,
        "m.py",
        Some("<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE"),
        json!({}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

// ─── the leg's own posture ──────────────────────────────────────

#[tokio::test]
async fn readonly_leg_refuses_the_edit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({"readonly": true})).await;
    put(&tmp, "m.py", "a = 1\n");
    let r = patch(&k, "m.py", Some(&block("a = 1", "a = 2")), json!({})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

#[tokio::test]
async fn sealed_leg_denies_everything() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({"ingress_rule": null})).await;
    put(&tmp, "m.py", "a = 1\n");
    let r = patch(&k, "m.py", Some(&block("a = 1", "a = 2")), json!({})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

// ─── read / write / append / version ────────────────────────────

#[tokio::test]
async fn write_then_read_lines() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let w = seed(&k, "doc.md", "alpha\nbeta\ngamma\n").await;
    assert_eq!(w["size"], 17, "{w}");
    assert_eq!(w["version"].as_str().unwrap().len(), 16);
    let r = call(&k, json!({"type": "read", "path": "doc.md"})).await;
    let texts: Vec<&str> = r["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["alpha", "beta", "gamma"]);
    let ns: Vec<u64> = r["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["n"].as_u64().unwrap())
        .collect();
    assert_eq!(ns, [1, 2, 3]);
    assert_eq!(r["total"], 3);
    assert_eq!(r["eof"], true);
    assert_eq!(r["version"], w["version"]);
}

#[tokio::test]
async fn read_pages_by_line_offset() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let body: String = (0..10).map(|i| format!("L{i}\n")).collect();
    seed(&k, "n.txt", &body).await;
    let r = call(
        &k,
        json!({"type": "read", "path": "n.txt", "offset": 4, "limit": 3}),
    )
    .await;
    let texts: Vec<&str> = r["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["L4", "L5", "L6"]);
    assert_eq!(r["lines"][0]["n"], 5);
    assert_eq!(r["eof"], false);
    assert_eq!(r["total"], 10);
}

#[tokio::test]
async fn read_refuses_binary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    call_bin(&k, json!({"type": "write", "path": "b.bin"}), b"a\x00b").await;
    let r = call(&k, json!({"type": "read", "path": "b.bin"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert!(r["error"].as_str().unwrap().contains("binary"));
}

#[tokio::test]
async fn append_creates_then_grows() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r1 = call(
        &k,
        json!({"type": "append", "path": "log.txt", "bytes": "one\n"}),
    )
    .await;
    assert_eq!(r1["size"], 4, "{r1}");
    let r2 = call(
        &k,
        json!({"type": "append", "path": "log.txt", "bytes": "two\n"}),
    )
    .await;
    assert_eq!(r2["size"], 8, "{r2}");
    assert_eq!(text_of(&tmp, "log.txt"), "one\ntwo\n");
    assert_ne!(r1["version"], r2["version"]);
}

#[tokio::test]
async fn write_is_atomic_no_tmp_debris_on_success() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.txt", "x\n").await;
    let names: Vec<String> = std::fs::read_dir(sd(&tmp))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.txt"]);
}

#[tokio::test]
async fn if_match_guards_write_and_edit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let w = seed(&k, "v.txt", "one\n").await;
    let stale = "0".repeat(16);
    let r = call(
        &k,
        json!({"type": "write", "path": "v.txt", "bytes": "two\n", "if_match": stale}),
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    assert_eq!(r["actual"], w["version"]);
    assert_eq!(text_of(&tmp, "v.txt"), "one\n");
    let r = call(
        &k,
        json!({"type": "write", "path": "v.txt", "bytes": "two\n", "if_match": w["version"]}),
    )
    .await;
    assert_ne!(r["version"], w["version"], "{r}");
    let r = patch(
        &k,
        "v.txt",
        Some(&block("two", "three")),
        json!({"if_match": stale}),
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    assert_eq!(r["path"], "v.txt");
}

/// The per-edit mismatch keys come in python's order.
#[tokio::test]
async fn version_mismatch_key_order_is_pythons() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "v.txt", "one\n").await;
    let r = call(
        &k,
        json!({"type": "write", "path": "v.txt", "bytes": "x", "if_match": 12345}),
    )
    .await;
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["error", "reason", "path", "expected", "actual"]);
    assert_eq!(r["expected"], 12345);
}

#[tokio::test]
async fn edit_dry_run_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "d.txt", "a = 1\n").await;
    let r = patch(
        &k,
        "d.txt",
        Some(&block("a = 1", "a = 2")),
        json!({"dry_run": true}),
    )
    .await;
    assert_eq!(r["dry_run"], true, "{r}");
    assert_eq!(r["edits"][0]["hunks"], 1);
    assert_eq!(text_of(&tmp, "d.txt"), "a = 1\n");
}

// ─── the TRANSACTION (multi-file edits in ONE call) ─────────────

#[tokio::test]
async fn multifile_edit_applies_both_atomically() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.py", "x = 1\n").await;
    seed(&k, "b.py", "y = 2\nz = 3\n").await;
    let blob = block("x = 1", "x = 10")
        + "\n"
        + &block("y = 2", "y = 20")
        + "\n"
        + &block("z = 3", "z = 30");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 2}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(r["edits"][0]["path"], "a.py");
    assert_eq!(r["edits"][1]["hunks"], 2);
    assert_eq!(text_of(&tmp, "a.py"), "x = 10\n");
    assert_eq!(text_of(&tmp, "b.py"), "y = 20\nz = 30\n");
    assert!(!sd(&tmp).join(".ckpt").exists(), "no pre-images on disk");
}

#[tokio::test]
async fn multifile_doomed_second_edit_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.py", "x = 1\n").await;
    seed(&k, "b.py", "y = 2\n").await;
    let blob = block("x = 1", "x = 10") + "\n" + &block("missing", "nope");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 1}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "no_match", "{r}");
    assert_eq!(r["failed"][0]["path"], "b.py");
    assert_eq!(text_of(&tmp, "a.py"), "x = 1\n");
    assert_eq!(text_of(&tmp, "b.py"), "y = 2\n");
}

#[tokio::test]
async fn hunk_counts_must_sum_to_the_blob() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.py", "x = 1\n").await;
    let r = call(
        &k,
        json!({"type": "edit", "edits": [{"path": "a.py", "hunks": 2}],
               "bytes": block("x = 1", "x = 2")}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert!(r["error"].as_str().unwrap().contains("sum"), "{r}");
    assert_eq!(text_of(&tmp, "a.py"), "x = 1\n");
}

#[tokio::test]
async fn per_edit_if_match_guards_the_whole_transaction() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.py", "one\n").await;
    seed(&k, "b.py", "two\n").await;
    let blob = block("one", "ONE") + "\n" + &block("two", "TWO");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.py", "hunks": 1},
                         {"path": "b.py", "hunks": 1, "if_match": "0".repeat(16)}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    assert_eq!(r["path"], "b.py");
    assert_eq!(text_of(&tmp, "a.py"), "one\n");
}

#[tokio::test]
async fn write_accepts_str_and_utf8_encodes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r = call(
        &k,
        json!({"type": "write", "path": "t.txt", "bytes": "h\u{e9}llo\n"}),
    )
    .await;
    assert_eq!(r["size"], "h\u{e9}llo\n".len(), "{r}");
    assert_eq!(text_of(&tmp, "t.txt"), "h\u{e9}llo\n");
}

/// On the binary channel the blob is the document — even an empty one.
#[tokio::test]
async fn binary_channel_blob_is_the_document() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r = call_bin(&k, json!({"type": "write", "path": "e.txt"}), b"").await;
    assert_eq!(r["size"], 0, "{r}");
    let r = call_bin(&k, json!({"type": "append", "path": "e.txt"}), b"x\n").await;
    assert_eq!(r["size"], 2, "{r}");
    assert_eq!(text_of(&tmp, "e.txt"), "x\n");
}

// ─── per-document serialization + payload hardening ─────────────

#[tokio::test]
async fn edit_refuses_the_same_path_twice() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.md", "one\ntwo\n").await;
    let blob = block("one", "1") + "\n" + &block("two", "2");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.md", "hunks": 1}, {"path": "./a.md", "hunks": 1}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "edits");
    assert!(r["error"].as_str().unwrap().contains("at most once"));
    assert_eq!(text_of(&tmp, "a.md"), "one\ntwo\n");
}

#[tokio::test]
async fn concurrent_edits_on_one_file_both_land() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "f.py", "a = 1\nb = 2\n").await;
    let b1 = block("a = 1", "a = 10");
    let b2 = block("b = 2", "b = 20");
    let (r1, r2) = tokio::join!(
        patch(&k, "f.py", Some(&b1), json!({})),
        patch(&k, "f.py", Some(&b2), json!({}))
    );
    assert_eq!(r1["edited"], true, "{r1}");
    assert_eq!(r2["edited"], true, "{r2}");
    assert_eq!(text_of(&tmp, "f.py"), "a = 10\nb = 20\n");
    let finals = [
        text::version_of(b"a = 10\nb = 20\n"),
        text::version_of(b"a = 10\nb = 2\n"),
        text::version_of(b"a = 1\nb = 20\n"),
    ];
    for r in [&r1, &r2] {
        let v = r["edits"][0]["version"].as_str().unwrap().to_string();
        assert!(finals.contains(&v), "{v}");
    }
    assert!(
        [&r1, &r2]
            .iter()
            .any(|r| r["edits"][0]["version"] == json!(finals[0])),
        "one reply is the final image"
    );
}

#[tokio::test]
async fn text_channel_bytes_guard_is_missing_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    for verb in ["write", "append"] {
        let r = call(&k, json!({"type": verb, "path": "x"})).await;
        assert_eq!(r["reason"], "missing_arg", "{r}");
        assert!(r["arg"].as_str().unwrap().starts_with("bytes"), "{r}");
    }
    let r = call(
        &k,
        json!({"type": "edit", "edits": [{"path": "x", "hunks": 1}]}),
    )
    .await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert!(r["arg"].as_str().unwrap().starts_with("bytes"), "{r}");
}

#[tokio::test]
async fn unknown_type_carries_verb() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r = call(&k, json!({"type": "frobnicate"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "frobnicate");
}

#[tokio::test]
async fn read_with_max_limit_does_not_trap() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "r.txt", "l1\nl2\n").await;
    let r = call(
        &k,
        json!({"type": "read", "path": "r.txt", "offset": 1, "limit": i64::MAX}),
    )
    .await;
    assert_eq!(r["eof"], true, "{r}");
    assert_eq!(r["lines"][0]["text"], "l2");
    assert_eq!(r["lines"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn non_int_args_are_bad_arg_refusals() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let body: String = (0..10).map(|i| format!("L{i}\n")).collect();
    seed(&k, "n.txt", &body).await;
    for bad in [json!("ten"), json!(1.5), json!(true), json!(-1), json!([0])] {
        let r = call(&k, json!({"type": "read", "path": "n.txt", "offset": bad})).await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "offset");
    }
    for bad in [json!("200"), json!(2.0), json!(true), json!(0), json!(-1)] {
        let r = call(&k, json!({"type": "read", "path": "n.txt", "limit": bad})).await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "limit");
    }
    let r = call(&k, json!({"type": "read", "path": "n.txt"})).await;
    assert_eq!(r["lines"].as_array().unwrap().len(), 10);
    assert_eq!(r["lines"][0]["n"], 1);
    assert_eq!(r["eof"], true);
}

#[tokio::test]
async fn edit_hunk_count_must_be_an_int() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "m.py", "a = 1\n").await;
    let blob = block("a = 1", "a = 2");
    for bad in [json!("1"), json!(1.5), json!(true), json!(0), json!(-1)] {
        let r = call(
            &k,
            json!({"type": "edit", "edits": [{"path": "m.py", "hunks": bad}], "bytes": blob}),
        )
        .await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "edits");
    }
    assert_eq!(text_of(&tmp, "m.py"), "a = 1\n");
}

// ─── undo: ONE slot per path, in RAM ────────────────────────────

#[tokio::test]
async fn undo_restores_what_the_last_write_replaced_and_redoes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "u.txt", "one\n").await;
    seed(&k, "u.txt", "two\n").await;
    let r = call(&k, json!({"type": "undo", "path": "u.txt"})).await;
    assert_eq!(r["undone"], true, "{r}");
    assert_eq!(text_of(&tmp, "u.txt"), "one\n");
    // a second undo is REDO: the slot now holds what the undo replaced
    call(&k, json!({"type": "undo", "path": "./u.txt"})).await;
    assert_eq!(text_of(&tmp, "u.txt"), "two\n");
}

#[tokio::test]
async fn undo_of_a_created_file_deletes_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "new.txt", "x\n").await;
    let r = call(&k, json!({"type": "undo", "path": "new.txt"})).await;
    assert_eq!(
        r,
        json!({"undone": true, "path": "new.txt", "deleted": true})
    );
    assert!(!sd(&tmp).join("new.txt").exists());
}

#[tokio::test]
async fn undo_reverts_an_edit_and_an_append() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "e.py", "a = 1\n").await;
    let r = patch(&k, "e.py", Some(&block("a = 1", "a = 2")), json!({})).await;
    assert_eq!(r["edited"], true, "{r}");
    call(&k, json!({"type": "undo", "path": "e.py"})).await;
    assert_eq!(text_of(&tmp, "e.py"), "a = 1\n");
    call(
        &k,
        json!({"type": "append", "path": "e.py", "bytes": "b = 2\n"}),
    )
    .await;
    call(&k, json!({"type": "undo", "path": "e.py"})).await;
    assert_eq!(text_of(&tmp, "e.py"), "a = 1\n");
}

#[tokio::test]
async fn undo_with_no_slot_is_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    put(&tmp, "z.txt", "z\n");
    let r = call(&k, json!({"type": "undo", "path": "z.txt"})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
    assert!(r["error"].as_str().unwrap().contains("nothing to undo"));
}

#[tokio::test]
async fn undo_is_refused_on_a_readonly_leg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({"readonly": true})).await;
    let r = call(&k, json!({"type": "undo", "path": "z.txt"})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
}

/// A deleted bridge takes its undo slots with it.
#[tokio::test]
async fn deleting_the_bridge_drops_its_slots() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "s.txt", "x\n").await;
    let del = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "fb"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(del["deleted"], true, "{del}");
    let k2 = mk(&tmp, json!({})).await;
    let r = call(&k2, json!({"type": "undo", "path": "s.txt"})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
}

#[tokio::test]
async fn text_verbs_see_only_exposed_paths() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({"allow": ["pub"]})).await;
    std::fs::create_dir_all(sd(&tmp).join("pub")).unwrap();
    put(&tmp, ".env", "SECRET=1\n");
    for verb in ["read", "write", "append", "undo"] {
        let r = call(&k, json!({"type": verb, "path": ".env", "bytes": "x"})).await;
        assert_eq!(r["reason"], "not_found", "{verb}: {r}");
    }
    assert_eq!(text_of(&tmp, ".env"), "SECRET=1\n");
    let r = call(
        &k,
        json!({"type": "write", "path": "pub/a.txt", "bytes": "ok"}),
    )
    .await;
    assert_eq!(r["size"], 2, "{r}");
}

/// The reflect carries the edit grammar and the compose rule.
#[tokio::test]
async fn reflect_carries_edit_grammar_and_compose() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    let r = call(&k, json!({"type": "reflect"})).await;
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert!(at("allow_count") < at("edit_grammar") && at("edit_grammar") < at("compose"));
    assert!(r.get("verbs").is_none(), "{r}");
    assert!(r.get("hidden_count").is_none(), "{r}");
    let docs = FileBundle::default().verb_docs();
    for v in ["read", "write", "edit", "append", "undo"] {
        assert!(docs.get(v).is_some(), "{v}");
    }
}

// ─── #206: a one-file edit needs no `hunks` count ───────────────

#[tokio::test]
async fn one_file_without_hunks_applies_every_block() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "b.py", "y = 2\nz = 3\n").await;
    let blob = block("y = 2", "y = 20") + "\n" + &block("z = 3", "z = 30");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "b.py"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(text_of(&tmp, "b.py"), "y = 20\nz = 30\n");
}

#[tokio::test]
async fn one_file_with_a_wrong_hunks_count_names_the_fix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "b.py", "y = 2\nz = 3\n").await;
    let blob = block("y = 2", "y = 20") + "\n" + &block("z = 3", "z = 30");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "b.py", "hunks": 3}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("leave hunks out"), "{r}");
    assert_eq!(text_of(&tmp, "b.py"), "y = 2\nz = 3\n");
}

#[tokio::test]
async fn two_files_still_need_hunks() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.py", "x = 1\n").await;
    seed(&k, "b.py", "y = 2\n").await;
    let blob = block("x = 1", "x = 10") + "\n" + &block("y = 2", "y = 20");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("hunks"), "{r}");
    assert_eq!(text_of(&tmp, "a.py"), "x = 1\n");
}

// ─── #206: a multi-file edit's blocks may name their file ─────────

fn named_block(path: &str, old: &str, new: &str) -> String {
    format!("<<<<<<< SEARCH {path}\n{old}\n=======\n{new}\n>>>>>>> REPLACE")
}

#[tokio::test]
async fn named_blocks_split_a_two_file_edit_without_hunks() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.txt", "x = 1\n").await;
    seed(&k, "b.txt", "y = 1\n").await;
    let blob =
        named_block("a.txt", "x = 1", "x = 2") + "\n" + &named_block("b.txt", "y = 1", "y = 2");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.txt"}, {"path": "b.txt"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(text_of(&tmp, "a.txt"), "x = 2\n");
    assert_eq!(text_of(&tmp, "b.txt"), "y = 2\n");
}

#[tokio::test]
async fn named_blocks_may_come_in_any_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.txt", "x = 1\nw = 3\n").await;
    seed(&k, "b.txt", "y = 1\n").await;
    let blob = named_block("a.txt", "x = 1", "x = 2")
        + "\n"
        + &named_block("b.txt", "y = 1", "y = 2")
        + "\n"
        + &named_block("a.txt", "w = 3", "w = 4");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.txt"}, {"path": "b.txt"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["edited"], true, "{r}");
    assert_eq!(text_of(&tmp, "a.txt"), "x = 2\nw = 4\n");
    assert_eq!(text_of(&tmp, "b.txt"), "y = 2\n");
}

#[tokio::test]
async fn a_block_naming_a_file_outside_edits_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.txt", "x = 1\n").await;
    let blob =
        named_block("a.txt", "x = 1", "x = 2") + "\n" + &named_block("c.txt", "z = 9", "z = 10");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.txt"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "bytes", "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("c.txt"), "{r}");
    assert_eq!(text_of(&tmp, "a.txt"), "x = 1\n");
}

#[tokio::test]
async fn named_and_unnamed_blocks_do_not_mix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.txt", "x = 1\n").await;
    seed(&k, "b.txt", "y = 1\n").await;
    let blob = named_block("a.txt", "x = 1", "x = 2") + "\n" + &block("y = 1", "y = 2");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.txt"}, {"path": "b.txt"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "bytes", "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("every block or on none"), "{r}");
    assert_eq!(text_of(&tmp, "a.txt"), "x = 1\n");
    assert_eq!(text_of(&tmp, "b.txt"), "y = 1\n");
}

#[tokio::test]
async fn a_stated_hunks_must_match_the_named_blocks() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp, json!({})).await;
    seed(&k, "a.txt", "x = 1\nw = 3\n").await;
    seed(&k, "b.txt", "y = 1\n").await;
    let blob = named_block("a.txt", "x = 1", "x = 2")
        + "\n"
        + &named_block("a.txt", "w = 3", "w = 4")
        + "\n"
        + &named_block("b.txt", "y = 1", "y = 2");
    let r = call(
        &k,
        json!({"type": "edit",
               "edits": [{"path": "a.txt", "hunks": 1}, {"path": "b.txt"}],
               "bytes": blob}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("leave hunks out"), "{r}");
    assert_eq!(text_of(&tmp, "a.txt"), "x = 1\nw = 3\n");
    assert_eq!(text_of(&tmp, "b.txt"), "y = 1\n");
}
