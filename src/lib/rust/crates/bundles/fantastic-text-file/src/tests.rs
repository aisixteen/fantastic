//! text_file — the stateless text-document agent, every byte a GATED send.
//!
//! The patch cases are the file_bridge `patch_text_file` suite, ported to the
//! send-path: the editor never touches disk; it sends stream verbs to the
//! file_bridge named by `target_id`. The old sealed posture case becomes "the
//! wired LEG refuses" — the leg's own gate answers, and the editor passes the
//! refusal through verbatim. Mirrors the python suite (the immutable
//! reference, `bundled_agents/text_file/tests/`).

use super::*;
use fantastic_file::FileBundle;
use fantastic_kernel::{Item, Kernel};
use serde_json::{json, Map};
use tempfile::TempDir;

/// A kernel with one OPEN file_bridge (`store`, rooted at `<tmp>/sd`) and one
/// editor (`ed`) wired to it — both gate ends granted.
async fn mk(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    mk_with(tmp, &[]).await
}

async fn mk_with(tmp: &TempDir, store_extra: &[(&str, Value)]) -> (Arc<Kernel>, AgentId) {
    let store_root = tmp.path().join("sd");
    let mut kernel = Kernel::new();
    kernel.bundles.register("file_bridge.tools", FileBundle);
    kernel.bundles.register(HANDLER_MODULE, TextFileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut store_meta = Map::new();
    store_meta.insert("root".to_string(), json!(store_root.to_string_lossy()));
    store_meta.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    store_meta.insert("callable_by".to_string(), json!({"ed": "*"}));
    for (k, v) in store_extra {
        store_meta.insert((*k).to_string(), v.clone());
    }
    let store = Agent::new(
        AgentId::from("store"),
        Some("file_bridge.tools".to_string()),
        Some(AgentId::from("kernel_state")),
        store_meta,
        store_root.clone(),
        false,
    );
    kernel.register(Arc::clone(&store));
    let mut meta = Map::new();
    meta.insert("target_id".to_string(), json!("store"));
    meta.insert("can_call".to_string(), json!({"store": "*"}));
    let ed = Agent::new(
        AgentId::from("ed"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("kernel_state")),
        meta,
        tmp.path().join("ed"),
        false,
    );
    kernel.register(Arc::clone(&ed));
    (kernel, AgentId::from("ed"))
}

async fn call(kernel: &Arc<Kernel>, ed: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(ed.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// A byte verb over the BINARY channel (the blob is the document/patch).
///
/// Through the BATCH (`Item::with_binary`), which is the production path and
/// the one python mirrors: one send, one task row, so `current_task()` names
/// this dispatch and a checkpoint id is that pid. `send_with_binary` bypasses
/// the batch and registers no task (documented at `Item::with_binary`).
async fn call_bin(kernel: &Arc<Kernel>, ed: &AgentId, header: Value, blob: &[u8]) -> Value {
    kernel
        .send(vec![
            Item::new(ed.clone(), header).with_binary(blob.to_vec())
        ])
        .await
        .remove(0)
        .result
}

fn block(old: &str, new: &str) -> String {
    format!("<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE")
}

async fn seed(kernel: &Arc<Kernel>, ed: &AgentId, path: &str, text: &str) -> Value {
    call_bin(
        kernel,
        ed,
        json!({"type": "write", "path": path}),
        text.as_bytes(),
    )
    .await
}

fn on_disk(tmp: &TempDir, rel: &str) -> String {
    std::fs::read_to_string(tmp.path().join("sd").join(rel)).unwrap()
}

// ─── patch: the ported grammar cases ────────────────────────────

#[tokio::test]
async fn patch_single_hunk_applies_and_writes() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "m.py", "a = 1\nb = 2\nc = 3\n").await;
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 1}]}),
        block("b = 2", "b = 20").as_bytes(),
    )
    .await;
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(r["edits"][0]["hunks"], 1);
    assert_eq!(on_disk(&tmp, "m.py"), "a = 1\nb = 20\nc = 3\n");
}

#[tokio::test]
async fn patch_multi_hunk_evolving_and_multiline() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(
        &k,
        &ed,
        "m.py",
        "def f():\n    return 1\n\ndef g():\n    return 2\n",
    )
    .await;
    let patch = format!(
        "{}\n{}",
        block("def f():\n    return 1", "def f():\n    return 99"),
        block("return 2", "return 22"),
    );
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 2}]}),
        patch.as_bytes(),
    )
    .await;
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(r["edits"][0]["hunks"], 2);
    assert_eq!(
        on_disk(&tmp, "m.py"),
        "def f():\n    return 99\n\ndef g():\n    return 22\n"
    );
}

