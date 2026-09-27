//! Unit tests for this bundle crate.

use super::*;
use tempfile::TempDir;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("This is a Fantastic kernel"));
    assert!(README.contains("send(target_id, payload)"));
}

#[test]
fn seed_root_readme_writes_when_missing() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let path = tmp.path().join(".fantastic/readme.md");
    assert!(path.exists());
    let read = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read, README);
}

#[test]
fn seed_root_readme_is_idempotent_and_preserves_edits() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let path = tmp.path().join(".fantastic/readme.md");
    std::fs::write(&path, "USER EDIT").unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let read = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read, "USER EDIT");
}

#[test]
fn docs_tree_present_keyed_and_descriptored() {
    assert!(!DOCS.is_empty());
    assert!(DOCS.iter().any(|(k, _)| *k == "self-extension"));
    // A12.2 — `minds` is not a topic on python, so it is not one here.
    assert!(!DOCS.iter().any(|(k, _)| *k == "minds"), "minds is gone");
    let mut keys: Vec<_> = DOCS.iter().map(|(k, _)| *k).collect();
    let n = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), n, "duplicate doc key");
    for (k, body) in DOCS {
        assert!(!body.is_empty(), "empty doc {k}");
        assert!(
            body.starts_with("> "),
            "doc {k} must open with a `> ` descriptor line"
        );
    }
}

#[test]
fn seed_root_readme_writes_docs_tree() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let se = tmp.path().join(".fantastic/docs/self-extension.md");
    assert!(se.exists(), "docs/self-extension.md not seeded");
    let want = DOCS.iter().find(|(k, _)| *k == "self-extension").unwrap().1;
    assert_eq!(std::fs::read_to_string(&se).unwrap(), want);
}

/// A12.1 + A12.2 + A12.3 — the keyed doc tree IS python's: the same keys (no
/// more `minds`, and the six topics rust never had), and every body byte for
/// byte. The rust index was 9 top-level entries against python's 12.
#[test]
fn docs_equal_python_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/loader/kernel_state/src/kernel_state");
    let docs = py.join("docs");

    // Every `.md` under python's docs/, keyed the way a nested tier is keyed.
    let mut want: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&docs).expect("python docs/") {
        let path = entry.expect("dir entry").path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.is_dir() {
            for sub in std::fs::read_dir(&path).expect("nested tier") {
                let sub = sub.expect("dir entry").path();
                if sub.extension().and_then(|e| e.to_str()) == Some("md") {
                    let leaf = sub.file_stem().and_then(|s| s.to_str()).expect("leaf");
                    want.push(format!("{stem}/{leaf}"));
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            want.push(stem.to_string());
        }
    }
    want.sort();
    let mut got: Vec<String> = DOCS.iter().map(|(k, _)| (*k).to_string()).collect();
    got.sort();
    assert_eq!(got, want, "the doc key set must be python's");

    for (key, body) in DOCS {
        let path = docs.join(format!("{key}.md"));
        assert_eq!(
            *body,
            std::fs::read_to_string(&path).expect("python doc"),
            "{key} has drifted from {}",
            path.display()
        );
    }
}

/// A12.4 — the root orientation readme is python's, byte for byte (rust was
/// missing the whole `kernel_verbs` paragraph).
#[test]
fn root_readme_is_pythons_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/loader/kernel_state/src/kernel_state/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
    assert!(README.contains("kernel_verbs"));
}

/// A12.1 — a nested tier seeds into its own directory, so `reflect
/// readme=frontend/routing` has a file to read.
#[test]
fn seed_root_readme_writes_nested_tiers() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let docs = tmp.path().join(".fantastic/docs");
    for key in [
        "frontend",
        "frontend/panel-recipe",
        "frontend/routing",
        "orchestration",
        "system-verbs",
        "terminal",
    ] {
        let path = docs.join(format!("{key}.md"));
        assert!(path.exists(), "{key} not seeded");
        let want = DOCS.iter().find(|(k, _)| k == &key).unwrap().1;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want);
    }
    assert!(!docs.join("minds.md").exists(), "minds is gone");
}

