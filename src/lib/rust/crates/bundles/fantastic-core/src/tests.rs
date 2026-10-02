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
                "id": "noted", "handler_module": "file_bridge.tools", "parent_id": "kernel_state"
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

/// a15 (audit 2026-09-28): the ROOT loader's tree is hydrated at boot, so no
/// AGENT writes it — not a sibling's record, not its own. The operator still does.
#[tokio::test]
async fn the_root_loader_refuses_record_verbs_from_an_agent() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    // NAMED grants, so the LOADER's own guard is what answers, not the gate.
    let made = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "someone",
                   "handler_module": "file_bridge.tools",
                   "can_call": {root.0.clone(): ["persist_record", "forget_record"]}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!("someone"), "{made}");
    for payload in [
        json!({"type": "persist_record", "record": {"id": "noted", "parent_id": "kernel_state"}}),
        json!({"type": "forget_record", "id": "noted"}),
    ] {
        let out = fantastic_kernel::send::with_sender(AgentId::from("someone"), {
            let kernel = Arc::clone(&kernel);
            let root = root.clone();
            async move {
                kernel
                    .send(vec![Item::new(root, payload)])
                    .await
                    .remove(0)
                    .result
            }
        })
        .await;
        assert_eq!(out["reason"], json!("unauthorized"), "{out}");
        assert!(
            out["error"]
                .as_str()
                .unwrap_or("")
                .contains("only by the operator"),
            "{out}"
        );
    }
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

// ─── loud persistence (audit 2026-09-28, d2) ───────────────────────────

/// A kernel with the root loader AND the real file_bridge, NOT yet booted.
fn with_file(tmp: &TempDir) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, KernelStateBundle::default());
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    bootstrap(reg, BootstrapOptions::daemon(tmp.path()))
        .expect("boot")
        .kernel
}

async fn make_store(kernel: &Arc<Kernel>, extra: serde_json::Value) {
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel", "id": "store",
        "handler_module": "file_bridge.tools", "root": ".fantastic",
        "allow": ["."], "ingress_rule": "allow_all_ingress",
    });
    for (k, v) in extra.as_object().unwrap() {
        create[k] = v.clone();
    }
    let r = root_reflect(kernel, create).await;
    assert_eq!(r["id"], json!("store"), "{r}");
}

async fn boot_root(kernel: &Arc<Kernel>) {
    let root = kernel.root().expect("root").id.clone();
    kernel
        .send(vec![Item::new(root, json!({"type": "boot"}))])
        .await;
}

async fn persistence_of(kernel: &Arc<Kernel>) -> serde_json::Value {
    root_reflect(kernel, json!({"type": "reflect", "tree": "none"})).await["persistence"].clone()
}

/// A discovered store whose `allow` does not expose the records is reported at
/// boot: no provider, and `persistence.error` names the store and the paths.
#[tokio::test]
async fn a_store_that_does_not_expose_the_records_is_loud_at_boot() {
    let tmp = TempDir::new().unwrap();
    let kernel = with_file(&tmp);
    make_store(&kernel, json!({"allow": ["data"]})).await;
    boot_root(&kernel).await;
    let p = persistence_of(&kernel).await;
    assert_eq!(p["provider"], serde_json::Value::Null, "{p}");
    assert_eq!(p["error"]["store"], json!("store"), "{p}");
    assert!(
        p["error"]["path"].as_str().unwrap().contains("agent.yaml"),
        "{p}"
    );
}