#[tokio::test]
async fn patch_failures_are_loud_and_write_nothing() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "m.py", "v = 1\nv = 1\n").await;
    // ambiguous
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 1}]}),
        block("v = 1", "v = 2").as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "ambiguous", "{r}");
    // no_match, second hunk — all-or-nothing
    let patch = format!(
        "{}\n{}",
        block("v = 1\nv = 1", "w = 1"),
        block("missing", "x")
    );
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 2}]}),
        patch.as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "no_match", "{r}");
    assert_eq!(r["failed"][0]["index"], 1);
    assert_eq!(on_disk(&tmp, "m.py"), "v = 1\nv = 1\n");
    // malformed / empty / bad utf8 / missing path / not found
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 1}]}),
        b"<<<<<<< SEARCH\nv = 1\n=======\nv = 2\n",
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 1}]}),
        b"no markers",
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 1}]}),
        &[0xff, 0xfe],
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch"}),
        block("a", "b").as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "nope.py", "hunks": 1}]}),
        block("a", "b").as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    assert_eq!(on_disk(&tmp, "m.py"), "v = 1\nv = 1\n");
}

#[tokio::test]
async fn sealed_leg_denies_and_editor_passes_it_through() {
    let tmp = TempDir::new().unwrap();
    // ingress_rule stripped ⇒ the leg is sealed by default.
    let (k, ed) = mk_with(&tmp, &[("ingress_rule", Value::Null)]).await;
    std::fs::create_dir_all(tmp.path().join("sd")).unwrap();
    std::fs::write(tmp.path().join("sd/m.py"), "a = 1\n").unwrap();
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "m.py", "hunks": 1}]}),
        block("a = 1", "a = 2").as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(on_disk(&tmp, "m.py"), "a = 1\n");
}

// ─── read / write / append / version / if_match ─────────────────

#[tokio::test]
async fn write_then_read_lines_and_version() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let w = seed(&k, &ed, "doc.md", "alpha\nbeta\ngamma\n").await;
    assert_eq!(w["size"], 17, "{w}");
    assert_eq!(w["version"].as_str().unwrap().len(), 16);
    let r = call(&k, &ed, json!({"type": "read", "path": "doc.md"})).await;
    let lines: Vec<&str> = r["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["text"].as_str().unwrap())
        .collect();
    assert_eq!(lines, ["alpha", "beta", "gamma"]);
    assert_eq!(r["lines"][0]["n"], 1);
    assert_eq!(r["total"], 3);
    assert_eq!(r["eof"], true);
    assert_eq!(r["version"], w["version"]);
    // paging
    let r = call(
        &k,
        &ed,
        json!({"type": "read", "path": "doc.md", "offset": 1, "limit": 1}),
    )
    .await;
    assert_eq!(r["lines"][0]["text"], "beta");
    assert_eq!(r["lines"][0]["n"], 2);
    assert_eq!(r["eof"], false);
    // binary refused
    call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "b.bin"}),
        b"a\x00b",
    )
    .await;
    let r = call(&k, &ed, json!({"type": "read", "path": "b.bin"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
}

#[tokio::test]
async fn append_creates_then_grows_and_write_leaves_no_tmp_debris() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r1 = call_bin(
        &k,
        &ed,
        json!({"type": "append", "path": "log.txt"}),
        b"one\n",
    )
    .await;
    assert_eq!(r1["size"], 4, "{r1}");
    let r2 = call_bin(
        &k,
        &ed,
        json!({"type": "append", "path": "log.txt"}),
        b"two\n",
    )
    .await;
    assert_eq!(r2["size"], 8, "{r2}");
    assert_eq!(on_disk(&tmp, "log.txt"), "one\ntwo\n");
    assert_ne!(r1["version"], r2["version"]);
    seed(&k, &ed, "a.txt", "x\n").await;
    let names: Vec<String> = std::fs::read_dir(tmp.path().join("sd"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains(".tmp.")),
        "tmp debris: {names:?}"
    );
}

#[tokio::test]
async fn if_match_guards_write_and_patch() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let w = seed(&k, &ed, "v.txt", "one\n").await;
    let stale = "0".repeat(16);
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "v.txt", "if_match": stale}),
        b"two\n",
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    assert_eq!(r["actual"], w["version"]);
    assert_eq!(on_disk(&tmp, "v.txt"), "one\n");
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "v.txt", "if_match": w["version"]}),
        b"two\n",
    )
    .await;
    assert_ne!(r["version"], w["version"], "{r}");
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "v.txt", "hunks": 1, "if_match": stale}]}),
        block("two", "three").as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    // dry_run writes nothing
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "v.txt", "hunks": 1}], "dry_run": true}),
        block("two", "three").as_bytes(),
    )
    .await;
    assert_eq!(r["dry_run"], true, "{r}");
    assert_eq!(on_disk(&tmp, "v.txt"), "two\n");
}

// ─── checkpoints ────────────────────────────────────────────────

#[tokio::test]
async fn checkpointed_write_restores_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "c.txt", "original\n").await;
    let w = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "c.txt", "checkpoint": true}),
        b"mutated\n",
    )
    .await;
    // A checkpoint id IS the dispatch's kernel task pid — an INT, python's
    // one id space with `ps`, the trace and the chat message (A3.10).
    let ck = w["ckpt_id"].as_i64().expect("ckpt_id is an int");
    assert!(ck > 0, "minted pid: {w}");
    assert_eq!(on_disk(&tmp, "c.txt"), "mutated\n");
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "c.txt", "ckpt_id": ck}),
    )
    .await;
    assert_eq!(r["restored"], true, "{r}");
    assert_eq!(on_disk(&tmp, "c.txt"), "original\n");
}