// ─── the kernel_state LOADER: the root's own bundle ──────────────────
//
// C12 moved the root frame (`persistence`, `guid`, `readmes`, `docs`,
// `self_extension`) out of the kernel and into this bundle's
// `reflect_root_extra`, so the texts and their tests live together here.

use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::{BundleRegistry, Item, Kernel};
use serde_json::json;
use std::sync::Arc;

/// A disk kernel at `tmp` whose root is the real `kernel_state` loader.
fn booted(tmp: &TempDir) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, KernelStateBundle::default());
    bootstrap(reg, BootstrapOptions::daemon(tmp.path()))
        .expect("boot")
        .kernel
}

async fn root_reflect(kernel: &Arc<Kernel>, payload: serde_json::Value) -> serde_json::Value {
    let root = kernel.root().expect("root").id.clone();
    kernel
        .send(vec![Item::new(root, payload)])
        .await
        .remove(0)
        .result
}

/// C12: the ROOT answers `reflect` with the substrate identity even though it
/// carries a handler_module, and the loader's frame is MERGED onto it.
#[tokio::test]
async fn root_with_handler_reflects_identity() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let v = root_reflect(&kernel, json!({"type": "reflect", "tree": "none"})).await;
    assert_eq!(v["id"], json!("kernel_state"));
    assert_eq!(v["handler_module"], json!("kernel_state.tools"));
    assert_eq!(
        v["sentence"],
        json!("Fantastic kernel. Everything is reachable by sending messages to agents.")
    );
    // The loader's contribution, python's distilled set.
    assert!(v["persistence"].is_object());
    assert!(v.get("self_extension").is_some());
    // DISTILLED carries no doc frame — `more` names the manual instead.
    assert!(v.get("readmes").is_none());
    assert!(v.get("docs").is_none());
}

/// C9 (merged into C12): the FULL frame is python's key order —
/// persistence, guid, readmes, docs, self_extension — appended after
/// runtime/env/version.
#[tokio::test]
async fn full_root_frame_key_order_is_pythons() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    seed_root_readme(tmp.path()).unwrap();
    let v = root_reflect(
        &kernel,
        json!({"type": "reflect", "detail": "full", "tree": "none"}),
    )
    .await;
    let keys: Vec<&str> = v
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .filter(|k| {
            matches!(
                *k,
                "runtime"
                    | "env"
                    | "version"
                    | "persistence"
                    | "guid"
                    | "readmes"
                    | "docs"
                    | "self_extension"
            )
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            "runtime",
            "env",
            "version",
            "persistence",
            "guid",
            "readmes",
            "docs",
            "self_extension"
        ],
        "{keys:?}"
    );
    assert!(v["readmes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["key"] == "self-extension"));
    assert!(v["docs"].as_str().unwrap().contains("keyed doc tree"));
}

/// A doc that will not decode takes the WHOLE root frame with it, exactly as
/// python does: `_readme_index` raises out of `reflect_root_extra` and the
/// substrate's merge site swallows the exception, so a `detail:"full"` reflect
/// carries none of persistence/guid/readmes/docs/self_extension. Degrading to
/// an empty `readmes` list published a frame python never sends (measured in
/// the acceptance capture: `store/s2/root/detail_full_tree_ids`).
#[tokio::test]
async fn full_root_frame_is_dropped_whole_when_a_doc_will_not_decode() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let docs = kernel.root().expect("root").root_path.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("bad.md"), b"# head\n\xff\xfe not utf-8\n").unwrap();
    let v = root_reflect(
        &kernel,
        json!({"type": "reflect", "detail": "full", "tree": "none"}),
    )
    .await;
    for k in ["persistence", "guid", "readmes", "docs", "self_extension"] {
        assert!(v.get(k).is_none(), "{k} survived: {v}");
    }
    // The substrate's own identity is untouched — only the loader's frame goes.
    assert_eq!(v["id"], json!("kernel_state"));
    assert_eq!(v["runtime"], json!("rust"));
}