/// A refused write is loud, and clears once a write lands again.
#[tokio::test]
async fn a_refused_write_is_loud_and_clears_when_writes_land() {
    let tmp = TempDir::new().unwrap();
    let kernel = with_file(&tmp);
    make_store(&kernel, json!({"readonly": true})).await;
    boot_root(&kernel).await;
    assert_eq!(persistence_of(&kernel).await, json!({"provider": "store"}));
    let made = root_reflect(
        &kernel,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "probe",
               "handler_module": "file_bridge.tools", "allow": ["."]}),
    )
    .await;
    assert_eq!(made["id"], json!("probe"), "{made}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let p = persistence_of(&kernel).await;
    assert_eq!(p["provider"], serde_json::Value::Null, "{p}");
    assert_eq!(p["error"]["store"], json!("store"), "{p}");
    assert!(
        p["error"]["error"].as_str().unwrap().contains("readonly"),
        "{p}"
    );
    // the operator fixes the store; the next write (its own updated record)
    // lands and the error clears
    let mut patch = Map::new();
    patch.insert("readonly".to_string(), json!(false));
    kernel.update(&AgentId::from("store"), patch);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(persistence_of(&kernel).await, json!({"provider": "store"}));
    assert!(tmp
        .path()
        .join(".fantastic/agents/store/agent.yaml")
        .exists());
}

/// python `fs.Root(workdir, _root(agent))` on every loader record verb: a
/// session loader whose `root` lies OUTSIDE the running dir raises the clamp's
/// ValueError — boot (the anchor write), `load_tree`, a `persist_record` of a
/// record that is not live and `forget_record` alike — and nothing is written or
/// removed out there. A relative `root` resolves under the running dir, never
/// the process cwd.
#[tokio::test]
async fn loader_record_verbs_are_clamped_to_the_running_dir() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let out_root = outside.path().to_string_lossy().into_owned();
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "far",
                   "handler_module": HANDLER_MODULE, "watch": false, "root": out_root}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "far", "{r}");
    let escape = format!(
        "ValueError: fs: root {} escapes the running dir",
        fantastic_bundle::py_repr(&out_root)
    );
    for payload in [
        json!({"type": "boot"}),
        json!({"type": "load_tree"}),
        json!({"type": "persist_record", "record": {"id": "ghost", "parent_id": "far"}}),
        json!({"type": "forget_record", "id": "ghost"}),
    ] {
        let out = kernel
            .send(vec![Item::new(AgentId::from("far"), payload.clone())])
            .await
            .remove(0);
        assert_eq!(
            out.status,
            fantastic_kernel::Status::Failed,
            "{payload}: {out:?}"
        );
        assert_eq!(out.result["error"], json!(escape), "{payload}");
    }
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing may land outside the running dir"
    );

    // A RELATIVE root resolves under the running dir.
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "near",
                   "handler_module": HANDLER_MODULE, "watch": false, "root": ".fantastic/near"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "near", "{r}");
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("near"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0);
    assert_ne!(out.status, fantastic_kernel::Status::Failed, "{out:?}");
    assert!(tmp.path().join(".fantastic/near/agent.yaml").is_file());
    // python's reflect reports `str(_root(agent))` — the root as written.
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from("near"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["root"], ".fantastic/near");
}

/// python `write_record` seeds the bundle's `readme.md` and `docs/` tree beside
/// the record (copy-if-missing), and a write that fails RAISES.
#[tokio::test]
async fn persist_record_seeds_readme_and_docs_and_raises_on_a_failed_write() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "persist_record", "record": {
                "id": "seeded", "handler_module": HANDLER_MODULE, "parent_id": "kernel_state"
            }}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["ok"], json!(true), "{r}");
    let dir = tmp.path().join(".fantastic/agents/seeded");
    assert_eq!(
        std::fs::read_to_string(dir.join("readme.md")).unwrap(),
        README
    );
    assert!(dir.join("docs/self-extension.md").is_file());

    // `agent.yaml` is a DIRECTORY: the atomic rename onto it fails — raised.
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/blocked/agent.yaml/x")).unwrap();
    let out = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "persist_record", "record": {"id": "blocked", "parent_id": "kernel_state"}}),
        )])
        .await
        .remove(0);
    assert_eq!(out.status, fantastic_kernel::Status::Failed, "{out:?}");
    assert!(
        out.result["error"].as_str().unwrap().contains("Errno"),
        "{out:?}"
    );
}