#[tokio::test]
async fn checkpoint_of_absent_file_restores_to_absence() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let w = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "new.txt", "checkpoint": true}),
        b"born\n",
    )
    .await;
    let ck = w["ckpt_id"].as_i64().expect("ckpt_id is an int");
    assert!(tmp.path().join("sd/new.txt").exists());
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "new.txt", "ckpt_id": ck}),
    )
    .await;
    assert_eq!(r["restored"], true, "{r}");
    assert_eq!(r["deleted"], true);
    assert!(!tmp.path().join("sd/new.txt").exists());
}

#[tokio::test]
async fn checkpoint_list_drop_restore_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "p.txt", "a = 1\n").await;
    let c = call(&k, &ed, json!({"type": "checkpoint", "path": "p.txt"})).await;
    let ck = c["ckpt_id"].as_i64().expect("ckpt_id is an int");
    call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "p.txt", "hunks": 1}]}),
        block("a = 1", "a = 2").as_bytes(),
    )
    .await;
    let ls = call(&k, &ed, json!({"type": "checkpoints", "path": "p.txt"})).await;
    // `checkpoints` reports an all-digit name as an int, python's
    // `int(name) if name.isdigit() else name`.
    let ids: Vec<i64> = ls["ckpts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_i64().expect("int ckpt id"))
        .collect();
    assert_eq!(ids, [ck], "{ls}");
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "p.txt", "ckpt_id": ck}),
    )
    .await;
    assert_eq!(r["version"], c["version"], "{r}");
    assert_eq!(on_disk(&tmp, "p.txt"), "a = 1\n");
    let r = call(
        &k,
        &ed,
        json!({"type": "drop", "path": "p.txt", "ckpt_id": ck}),
    )
    .await;
    assert_eq!(r["dropped"], true, "{r}");
    let ls = call(&k, &ed, json!({"type": "checkpoints", "path": "p.txt"})).await;
    assert_eq!(ls["ckpts"].as_array().unwrap().len(), 0);
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "p.txt", "ckpt_id": ck}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
}

// ─── unwired failfast + reflect ─────────────────────────────────

#[tokio::test]
async fn unwired_editor_fails_loud_on_every_verb() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, TextFileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let ed = Agent::new(
        AgentId::from("bare"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("kernel_state")),
        Map::new(),
        tmp.path().join("bare"),
        false,
    );
    kernel.register(Arc::clone(&ed));
    let ed = AgentId::from("bare");
    for verb in ["read", "checkpoint", "checkpoints", "restore", "drop"] {
        let r = call(&kernel, &ed, json!({"type": verb, "path": "x"})).await;
        assert_eq!(r["reason"], "not_found", "{verb}: {r}");
        assert!(
            r["error"].as_str().unwrap().contains("target"),
            "{verb}: {r}"
        );
    }
    for verb in ["write", "patch", "append"] {
        let r = call_bin(&kernel, &ed, json!({"type": verb, "path": "x"}), b"y").await;
        assert_eq!(r["reason"], "not_found", "{verb}: {r}");
    }
}

#[tokio::test]
async fn reflect_names_surface_and_grammar_and_byte_verbs_redirect_on_text() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r = call(&k, &ed, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], "ed");
    assert_eq!(r["target_id"], "store");
    assert!(r["patch_grammar"].as_str().unwrap().contains("SEARCH"));
    for v in [
        "read",
        "write",
        "patch",
        "append",
        "checkpoint",
        "restore",
        "drop",
    ] {
        assert!(r["verbs"].get(v).is_some(), "verb {v} missing: {r}");
    }
    // A str `bytes` on the TEXT channel is the text courtesy (py-canonical,
    // 2026-09-01): the document, utf-8-encoded — a spawned program's ctrl-fd
    // wire is JSON and has no bytes type, so this is its one write path.
    let r = call(
        &k,
        &ed,
        json!({"type": "write", "path": "x", "bytes": "hi\n"}),
    )
    .await;
    assert_eq!(r["size"], 3, "{r}");
    // With NO usable document the byte verbs refuse with the one shape.
    let r = call(&k, &ed, json!({"type": "write", "path": "x"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert!(r["arg"].as_str().unwrap().starts_with("bytes"), "{r}");
}

// ─── the TRANSACTION (multi-file edits in ONE patch) ────────────

#[tokio::test]
async fn multifile_patch_applies_both_atomically() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "x = 1\n").await;
    seed(&k, &ed, "b.py", "y = 2\nz = 3\n").await;
    let blob = format!(
        "{}\n{}\n{}",
        block("x = 1", "x = 10"),
        block("y = 2", "y = 20"),
        block("z = 3", "z = 30")
    );
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [
            {"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 2}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(r["patched"], true, "{r}");
    assert_eq!(r["edits"][1]["hunks"], 2);
    assert_eq!(on_disk(&tmp, "a.py"), "x = 10\n");
    assert_eq!(on_disk(&tmp, "b.py"), "y = 20\nz = 30\n");
    // committed transaction self-cleans its pre-images
    let ck = tmp.path().join("sd").join(".ckpt");
    let leftovers = std::fs::read_dir(&ck)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    let files = walk_files(&ck);
    assert!(
        files.is_empty(),
        "pre-image debris: {files:?} ({leftovers} entries)"
    );
}

fn walk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[tokio::test]
async fn multifile_doomed_second_edit_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "x = 1\n").await;
    seed(&k, &ed, "b.py", "y = 2\n").await;
    let blob = format!("{}\n{}", block("x = 1", "x = 10"), block("missing", "nope"));
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [
            {"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 1}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "no_match", "{r}");
    assert_eq!(r["failed"][0]["path"], "b.py");
    assert_eq!(on_disk(&tmp, "a.py"), "x = 1\n");
    assert_eq!(on_disk(&tmp, "b.py"), "y = 2\n");
}

#[tokio::test]
async fn hunk_counts_must_sum_to_the_blob() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "x = 1\n").await;
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "a.py", "hunks": 2}]}),
        block("x = 1", "x = 2").as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert!(r["error"].as_str().unwrap().contains("sum"), "{r}");
    assert_eq!(on_disk(&tmp, "a.py"), "x = 1\n");
}