/// C9: the FULL `self_extension` is python's text VERBATIM — and it ends at
/// `work as before.`, with no rust-only fd-wire tail sentence.
#[tokio::test]
async fn full_self_extension_is_python_verbatim() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let v = root_reflect(
        &kernel,
        json!({"type": "reflect", "detail": "full", "tree": "none"}),
    )
    .await;
    let se = v["self_extension"].as_str().unwrap();
    assert_eq!(se, SELF_EXTENSION_FULL);
    assert!(
        se.ends_with("work as before."),
        "the rust-only fd-wire tail must be gone: {}",
        &se[se.len() - 120..]
    );
    assert!(!se.contains("for the fd wire + detail."));
}

/// The SHORT pointer still states the FILE + command model — a model that lacks
/// it writes something that cannot work, and the weak models never follow the
/// pointer to go read the manual.
#[tokio::test]
async fn distilled_self_extension_states_the_file_and_command_model() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let v = root_reflect(&kernel, json!({"type": "reflect", "tree": "none"})).await;
    let se = v["self_extension"].as_str().unwrap();
    assert_eq!(se, SELF_EXTENSION_SHORT);
    assert!(se.contains("no `code` field"));
    assert!(se.contains("write_stream"));
    assert!(se.contains("`bytes` is VERBATIM") && se.contains("</bytes>"));
    assert!(se.contains("exec_bridge.tools"));
    for dead in [
        "code:'",
        "code=",
        "interpreters:",
        "shim:",
        "handler_module:'",
    ] {
        assert!(!se.contains(dead), "withdrawn recipe {dead:?}: {se}");
    }
    assert!(!se.contains("send_binary") && !se.contains("Begin Bytes"));
    assert!(!se.contains("Executors here") && !se.contains("NO script executor"));
}

/// C12: an unknown verb on the loader is python's CODED refusal, verbatim.
#[tokio::test]
async fn unknown_type_refusal_is_verbatim() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let v = root_reflect(&kernel, json!({"type": "wat"})).await;
    assert_eq!(v["error"], json!("kernel_state: unknown type 'wat'"));
    assert_eq!(v["reason"], json!("unhandled"));
    assert_eq!(v["verb"], json!("wat"));
}