/// python `read_tree`: A MISSING OR CORRUPT ROOT RECORD DOES NOT HIDE THE TREE —
/// the children under `agents/` come back parented to `kernel_state`.
#[tokio::test]
async fn load_tree_without_an_anchor_still_returns_the_children() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let sess = tmp.path().join(".fantastic/web/s9");
    std::fs::create_dir_all(sess.join("agents/canvas")).unwrap();
    std::fs::write(sess.join("agents/canvas/agent.yaml"), "id: canvas\n").unwrap();
    std::fs::write(sess.join("agent.yaml"), "just a scalar\n").unwrap();
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "s9",
                   "handler_module": HANDLER_MODULE, "watch": false,
                   "root": sess.to_string_lossy()}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "s9", "{r}");
    let tree = kernel
        .send(vec![Item::new(
            AgentId::from("s9"),
            json!({"type": "load_tree"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        tree["records"],
        json!([{"id": "canvas", "parent_id": "kernel_state"}]),
        "{tree}"
    );
}

/// python: `persist_record` runs `_guard_parent_chain` on a non-None
/// `parent_id` (a non-string is `bad_arg`), and `forget_record` with a truthy
/// non-string id is the id guard's `bad_arg`, not `missing_arg`.
#[tokio::test]
async fn loader_verbs_refuse_non_string_ids_like_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    for payload in [
        json!({"type": "persist_record", "record": {"id": "x1", "parent_id": 5}}),
        json!({"type": "forget_record", "id": 5}),
    ] {
        let r = kernel
            .send(vec![Item::new(root.clone(), payload.clone())])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "bad_arg", "{payload}: {r}");
        assert_eq!(r["arg"], "id");
    }
    assert!(!tmp.path().join(".fantastic/agents/x1").exists());
}

/// python `_find_store(agent)` searches THE ADDRESSED LOADER's subtree for a
/// store rooted at ITS `_root(agent)`: a session loader with no store of its
/// own answers the blob verbs "no store wired", and a watching session loader
/// persists nothing through the ROOT's store — every record is written once.
#[tokio::test]
async fn a_session_loader_never_writes_through_the_roots_store() {
    let tmp = TempDir::new().unwrap();
    let kernel = with_file(&tmp);
    boot_root(&kernel).await;
    make_store(&kernel, json!({})).await;
    let root = kernel.root().expect("root").id.clone();
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "sess",
                   "handler_module": HANDLER_MODULE, "root": ".fantastic/web/s1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "sess", "{r}");
    kernel
        .send(vec![Item::new(
            AgentId::from("sess"),
            json!({"type": "boot"}),
        )])
        .await;
    // The blob verbs: the session loader has no store; the root has one.
    let blob =
        json!({"type": "persist_blob", "agent_id": "sess", "name": "n.yaml", "content": "x"});
    let r = kernel
        .send(vec![Item::new(AgentId::from("sess"), blob.clone())])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"], "persist_blob: no store wired — persistence not available",
        "{r}"
    );
    let r = kernel
        .send(vec![Item::new(root.clone(), blob)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["ok"], true, "{r}");
    // One record write per created agent — the session loader's writer found
    // no store of its own and wrote nothing.
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "plain",
                   "handler_module": "file_bridge.tools"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "plain", "{r}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let writes = fantastic_kernel::test_support::rows_about(&kernel, &AgentId::from("store"))
        .into_iter()
        .filter(|row| {
            row.get("kind") == Some(&json!("call"))
                && row["payload"]["type"] == "write_stream"
                && row["payload"]["path"]
                    .as_str()
                    .is_some_and(|p| p.starts_with("agents/plain/agent.yaml.tmp."))
        })
        .count();
    assert_eq!(writes, 1, "the record was written by more than one loader");
}

/// python's loader reflect is `str(_root(agent))`: with no `root` meta that is
/// the agent's own address RELATIVE to the running dir.
#[tokio::test]
async fn a_loader_without_a_root_reflects_its_relative_address() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "inner",
                   "handler_module": HANDLER_MODULE, "watch": false}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "inner", "{r}");
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from("inner"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["root"], ".fantastic/agents/inner");
}