#[tokio::test]
async fn multifile_checkpoint_true_keeps_preimages() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "one\n").await;
    seed(&k, &ed, "b.py", "two\n").await;
    let blob = format!("{}\n{}", block("one", "ONE"), block("two", "TWO"));
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "checkpoint": true, "edits": [
            {"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 1}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(r["patched"], true, "{r}");
    let cks = r["ckpt_ids"].as_array().expect("ckpt_ids").clone();
    assert_eq!(cks.len(), 2);
    for row in &cks {
        let rr = call(
            &k,
            &ed,
            json!({"type": "restore", "path": row["path"], "ckpt_id": row["ckpt_id"]}),
        )
        .await;
        assert_eq!(rr["restored"], true, "{rr}");
    }
    assert_eq!(on_disk(&tmp, "a.py"), "one\n");
    assert_eq!(on_disk(&tmp, "b.py"), "two\n");
}

#[tokio::test]
async fn per_edit_if_match_guards_the_whole_transaction() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "one\n").await;
    seed(&k, &ed, "b.py", "two\n").await;
    let blob = format!("{}\n{}", block("one", "ONE"), block("two", "TWO"));
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [
            {"path": "a.py", "hunks": 1},
            {"path": "b.py", "hunks": 1, "if_match": "0000000000000000"}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    assert_eq!(r["path"], "b.py");
    assert_eq!(on_disk(&tmp, "a.py"), "one\n");
}

// ─── per-document serialization + payload hardening (sweep I-5) ──

#[tokio::test]
async fn patch_refuses_the_same_path_twice() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.md", "one\ntwo\n").await;
    let blob = format!("{}\n{}", block("one", "1"), block("two", "2"));
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "a.md", "hunks": 1}, {"path": "a.md", "hunks": 1}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "edits", "{r}");
    assert!(r["error"].as_str().unwrap().contains("at most once"), "{r}");
    assert_eq!(on_disk(&tmp, "a.md"), "one\ntwo\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_patches_on_one_file_both_land() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "f.py", "a = 1\nb = 2\n").await;
    let one = {
        let (k, ed) = (Arc::clone(&k), ed.clone());
        tokio::spawn(async move {
            call_bin(
                &k,
                &ed,
                json!({"type": "patch", "edits": [{"path": "f.py", "hunks": 1}]}),
                block("a = 1", "a = 10").as_bytes(),
            )
            .await
        })
    };
    let two = {
        let (k, ed) = (Arc::clone(&k), ed.clone());
        tokio::spawn(async move {
            call_bin(
                &k,
                &ed,
                json!({"type": "patch", "edits": [{"path": "f.py", "hunks": 1}]}),
                block("b = 2", "b = 20").as_bytes(),
            )
            .await
        })
    };
    let (r1, r2) = (one.await.unwrap(), two.await.unwrap());
    assert_eq!(r1["patched"], true, "{r1}");
    assert_eq!(r2["patched"], true, "{r2}");
    let final_text = on_disk(&tmp, "f.py");
    assert_eq!(final_text, "a = 10\nb = 20\n");
    // each reply's version is the document as it stood at THAT commit: one is
    // the final image, the other the intermediate with only its own hunk.
    let v_final = version_of(final_text.as_bytes());
    let versions = [
        r1["edits"][0]["version"].as_str().unwrap().to_string(),
        r2["edits"][0]["version"].as_str().unwrap().to_string(),
    ];
    assert!(versions.contains(&v_final), "{versions:?} vs {v_final}");
    let intermediates = [
        version_of(b"a = 10\nb = 2\n"),
        version_of(b"a = 1\nb = 20\n"),
    ];
    for v in &versions {
        assert!(*v == v_final || intermediates.contains(v), "{v}");
    }
}

