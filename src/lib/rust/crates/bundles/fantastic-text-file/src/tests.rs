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
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
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
        Some(AgentId::from("core")),
        store_meta,
        store_root.clone(),
        false,
    );
    let _ = kernel.register(Arc::clone(&store));
    let mut meta = Map::new();
    meta.insert("target_id".to_string(), json!("store"));
    meta.insert("can_call".to_string(), json!({"store": "*"}));
    let ed = Agent::new(
        AgentId::from("ed"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("core")),
        meta,
        tmp.path().join("ed"),
        false,
    );
    let _ = kernel.register(Arc::clone(&ed));
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
async fn call_bin(kernel: &Arc<Kernel>, ed: &AgentId, header: Value, blob: &[u8]) -> Value {
    kernel.send_with_binary(ed, header, blob.to_vec()).await.0
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
    let ck = w["ckpt_id"].as_str().expect("ckpt_id").to_string();
    // A checkpoint id is the dispatch's pid rendered — a number now, or `op`
    // when the write came from an operator with no task of its own.
    assert!(
        ck == "op" || ck.parse::<i64>().is_ok(),
        "task pid shape: {ck}"
    );
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
    let ck = w["ckpt_id"].as_str().expect("ckpt_id").to_string();
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
    let ck = c["ckpt_id"].as_str().expect("ckpt_id").to_string();
    call_bin(
        &k,
        &ed,
        json!({"type": "patch", "edits": [{"path": "p.txt", "hunks": 1}]}),
        block("a = 1", "a = 2").as_bytes(),
    )
    .await;
    let ls = call(&k, &ed, json!({"type": "checkpoints", "path": "p.txt"})).await;
    let ids: Vec<&str> = ls["ckpts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [ck.as_str()], "{ls}");
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
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let ed = Agent::new(
        AgentId::from("bare"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("core")),
        Map::new(),
        tmp.path().join("bare"),
        false,
    );
    let _ = kernel.register(Arc::clone(&ed));
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
    for verb in ["write", "patch", "append"] {
        let r = call(&k, &ed, json!({"type": verb, "path": "x"})).await;
        assert_eq!(r["reason"], "missing_arg", "{verb}: {r}");
        assert!(r["arg"].as_str().unwrap().starts_with("bytes"), "{r}");
    }
}

#[tokio::test]
async fn unknown_type_carries_verb() {
    let tmp = TempDir::new().unwrap();
    let (k, ed) = mk(&tmp).await;
    let r = call(&k, &ed, json!({"type": "frobnicate"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "frobnicate", "{r}");
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
    let ck = w["ckpt_id"].as_str().expect("ckpt_id").to_string();
    assert_eq!(on_disk(&tmp, "c.txt"), "after\n");

    // The model writes the id it was shown; the notation makes it an int when
    // it looks like one.
    let numeric: Value = ck.parse::<i64>().map(Value::from).unwrap_or(json!(ck));
    let r = call(
        &k,
        &ed,
        json!({"type": "restore", "path": "c.txt", "ckpt_id": numeric}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(on_disk(&tmp, "c.txt"), "before\n");
}