/// python `str(_root(agent))` = `str(Path(r))` for a `root` meta: an ABSOLUTE
/// meta is reported absolute — even one inside the running dir — normalized the
/// way `Path` prints it (no `.` segment, no doubled or trailing separator).
#[tokio::test]
async fn a_loader_with_an_absolute_root_meta_reflects_it_as_given() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let base = tmp.path().to_string_lossy().into_owned();
    let given = format!("{base}/./.fantastic//abs/");
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "abs",
                   "handler_module": HANDLER_MODULE, "watch": false, "root": given}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "abs", "{r}");
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from("abs"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["root"], json!(format!("{base}/.fantastic/abs")));
}

/// With no `root` meta the reflect shows the agent's address relative to the
/// running dir; a kernel WITHOUT a running dir has none to show, and the
/// reflect RAISES (python's `RuntimeError` text) instead of leaking a raw path.
#[tokio::test]
async fn a_loader_without_a_root_in_a_kernel_with_no_running_dir_raises() {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, KernelStateBundle::default());
    let kernel = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    let root = kernel.root().expect("root").id.clone();
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "inner",
                   "handler_module": HANDLER_MODULE, "watch": false}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "inner", "{r}");
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("inner"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0);
    assert_eq!(out.status, fantastic_kernel::Status::Failed, "{out:?}");
    assert_eq!(
        out.result["error"],
        json!(
            "RuntimeError: the kernel root has no disk address — no running dir to clamp disk to"
        ),
        "{out:?}"
    );

    // A `root` meta needs no running dir: it is reported as given.
    let r = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": root.0, "id": "absmem",
                   "handler_module": HANDLER_MODULE, "watch": false, "root": "/srv/state"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "absmem", "{r}");
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from("absmem"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["root"], "/srv/state");
}

/// STEP-4 PIN (2026-10-01): the loader's unknown type, as TEXT — key order is
/// part of the wire (`preserve_order`) and `Value` equality ignores it.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    for (verb, want) in [
        (
            "wat",
            r#"{"error":"kernel_state: unknown type 'wat'","reason":"unhandled","verb":"wat"}"#,
        ),
        (
            "shutdown",
            r#"{"error":"kernel_state: unknown type 'shutdown'","reason":"unhandled","verb":"shutdown"}"#,
        ),
        (
            "it's",
            r#"{"error":"kernel_state: unknown type \"it's\"","reason":"unhandled","verb":"it's"}"#,
        ),
    ] {
        let v = root_reflect(&kernel, json!({"type": verb})).await;
        assert_eq!(serde_json::to_string(&v).unwrap(), want, "{verb}");
    }
}

/// The handler's `verb` echoes the RAW `type` — python `verb=t`: null when it is
/// absent, the number for `5` — never the empty string the dispatch matched on.
/// `send` refuses those payloads before any handler runs (python measured: `send:
/// payload.type required` / `must be a non-empty string`), so the handler is
/// called directly, as a bundle that forwards a payload would.
#[tokio::test]
async fn an_unknown_type_echoes_the_raw_value_as_verb() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    for (payload, want) in [
        (
            json!({}),
            r#"{"error":"kernel_state: unknown type None","reason":"unhandled","verb":null}"#,
        ),
        (
            json!({"type": null}),
            r#"{"error":"kernel_state: unknown type None","reason":"unhandled","verb":null}"#,
        ),
        (
            json!({"type": 5}),
            r#"{"error":"kernel_state: unknown type 5","reason":"unhandled","verb":5}"#,
        ),
        (
            json!({"type": ""}),
            r#"{"error":"kernel_state: unknown type ''","reason":"unhandled","verb":""}"#,
        ),
    ] {
        let r = KernelStateBundle::default()
            .handle(&root, &payload, &kernel)
            .await
            .expect("a refusal, not a raise")
            .expect("a reply");
        assert_eq!(serde_json::to_string(&r).unwrap(), want, "{payload}");
    }
}