#[tokio::test]
async fn text_channel_bytes_guard_is_missing_arg() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    // A3.1: the order is unwired -> path/edits -> bytes, and each verb names
    // its OWN document arg (py ::test_text_channel_bytes_guard_is_missing_arg).
    for (verb, arg) in [
        ("write", "bytes (the document — str is utf-8-encoded)"),
        ("append", "bytes (one chunk — str is utf-8-encoded)"),
    ] {
        let r = call(&k, &ed, json!({"type": verb, "path": "x"})).await;
        assert_eq!(r["reason"], "missing_arg", "{verb}: {r}");
        assert_eq!(r["arg"], arg, "{verb}: {r}");
        assert_eq!(r["error"], format!("{verb}: {arg} required"), "{r}");
    }
    let r = call(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "x", "hunks": 1}]}),
    )
    .await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(
        r["arg"], "bytes (the utf-8 patch — all hunks, in edits order)",
        "{r}"
    );
    // …and `edits` is reported BEFORE `bytes` (py ::test_missing_path).
    let r = call(&k, &ed, json!({"type": "patch"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(
        r["arg"], "edits (a list of {path, hunks, if_match?})",
        "{r}"
    );
    // A missing `path` beats a missing document on the byte verbs too.
    for verb in ["write", "append"] {
        let r = call(&k, &ed, json!({"type": verb})).await;
        assert_eq!(r["arg"], "path", "{verb}: {r}");
    }
}

#[tokio::test]
async fn unknown_type_carries_verb() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r = call(&k, &ed, json!({"type": "frobnicate"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "frobnicate", "{r}");
    // A3.3: python reprs the RAW type value.
    assert_eq!(r["error"], "text_file: unknown type 'frobnicate'", "{r}");
}

#[tokio::test]
async fn read_with_max_limit_does_not_trap() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "r.txt", "l1\nl2\n").await;
    let r = call(
        &k,
        &ed,
        json!({"type": "read", "path": "r.txt", "offset": 1, "limit": i64::MAX}),
    )
    .await;
    assert_eq!(r["eof"], true, "{r}");
    assert_eq!(r["lines"][0]["text"], "l2", "{r}");
    assert_eq!(r["lines"].as_array().unwrap().len(), 1, "{r}");
}

#[tokio::test]
async fn non_int_args_are_bad_arg_refusals() {
    // #136 — a present-but-wrong number is a TYPO, not a new default.
    // `as_u64().unwrap_or(0)` used to swallow `offset: "12"` and silently page
    // from the top.
    let tmp = TempDir::new().unwrap();
    let (kernel, ed) = mk(&tmp).await;
    let body: String = (0..10).map(|i| format!("L{i}\n")).collect();
    seed(&kernel, &ed, "n.txt", &body).await;
    for bad_value in [
        json!("ten"),
        json!(1.5),
        json!(true),
        json!(null),
        json!(-1),
        json!([0]),
    ] {
        let r = call(
            &kernel,
            &ed,
            json!({"type": "read", "path": "n.txt", "offset": bad_value.clone()}),
        )
        .await;
        assert_eq!(r["reason"], json!("bad_arg"), "{bad_value}: {r}");
        assert_eq!(r["arg"], json!("offset"), "{bad_value}: {r}");
    }
    for bad_value in [
        json!("200"),
        json!(2.5),
        json!(true),
        json!(null),
        json!(0),
        json!(-1),
    ] {
        let r = call(
            &kernel,
            &ed,
            json!({"type": "read", "path": "n.txt", "limit": bad_value.clone()}),
        )
        .await;
        assert_eq!(r["reason"], json!("bad_arg"), "{bad_value}: {r}");
        assert_eq!(r["arg"], json!("limit"), "{bad_value}: {r}");
    }
    // ABSENT still takes the documented defaults (offset 0, limit 200).
    let r = call(&kernel, &ed, json!({"type": "read", "path": "n.txt"})).await;
    assert_eq!(r["total"], json!(10), "{r}");
    assert_eq!(r["lines"][0]["n"], json!(1), "{r}");
    assert_eq!(r["lines"][9]["text"], json!("L9"), "{r}");
    assert_eq!(r["eof"], json!(true), "{r}");
}

#[tokio::test]
async fn patch_hunk_count_must_be_an_int() {
    // #136 — the per-edit `hunks` count is typed too: a string count would slice
    // the shared blob by nothing at all.
    let tmp = TempDir::new().unwrap();
    let (kernel, ed) = mk(&tmp).await;
    seed(&kernel, &ed, "m.py", "a = 1\n").await;
    let blob = block("a = 1", "a = 2");
    for bad_value in [
        json!("1"),
        json!(1.5),
        json!(true),
        json!(null),
        json!(0),
        json!(-1),
    ] {
        let r = call_bin(
            &kernel,
            &ed,
            json!({"type": "patch", "edits": [{"path": "m.py", "hunks": bad_value.clone()}]}),
            blob.as_bytes(),
        )
        .await;
        assert_eq!(r["reason"], json!("bad_arg"), "{bad_value}: {r}");
        assert_eq!(r["arg"], json!("edits"), "{bad_value}: {r}");
    }
    // …and the file was not touched by any of them.
    assert_eq!(on_disk(&tmp, "m.py"), "a = 1\n");
}

/// A CKPT ID IS A PID, and a pid is a NUMBER — so the notation types the
/// model's `<ckpt_id>29</ckpt_id>` as an integer, and `restore` must take it.
/// Reading it as a string only refused the model its own checkpoint back
/// (measured 2026-09-10 on `undo:rust`: "restore: ckpt_id required", twenty
/// times in one run).
#[tokio::test]
async fn restore_takes_the_ckpt_id_as_the_notation_typed_it() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "c.txt", "before\n").await;
    let w = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "c.txt", "checkpoint": true}),
        b"after\n",
    )
    .await;
    let ck = w["ckpt_id"].as_i64().expect("ckpt_id is an int");
    assert_eq!(on_disk(&tmp, "c.txt"), "after\n");

    // The model writes back the int it was shown.
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "c.txt", "ckpt_id": ck}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(on_disk(&tmp, "c.txt"), "before\n");
}

