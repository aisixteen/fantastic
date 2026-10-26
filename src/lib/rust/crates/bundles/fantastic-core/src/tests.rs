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
fn docs_tree_present_keyed_and_descriptored() {
    assert!(!DOCS.is_empty());
    assert!(DOCS.iter().any(|(k, _)| *k == "self-extension"));
    // `minds` is not a topic on python, so it is not one here.
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

/// The keyed doc tree IS python's: the same keys (no
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

/// The root orientation readme is python's, byte for byte (rust was
/// missing the whole `kernel_verbs` paragraph).
#[test]
fn root_readme_is_pythons_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/loader/kernel_state/src/kernel_state/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
    assert!(README.contains("type: docs"));
}

/// The nested tiers are served from the compiled table: the index
/// nests them under their parent topic, and nothing is written to disk.
#[test]
fn nested_tiers_index_from_the_table() {
    let index = fantastic_kernel::reflect::readme_index(DOCS, "");
    let frontend = index
        .iter()
        .find(|e| e["key"] == "frontend")
        .expect("frontend topic");
    let children: Vec<&str> = frontend["children"]
        .as_array()
        .expect("frontend tiers")
        .iter()
        .filter_map(|e| e["key"].as_str())
        .collect();
    assert_eq!(children, vec!["frontend/html-agent", "frontend/routing"]);
    assert!(!index.iter().any(|e| e["key"] == "minds"), "minds is gone");
}

// ─── the kernel_state LOADER: the root's own bundle ──────────────────
//
// The root frame (`persistence`, `guid`, `readmes`, `docs`,
// `self_extension`) lives in this bundle's `reflect_root_extra`, not the
// kernel, so the texts and their tests live together here.

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

/// The ROOT answers `reflect` with the substrate identity even though it
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

/// The FULL frame is python's key order —
/// persistence, guid, readmes, docs, self_extension — appended after
/// runtime/env/version.
#[tokio::test]
async fn full_root_frame_key_order_is_pythons() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
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

/// The FULL `self_extension` is python's text VERBATIM — and it ends at
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
    assert!(se.contains("`bytes` verbatim"));
    assert!(se.contains("exec_bridge.tools"));
    assert!(se.contains("can_call") && se.contains("callable_by"));
}

/// An unknown verb on the loader is python's CODED refusal, verbatim.
#[tokio::test]
async fn unknown_type_refusal_is_verbatim() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let v = root_reflect(&kernel, json!({"type": "wat"})).await;
    assert_eq!(v["error"], json!("kernel_state: unknown type 'wat'"));
    assert_eq!(v["reason"], json!("unhandled"));
    assert_eq!(v["verb"], json!("wat"));
}

/// `load_tree` reads the subtree back and `persist_record` writes one
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