/// C12: `load_tree` reads the subtree back and `persist_record` writes one
/// agent.yaml — the explicit verb contract a session/remote loader uses.
#[tokio::test]
async fn load_tree_persist_record_round_trip() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let wrote = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "persist_record", "record": {
                "id": "noted", "handler_module": "text_file.tools", "parent_id": "kernel_state"
            }}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(wrote["ok"], json!(true));
    assert!(tmp
        .path()
        .join(".fantastic/agents/noted/agent.yaml")
        .exists());

    let tree = kernel
        .send(vec![Item::new(root.clone(), json!({"type": "load_tree"}))])
        .await
        .remove(0)
        .result;
    let recs = tree["records"].as_array().expect("records");
    assert_eq!(recs[0]["id"], json!("kernel_state"));
    assert!(recs
        .iter()
        .any(|r| r["id"] == json!("noted") && r["parent_id"] == json!("kernel_state")));

    // `forget_record` removes the dir again.
    let gone = kernel
        .send(vec![Item::new(
            root,
            json!({"type": "forget_record", "id": "noted"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(gone["ok"], json!(true));
    assert!(!tmp.path().join(".fantastic/agents/noted").exists());
}

/// C13: the blob verbs are the LOADER's now — a bare kernel verb set no longer
/// answers them, and the loader's refusals are python's bare `{error}` shape.
#[tokio::test]
async fn blob_verbs_are_the_loaders_and_refuse_like_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let v = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "persist_blob", "name": "x.yaml", "content": "a"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["error"], json!("persist_blob: agent_id (str) required"));
    assert!(v.get("reason").is_none(), "python's bare error: {v}");

    let v = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "persist_blob", "agent_id": "kernel_state", "name": "agent.yaml", "content": "a"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        v["error"],
        json!("persist_blob: name 'agent.yaml' is reserved (kernel-managed)")
    );

    let v = kernel
        .send(vec![Item::new(
            root,
            json!({"type": "load_blob", "agent_id": "kernel_state", "name": "x.yaml"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        v["error"],
        json!("load_blob: no store wired — persistence not available")
    );
}

/// The catalog NAME is python's entry-point name (`kernel_state`), not the
/// module path: `reflect bundles=all|ids`, its `bundles_next` cursor and the AI
/// prompt all carry it. It used to read `kernel_state.tools`.
#[test]
fn the_catalog_name_is_pythons_entry_point_name() {
    use fantastic_kernel::Bundle;
    assert_eq!(KernelStateBundle::default().name(), "kernel_state");
    assert_eq!(HANDLER_MODULE, "kernel_state.tools");
}

/// Build a host agent + a SESSION loader (a `kernel_state` child with a `root`
/// meta) on `kernel`, the shape a web canvas session opens.
async fn open_session(kernel: &Arc<Kernel>, sess_root: &std::path::Path) {
    let root = kernel.root().expect("root").id.clone();
    for (id, parent, extra) in [
        ("web", root.0.as_str(), json!({})),
        ("sess", "web", json!({"root": sess_root.to_string_lossy()})),
    ] {
        let mut payload = json!({
            "type": "create_agent", "parent_id": parent,
            "handler_module": HANDLER_MODULE, "id": id, "watch": false,
        });
        for (k, v) in extra.as_object().unwrap() {
            payload[k] = v.clone();
        }
        let r = kernel
            .send(vec![Item::new(root.clone(), payload)])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], json!(id), "{r}");
        kernel
            .send(vec![Item::new(AgentId::from(id), json!({"type": "boot"}))])
            .await;
    }
}

/// §6–7 (rust-only fixes, python `_resolve_path` + `_boot`): a session loader
/// SEEDS its anchor `<root>/agent.yaml` at boot, a record it persists nests
/// directly under its own root (the parent chain stops at the LOADER, not at the
/// kernel root through `web`), and after a restart `load_tree` returns it.
#[tokio::test]
async fn a_session_loader_persists_a_canvas_that_survives_a_restart() {
    let tmp = TempDir::new().unwrap();
    let sess_root = tmp.path().join(".fantastic/web/s1");

    // ── life 1 ─────────────────────────────────────────────────────────
    let kernel = booted(&tmp);
    open_session(&kernel, &sess_root).await;
    let anchor = sess_root.join("agent.yaml");
    assert!(anchor.is_file(), "boot seeds the session anchor");
    assert!(std::fs::read_to_string(&anchor)
        .unwrap()
        .starts_with("id: sess\n"));
    for (id, parent) in [("canvas", "sess"), ("term1", "canvas")] {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("sess"),
                json!({"type": "persist_record", "record": {
                    "id": id, "handler_module": "canvas.tools", "parent_id": parent,
                }}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["ok"], json!(true), "{r}");
    }
    let canvas = sess_root.join("agents/canvas/agent.yaml");
    assert!(
        canvas.is_file(),
        "under the LOADER's root, not the host chain"
    );
    assert!(sess_root
        .join("agents/canvas/agents/term1/agent.yaml")
        .is_file());
    assert!(
        !sess_root.join("agents/web").exists(),
        "the host chain is not joined in"
    );
    kernel.shutdown().await;
    fantastic_kernel::bootstrap::shutdown(tmp.path()).unwrap();
    drop(kernel);

    // ── life 2: a fresh kernel, the session re-opened on the same root ────
    let kernel = booted(&tmp);
    open_session(&kernel, &sess_root).await;
    let tree = kernel
        .send(vec![Item::new(
            AgentId::from("sess"),
            json!({"type": "load_tree"}),
        )])
        .await
        .remove(0)
        .result;
    let recs = tree["records"].as_array().expect("records");
    let ids: Vec<&str> = recs.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, ["sess", "canvas", "term1"], "{tree}");
    assert_eq!(recs[1]["parent_id"], json!("sess"));
    assert_eq!(recs[2]["parent_id"], json!("canvas"));
    kernel.shutdown().await;
    fantastic_kernel::bootstrap::shutdown(tmp.path()).unwrap();
}

/// Records nested under a federated record (python `test_nested_records.py`).
#[path = "tests_nested_records.rs"]
mod nested_records;