// ─── python parity (A3) ─────────────────────────────────────────

/// A3.2 — `shutdown` is not in python's VERBS map, so it is an unknown type,
/// not a silent no-op (py `tools.py:787-798,811-816`).
#[tokio::test]
async fn shutdown_is_unhandled() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r = call(&k, &ed, json!({"type": "shutdown"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "shutdown", "{r}");
    assert_eq!(r["error"], "text_file: unknown type 'shutdown'", "{r}");
    // `boot` stays the no-op python declares.
    assert_eq!(call(&k, &ed, json!({"type": "boot"})).await, Value::Null);
}

/// A3.3 — every refusal that embeds a value uses python's `repr`: an int bare,
/// a str single-quoted (py `tools.py:505,554,755,815`).
#[tokio::test]
async fn refusal_texts_use_python_repr() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "c.txt", "keep\n").await;
    // restore, no such checkpoint — a RAW int id stays bare, the path is quoted
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "c.txt", "ckpt_id": 999999}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    assert_eq!(
        r["error"], "restore: no checkpoint 999999 for 'c.txt'",
        "{r}"
    );
    // …and a string id is quoted like python's `{ckpt_id!r}`.
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "c.txt", "ckpt_id": "tf:deadbeef"}),
    )
    .await;
    assert_eq!(
        r["error"], "restore: no checkpoint 'tf:deadbeef' for 'c.txt'",
        "{r}"
    );
    // a per-edit `hunks` count that is not an int >= 1
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "a.py", "hunks": "2"}]}),
        block("x", "y").as_bytes(),
    )
    .await;
    assert_eq!(
        r["error"], "patch: edits must be edit 0 ('a.py'): `hunks` (int >= 1)",
        "{r}"
    );
    // a non-utf8 document inside a transaction
    seed(&k, &ed, "raw.bin", "").await;
    std::fs::write(tmp.path().join("sd/raw.bin"), [0xffu8, 0xfe]).unwrap();
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "raw.bin", "hunks": 1}]}),
        block("x", "y").as_bytes(),
    )
    .await;
    assert_eq!(
        r["error"], "patch: edits must be 'raw.bin': a utf-8 text file",
        "{r}"
    );
}

/// A3.4 — every edit is validated (python does not `break` on a short hunk
/// stream) and the sum names the FULL claim (py `tools.py:495-527`).
#[tokio::test]
async fn patch_reports_the_full_hunk_claim() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "x = 1\n").await;
    seed(&k, &ed, "b.py", "y = 2\n").await;
    let blob = format!("{}\n{}", block("x = 1", "x = 10"), block("y = 2", "y = 20"));
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [
            {"path": "a.py", "hunks": 3}, {"path": "b.py", "hunks": 1}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(
        r["error"],
        "patch: edits must be hunk counts that sum to the blob's blocks — \
         edits claim 4, the patch holds 2",
        "{r}"
    );
    // a LATER edit's own typing still wins over the sum — no early break.
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [
            {"path": "a.py", "hunks": 3}, {"path": "b.py", "hunks": 0}]}),
        blob.as_bytes(),
    )
    .await;
    assert_eq!(
        r["error"], "patch: edits must be edit 1 ('b.py'): `hunks` (int >= 1)",
        "{r}"
    );
    assert_eq!(on_disk(&tmp, "a.py"), "x = 1\n");
}

/// A3.5 — the per-edit version-mismatch dict is written `error, reason, path,
/// expected, actual` (py `tools.py:544-550`).
#[tokio::test]
async fn per_edit_version_mismatch_key_order() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "a.py", "one\n").await;
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [
            {"path": "a.py", "hunks": 1, "if_match": "0000000000000000"}]}),
        block("one", "ONE").as_bytes(),
    )
    .await;
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["error", "reason", "path", "expected", "actual"],
        "{r}"
    );
    // the bare `if_match` guard has no `path` (py `_if_match_guard`).
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "a.py", "if_match": "0000000000000000"}),
        b"two\n",
    )
    .await;
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["error", "reason", "expected", "actual"], "{r}");
}