/// The ROOT loader's tree is hydrated at boot, so no
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
    fantastic_kernel::test_support::grant(
        &kernel,
        root.as_str(),
        "callable_by",
        "someone",
        json!(["persist_record", "forget_record"]),
    );
    for payload in [
        json!({"type": "persist_record", "record": {"id": "noted", "parent_id": "kernel_state"}}),
        json!({"type": "forget_record", "id": "noted"}),
    ] {
        let out = fantastic_kernel::test_support::as_agent(AgentId::from("someone"), {
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

/// The blob verbs are the LOADER's now — a bare kernel verb set no longer
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
/// prompt all carry it.
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
/// writes NO record of its own under its root, a record it persists nests
/// directly under its own root (the parent chain stops at the LOADER, not at the
/// kernel root through `web`), and after a restart `load_tree` returns it.
#[tokio::test]
async fn a_session_loader_persists_a_canvas_that_survives_a_restart() {
    let tmp = TempDir::new().unwrap();
    let sess_root = tmp.path().join(".fantastic/web/s1");

    // ── life 1 ─────────────────────────────────────────────────────────
    let kernel = booted(&tmp);
    open_session(&kernel, &sess_root).await;
    assert!(
        !sess_root.join("agent.yaml").exists(),
        "the session's record lives in the host tree only"
    );
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

// ─── loud persistence ──────────────────────────────────────────────────

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
    assert!(!tmp.path().join(".fantastic/near/agent.yaml").exists());
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

/// python `write_record` writes the record only — no readme or `docs/` copy —
/// and a write that fails RAISES.
#[tokio::test]
async fn persist_record_writes_the_record_only_and_raises_on_a_failed_write() {
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
    assert!(dir.join("agent.yaml").is_file());
    assert!(!dir.join("readme.md").exists() && !dir.join("docs").exists());

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

/// A session loader's `load_tree` is rooted at its LIVE record — a stale
/// `<root>/agent.yaml` left by an older build is ignored, and the children
/// under `agents/` come back parented to the loader.
#[tokio::test]
async fn load_tree_is_rooted_at_the_live_session_record() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.clone();
    let sess = tmp.path().join(".fantastic/web/s9");
    std::fs::create_dir_all(sess.join("agents/canvas")).unwrap();
    std::fs::write(sess.join("agents/canvas/agent.yaml"), "id: canvas\n").unwrap();
    std::fs::write(sess.join("agent.yaml"), "id: stale\n").unwrap();
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
    let recs = tree["records"].as_array().expect("records");
    assert_eq!(recs.len(), 2, "{tree}");
    assert_eq!(recs[0]["id"], "s9", "{tree}");
    assert_eq!(recs[0]["watch"], false, "the live record: {tree}");
    assert_eq!(
        recs[1],
        json!({"id": "canvas", "parent_id": "s9"}),
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
    // no store of its own and wrote nothing. A store write is kernel bookkeeping
    // (no call row), so it is counted off the store's traffic events.
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&seen);
    kernel.add_state_subscriber(Arc::new(move |ev: &Value, _body| {
        if ev["agent_id"] == "store" && ev["kind"] == "send" {
            sink.lock().unwrap().push(ev["payload"].clone());
        }
    }));
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
    let writes = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|p| {
            p["type"] == "write_stream"
                && p["path"]
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
    let want = fantastic_bundle::as_posix(std::path::Path::new(&format!("{base}/.fantastic/abs")));
    assert_eq!(reflect["root"], json!(want));
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

/// The loader's unknown type, as TEXT — key order is
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
/// `send` refuses those payloads before any handler runs (python: `send:
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

async fn ask(kernel: &Arc<Kernel>, target: &str, payload: serde_json::Value) -> serde_json::Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// python `test_read_bundles_hands_each_folders_bundle_yaml_raw`: a loader hands
/// its OWN root's folder bundles as raw text, one by name too.
#[tokio::test]
async fn read_bundles_hands_each_folders_bundle_yaml_raw() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.0.clone();
    let r = ask(
        &kernel,
        &root,
        json!({"type": "create_agent", "parent_id": root, "id": "web_loader",
               "handler_module": HANDLER_MODULE, "watch": false, "root": ".fantastic/web"}),
    )
    .await;
    assert_eq!(r["id"], "web_loader", "{r}");
    let text = "base: html_agent\nsentence: A clock.\nhtml: index.html  # not parsed here\n";
    let b = tmp.path().join(".fantastic/web/bundles");
    std::fs::create_dir_all(b.join("clock")).unwrap();
    std::fs::write(b.join("clock/bundle.yaml"), text).unwrap();
    std::fs::create_dir_all(b.join("empty")).unwrap();
    std::fs::write(b.join("stray.txt"), "not a folder").unwrap();

    let got = ask(&kernel, "web_loader", json!({"type": "read_bundles"})).await;
    assert_eq!(
        got,
        json!({"bundles": [
            {"name": "clock", "folder": "web/bundles/clock", "text": text},
            {"name": "empty", "folder": "web/bundles/empty",
             "error": "bundles/empty: no bundle.yaml"},
        ]})
    );
    let one = ask(
        &kernel,
        "web_loader",
        json!({"type": "read_bundles", "name": "clock"}),
    )
    .await;
    assert_eq!(one["bundles"], json!([got["bundles"][0]]));
    let own = ask(&kernel, &root, json!({"type": "read_bundles"})).await;
    assert_eq!(own, json!({"bundles": []}));
}

#[tokio::test]
async fn read_bundles_refuses_what_is_no_folder() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().expect("root").id.0.clone();
    for (name, reason) in [
        ("nope", "not_found"),
        ("../x", "not_found"),
        ("", "bad_arg"),
    ] {
        let r = ask(
            &kernel,
            &root,
            json!({"type": "read_bundles", "name": name}),
        )
        .await;
        assert_eq!(r["reason"], reason, "{name}: {r}");
    }
}

// ─── export_subtree on the ROOT (#268) ───────────────────────────────

async fn mk(kernel: &Arc<Kernel>, id: &str, parent: &str) {
    let root = kernel.root().expect("root").id.clone();
    let made = kernel
        .send(vec![Item::new(
            root.clone(),
            json!({"type": "create_agent", "parent_id": parent, "id": id,
                   "handler_module": "noop.tools", "note": id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!(id), "{made}");
}

#[test]
fn root_export_subtree_is_in_the_roots_verbs_and_docs() {
    let v = KernelStateBundle::default().verb_docs();
    let doc = v["export_subtree"]
        .as_str()
        .expect("export_subtree is a verb");
    assert!(doc.contains("agent_id:str (req), path:str (req)"), "{doc}");
    assert!(doc.contains("{exported, path, ports, files}"), "{doc}");
}

#[tokio::test]
async fn root_export_subtree_exports_a_three_level_subtree() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().unwrap().id.0.clone();
    mk(&kernel, "a", &root).await;
    mk(&kernel, "b", "a").await;
    mk(&kernel, "c", "b").await;
    let r = root_reflect(
        &kernel,
        json!({"type": "export_subtree", "agent_id": "a", "path": "out_a"}),
    )
    .await;
    assert_eq!(r["exported"], json!(3), "{r}");
    assert_eq!(r["path"], json!("out_a"), "{r}");
    assert_eq!(r["files"], json!(["blueprint.yaml"]), "{r}");
    let text = std::fs::read_to_string(tmp.path().join("out_a/blueprint.yaml")).unwrap();
    let d: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    assert_eq!(d["agents"][0]["id"], "a");
    assert_eq!(d["agents"][0]["agents"][0]["id"], "b");
    assert_eq!(d["agents"][0]["agents"][0]["agents"][0]["id"], "c");
    assert!(!text.contains("parent_id"), "{text}");
}

#[tokio::test]
async fn root_export_subtree_refuses_the_root_id_and_unknown_id_and_bad_args() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().unwrap().id.0.clone();
    mk(&kernel, "a", &root).await;
    for (p, reason) in [
        (
            json!({"type": "export_subtree", "agent_id": root, "path": "o1"}),
            "bad_arg",
        ),
        (
            json!({"type": "export_subtree", "agent_id": "nope", "path": "o2"}),
            "bad_arg",
        ),
        (
            json!({"type": "export_subtree", "agent_id": "a"}),
            "bad_arg",
        ),
        (json!({"type": "export_subtree", "path": "o3"}), "bad_arg"),
        (
            json!({"type": "export_subtree", "agent_id": "a", "path": "../o4"}),
            "bad_arg",
        ),
    ] {
        let r = root_reflect(&kernel, p.clone()).await;
        assert_eq!(r["reason"], json!(reason), "{p}: {r}");
        assert!(
            r["error"]
                .as_str()
                .unwrap_or("")
                .starts_with("kernel_state.export_subtree:"),
            "{p}: {r}"
        );
    }
}

#[tokio::test]
async fn non_root_loader_refuses_export_subtree() {
    let tmp = TempDir::new().unwrap();
    let kernel = booted(&tmp);
    let root = kernel.root().unwrap().id.0.clone();
    let made = kernel
        .send(vec![Item::new(
            kernel.root().unwrap().id.clone(),
            json!({"type": "create_agent", "parent_id": root, "id": "inner",
                   "handler_module": HANDLER_MODULE}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!("inner"), "{made}");
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("inner"),
            json!({"type": "export_subtree", "agent_id": "inner", "path": "o"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap_or("")
            .contains("kernel_state.export_subtree: the ROOT answers it"),
        "{r}"
    );
    assert!(!tmp.path().join("o").exists());
}

#[test]
fn a_secret_hit_names_the_file_with_slashes_on_every_os() {
    let d = TempDir::new().unwrap();
    std::fs::create_dir_all(d.path().join("bundles").join("py_ab12")).unwrap();
    std::fs::write(
        d.path().join("bundles").join("py_ab12").join("agent.py"),
        "KEY = 'sk-abcd1234efgh'\n",
    )
    .unwrap();
    let p = d.path().join("bundles").join("py_ab12").join("agent.py");
    assert_eq!(export::rel_name(d.path(), &p), "bundles/py_ab12/agent.py");
    assert_eq!(
        export::scan_secrets(d.path()),
        vec!["bundles/py_ab12/agent.py: an sk- key token".to_string()]
    );
}