/// A3.6 — `validate_meta` refuses an unusable `target_id` (py
/// `tools.py:801-808`).
#[tokio::test]
async fn validate_meta_refuses_an_empty_target_id() {
    assert_eq!(
        TextFileBundle.validate_meta(
            &[("target_id".to_string(), json!(""))].into_iter().collect(),
            &Map::new()
        ),
        Some("target_id must be a non-empty file_bridge agent id".to_string())
    );
    assert_eq!(
        TextFileBundle.validate_meta(
            &[("target_id".to_string(), json!(7))].into_iter().collect(),
            &Map::new()
        ),
        Some("target_id must be a non-empty file_bridge agent id".to_string())
    );
    assert_eq!(TextFileBundle.validate_meta(&Map::new(), &Map::new()), None);
    // …and the substrate refuses the create.
    let tmp = TempDir::new().unwrap();
    let (k, _ed) = mk(&tmp).await;
    let r = call(
        &k,
        &AgentId::from("kernel_state"),
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "ed2",
               "handler_module": HANDLER_MODULE, "target_id": ""}),
    )
    .await;
    assert_eq!(r["reason"], "invalid_meta", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .contains("target_id must be a non-empty file_bridge agent id"),
        "{r}"
    );
    assert!(!k.agents.contains_key(&AgentId::from("ed2")));
}

/// A3.7 — `checkpoint`/`dry_run` go by python truthiness, `if_match` guards
/// whenever it is present and non-null (a non-string never matches, and the
/// RAW value is echoed as `expected`), and a falsy `ckpt_id` is missing.
#[tokio::test]
async fn arg_typing_follows_python_truthiness() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "t.txt", "one\n").await;
    // a truthy non-bool `checkpoint` still takes a pre-image
    let w = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "t.txt", "checkpoint": 1}),
        b"two\n",
    )
    .await;
    assert!(w["ckpt_id"].is_i64(), "{w}");
    // a non-string `if_match` can never equal a hex digest
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "t.txt", "if_match": 12345}),
        b"three\n",
    )
    .await;
    assert_eq!(r["reason"], "version_mismatch", "{r}");
    assert_eq!(r["expected"], 12345, "{r}");
    assert_eq!(on_disk(&tmp, "t.txt"), "two\n");
    // an explicit null `if_match` does NOT guard (python: `want is None`)
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "write", "path": "t.txt", "if_match": null}),
        b"four\n",
    )
    .await;
    assert_eq!(on_disk(&tmp, "t.txt"), "four\n", "{r}");
    // 0 is a falsy ckpt id
    for verb in ["restore", "drop"] {
        let r = call(
            &k,
            &ed,
            json!({"type": verb, "path": "t.txt", "ckpt_id": 0}),
        )
        .await;
        assert_eq!(r["reason"], "missing_arg", "{verb}: {r}");
        assert_eq!(r["arg"], "ckpt_id", "{verb}: {r}");
    }
    // a truthy non-bool `dry_run` writes nothing
    let r = call_bin(
        &k,
        &ed,
        json!({"type": "patch", "dry_run": "yes",
               "edits": [{"path": "t.txt", "hunks": 1}]}),
        block("four", "five").as_bytes(),
    )
    .await;
    assert_eq!(r["dry_run"], true, "{r}");
    assert_eq!(on_disk(&tmp, "t.txt"), "four\n");
}

/// A3.8 — the ONE leg-reply failure test: a non-object reply becomes
/// `{"error": str(r)}`, an object fails only on a TRUTHY `error`
/// (py `tools.py:169-170,202-203,225-226,…`).
#[test]
fn leg_failure_mirrors_python() {
    assert_eq!(leg_failure(&Value::Null), Some(json!({"error": "None"})));
    assert_eq!(leg_failure(&json!(7)), Some(json!({"error": "7"})));
    assert_eq!(leg_failure(&json!("boom")), Some(json!({"error": "boom"})));
    assert_eq!(
        leg_failure(&json!([1, 2])),
        Some(json!({"error": "[1, 2]"}))
    );
    assert_eq!(leg_failure(&json!({})), None);
    assert_eq!(leg_failure(&json!({"error": ""})), None);
    assert_eq!(leg_failure(&json!({"error": null})), None);
    assert_eq!(
        leg_failure(&json!({"error": "no", "reason": "not_found"})),
        Some(json!({"error": "no", "reason": "not_found"}))
    );
}

/// A3.9 — the tag grammar types `<path>2024</path>` as an int; a TRUTHY
/// non-string path rides to the leg verbatim and gets the leg's own refusal.
/// Only a falsy path is this agent's to refuse (py `tools.py:322-324,…`).
#[tokio::test]
async fn a_truthy_non_string_path_goes_to_the_leg() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r = call(&k, &ed, json!({"type": "read", "path": 2024})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["error"], "read_stream: path must be a string", "{r}");
    let r = call(&k, &ed, json!({"type": "checkpoint", "path": 2024})).await;
    assert_eq!(r["error"], "read_stream: path must be a string", "{r}");
    let r = call_bin(&k, &ed, json!({"type": "append", "path": 2024}), b"x").await;
    assert_eq!(r["error"], "write_stream: path must be a string", "{r}");
    // falsy stays ours
    for p in [json!(""), json!(0), json!(null), json!([])] {
        let r = call(&k, &ed, json!({"type": "read", "path": p})).await;
        assert_eq!(r["reason"], "missing_arg", "{p}: {r}");
        assert_eq!(r["arg"], "path", "{p}: {r}");
    }
}

/// A3.10 — ckpt ids are ints end to end: `checkpoint` mints one, `write` adds
/// the key only when it is non-zero, `checkpoints` reports an all-digit name as
/// an int, and `drop` echoes the RAW payload value (py `tools.py:100-103,…`).
#[tokio::test]
async fn ckpt_ids_are_ints() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "i.txt", "one\n").await;
    let c = call(&k, &ed, json!({"type": "checkpoint", "path": "i.txt"})).await;
    let id = c["ckpt_id"].as_i64().expect("int ckpt_id");
    let ls = call(&k, &ed, json!({"type": "checkpoints", "path": "i.txt"})).await;
    assert_eq!(ls["ckpts"][0]["id"], json!(id), "{ls}");
    assert!(ls["ckpts"][0]["id"].is_i64(), "{ls}");
    // a plain write mints nothing
    let w = seed(&k, &ed, "i.txt", "two\n").await;
    assert!(w.get("ckpt_id").is_none(), "{w}");
    // `drop` echoes what the caller passed, verbatim
    let r = call(
        &k,
        &ed,
        json!({"type": "drop", "path": "i.txt", "ckpt_id": id}),
    )
    .await;
    assert_eq!(
        r,
        json!({"dropped": true, "ckpt_id": id, "path": "i.txt"}),
        "{r}"
    );
}

/// A3.11 — every reflect verb line is the first line of the python verb's
/// docstring, read from the immutable python source.
#[tokio::test]
async fn reflect_verbs_are_python_docstrings_verbatim() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r = call(&k, &ed, json!({"type": "reflect"})).await;
    let py = std::fs::read_to_string(PY_TOOLS).expect("python tools.py");
    let verbs = [
        "reflect",
        "boot",
        "read",
        "write",
        "patch",
        "append",
        "checkpoint",
        "checkpoints",
        "restore",
        "drop",
    ];
    // key order is python's VERBS order too
    let got: Vec<&str> = r["verbs"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(got, verbs, "{r}");
    for v in verbs {
        let head = format!("async def _{v}(id, payload, kernel):\n    \"\"\"");
        let at = py
            .find(&head)
            .unwrap_or_else(|| panic!("python def for {v}"))
            + head.len();
        let rest = &py[at..];
        let end = rest.find("\"\"\"").expect("docstring close");
        let want = rest[..end.min(rest.find('\n').unwrap_or(end))].trim_end();
        assert_eq!(r["verbs"][v], want, "{v}");
    }
}

/// A3.12 — the readme is a byte copy of python's.
#[test]
fn readme_is_the_python_readme() {
    let py = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../python/bundled_agents/text_file/src/text_file/readme.md"
    ))
    .expect("python readme.md");
    assert_eq!(README, py);
}

const PY_TOOLS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../python/bundled_agents/text_file/src/text_file/tools.py"
);

/// C37: THE LEG IO SEAM RIDES THE BATCH. Every chunk this agent moves is an
/// ordinary execution on the leg — a `call` row with the editor as its origin,
/// a ledger id, a settle. `send_with_binary` minted none of that, so the bytes
/// a model wrote left no record of who asked or of how it ended, and a
/// checkpoint id named a task that was never in the table.
#[tokio::test]
async fn leg_calls_are_batch_rows() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    seed(&k, &ed, "doc.md", "one\n").await;
    let r = call(&k, &ed, json!({"type": "read", "path": "doc.md"})).await;
    assert_eq!(r["lines"][0]["text"], "one", "{r}");

    let rows = k.rows.for_agent(&AgentId::from("store"));
    let stream_calls: Vec<_> = rows
        .iter()
        .filter(|r| {
            r["kind"] == json!("call")
                && matches!(
                    r["verb"].as_str(),
                    Some("read_stream") | Some("write_stream") | Some("rename")
                )
        })
        .collect();
    assert!(
        !stream_calls.is_empty(),
        "no leg call rows at all: {rows:?}"
    );
    for row in &stream_calls {
        assert_eq!(
            row["origin"],
            json!("ed"),
            "a leg call must be attributed to the editor: {row:?}"
        );
    }
    // Every one of them ENDED — a settle row on the root names its id.
    let settles: Vec<i64> = k
        .rows
        .for_agent(&AgentId::from("kernel_state"))
        .iter()
        .filter(|r| r["kind"] == json!("settle"))
        .filter_map(|r| r["parent"].as_i64())
        .collect();
    for row in &stream_calls {
        let id = row["id"].as_i64().expect("a call row is named by its id");
        assert!(settles.contains(&id), "leg call {id} never settled");
    }
}

/// C37, the other half: the reply BODY comes back through the batch, so an
/// EMPTY file reads as an empty document rather than as a failure. The carrier
/// is `Some(vec![])`, not "no body" — testing the length instead made a 0-byte
/// read indistinguishable from a refusal.
#[tokio::test]
async fn an_empty_document_reads_back_empty() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let w = call_bin(&k, &ed, json!({"type": "write", "path": "empty.md"}), b"").await;
    assert_eq!(w["size"], 0, "{w}");
    let r = call(&k, &ed, json!({"type": "read", "path": "empty.md"})).await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["total"], 0, "{r}");
    assert_eq!(r["lines"], json!([]), "{r}");
}
