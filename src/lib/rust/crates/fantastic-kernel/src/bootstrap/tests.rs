//! Unit tests for [`crate::bootstrap`].
use crate::outcome::Item;

use super::*;
use crate::Bundle;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Register the ROOT LOADER. The root's `handler_module` is
/// `kernel_state.tools`, and its `boot` is what mints the guid, rehydrates the
/// ledger, starts the pump and subscribes the record writer — so a test that
/// wants any of that boots the real bundle, not a second copy of it.
fn with_loader(mut reg: BundleRegistry) -> BundleRegistry {
    crate::test_support::register_root_loader(&mut reg);
    reg
}

use crate::test_support::boot_root;

struct Noop;
#[async_trait]
impl Bundle for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _k: &Arc<Kernel>,
    ) -> Result<crate::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

#[test]
fn bootstrap_creates_root_on_virgin_workdir() {
    let tmp = TempDir::new().unwrap();
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .expect("boot");
    assert!(booted.kernel.root().is_some());
    assert_eq!(booted.kernel.root().unwrap().id.0, "kernel_state");
    assert!(tmp.path().join(".fantastic/agent.yaml").exists());
    assert!(tmp.path().join(".fantastic/lock.yaml").exists());
    shutdown(tmp.path()).unwrap();
}

#[test]
fn bootstrap_hydrates_persisted_children() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Boot once + wire a persistence provider + create a child. Persistence is
    // provider-routed AND debounced through the root loader's record writer now,
    // so the child only lands on disk after the loader booted and a pass ran.
    {
        let mut reg = with_loader(BundleRegistry::new());
        reg.register("noop.tools", Noop);
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
        let kernel = Arc::clone(&booted.kernel);
        rt.block_on(async {
            // THE ROOT BOOTS FIRST (registration order), so the record writer
            // is subscribed before the store agent exists — which is how the
            // store persists ITSELF through itself, as python's does.
            boot_root(&kernel).await;
            crate::test_support::wire_fake_store(&kernel, &store_root).await;
            let _ = kernel
                .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"child_a"}))])
                .await.remove(0).result;
            kernel.flush_records().await;
        });
        shutdown(tmp.path()).unwrap();
    }
    // Boot again — child + store must rehydrate (the store re-discovers itself).
    let mut reg2 = with_loader(BundleRegistry::new());
    reg2.register("noop.tools", Noop);
    crate::test_support::register_fake_store(&mut reg2, &store_root);
    let booted2 = bootstrap(reg2, BootstrapOptions::daemon(tmp.path())).expect("boot2");
    assert!(booted2
        .kernel
        .agents
        .contains_key(&AgentId::from("child_a")));
    // Both the child AND the persisted store provider rehydrate.
    assert!(booted2.loaded.contains(&AgentId::from("child_a")));
    assert!(booted2.loaded.contains(&AgentId::from("store")));
    shutdown(tmp.path()).unwrap();
}

#[test]
fn edit_grants_persists_both_keys_across_a_restart() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let rt = tokio::runtime::Runtime::new().unwrap();
    {
        let mut reg = with_loader(BundleRegistry::new());
        reg.register("noop.tools", Noop);
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
        let kernel = Arc::clone(&booted.kernel);
        rt.block_on(async {
            boot_root(&kernel).await;
            crate::test_support::wire_fake_store(&kernel, &store_root).await;
            let _ = kernel
                .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"child_m"}))])
                .await.remove(0).result;
            let id = AgentId::from("child_m");
            let _ = kernel.edit_callable_by(&id, |cb| {
                cb.insert("view_a".to_string(), json!(["*"]));
                true
            });
            let _ = kernel.edit_callable_by(&id, |cb| {
                cb.insert("view_b".to_string(), json!(["*"]));
                true
            });
            kernel.flush_records().await;
        });
        shutdown(tmp.path()).unwrap();
    }
    let mut reg2 = with_loader(BundleRegistry::new());
    reg2.register("noop.tools", Noop);
    crate::test_support::register_fake_store(&mut reg2, &store_root);
    let booted2 = bootstrap(reg2, BootstrapOptions::daemon(tmp.path())).expect("boot2");
    let cb = booted2
        .kernel
        .agents
        .get(&AgentId::from("child_m"))
        .expect("child_m rehydrated")
        .record()
        .meta
        .get("callable_by")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(cb.get("view_a"), Some(&json!(["*"])), "{cb}");
    assert_eq!(cb.get("view_b"), Some(&json!(["*"])), "{cb}");
    shutdown(tmp.path()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edit_grants_keep_every_key() {
    let tmp = TempDir::new().unwrap();
    let mut reg = with_loader(BundleRegistry::new());
    reg.register("noop.tools", Noop);
    let booted = bootstrap(reg, BootstrapOptions::one_shot(tmp.path())).expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    for round in 0..200 {
        let id = AgentId::from(format!("merge_{round}").as_str());
        kernel.register(Agent::new(
            id.clone(),
            Some("noop.tools".to_string()),
            None,
            Map::new(),
            tmp.path().join(format!("agent_{round}")),
            false,
        ));
        let (a, b) = (Arc::clone(&kernel), Arc::clone(&kernel));
        let (ida, idb) = (id.clone(), id.clone());
        let ta = tokio::spawn(async move {
            a.edit_callable_by(&ida, |cb| {
                cb.insert("view_a".to_string(), json!(["*"]));
                true
            })
        });
        let tb = tokio::spawn(async move {
            b.edit_callable_by(&idb, |cb| {
                cb.insert("view_b".to_string(), json!(["*"]));
                true
            })
        });
        ta.await.unwrap().unwrap();
        tb.await.unwrap().unwrap();
        let cb = kernel
            .agents
            .get(&id)
            .expect("agent exists")
            .record()
            .meta
            .get("callable_by")
            .cloned()
            .unwrap_or(Value::Null);
        assert!(
            cb.get("view_a").is_some() && cb.get("view_b").is_some(),
            "round {round}: {cb}"
        );
    }
    shutdown(tmp.path()).unwrap();
}

#[test]
fn edit_callable_by_holds_the_record_lock_while_it_edits() {
    let kernel = Kernel::new();
    kernel.register(Agent::new(
        AgentId::from("locked"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::from("/nonexistent"),
        false,
    ));
    let agent = kernel
        .agents
        .get(&AgentId::from("locked"))
        .map(|e| Arc::clone(&e))
        .unwrap();
    let mut held = false;
    let r = kernel.edit_callable_by(&AgentId::from("locked"), |cb| {
        // a reader or writer from outside would block here: the record is locked for the edit
        held = agent.meta.try_read().is_err();
        cb.insert("view_a".to_string(), json!(["*"]));
        true
    });
    assert!(r.is_ok(), "{r:?}");
    assert!(held, "the record's lock was free while the edit ran");
}

#[test]
fn an_edit_the_write_time_checks_refuse_writes_nothing() {
    let kernel = Kernel::new();
    kernel.register(Agent::new(
        AgentId::from("guarded"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::from("/nonexistent"),
        false,
    ));
    let before = kernel
        .agents
        .get(&AgentId::from("guarded"))
        .unwrap()
        .record()
        .meta
        .get("callable_by")
        .cloned();
    let refused = kernel.edit_callable_by(&AgentId::from("guarded"), |cb| {
        cb.insert("x".to_string(), json!(5));
        true
    });
    assert!(refused.is_err(), "{refused:?}");
    let cb = kernel
        .agents
        .get(&AgentId::from("guarded"))
        .unwrap()
        .record()
        .meta
        .get("callable_by")
        .cloned();
    assert_eq!(cb, before, "a refused edit writes nothing");
}

#[test]
fn edit_can_call_holds_the_record_lock_while_it_edits() {
    let kernel = Kernel::new();
    kernel.register(Agent::new(
        AgentId::from("cc_locked"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::from("/nonexistent"),
        false,
    ));
    let agent = kernel
        .agents
        .get(&AgentId::from("cc_locked"))
        .map(|e| Arc::clone(&e))
        .unwrap();
    let mut held = false;
    let r = kernel.edit_can_call(&AgentId::from("cc_locked"), |cc| {
        // a reader or writer from outside would block here: the record is locked for the edit
        held = agent.meta.try_read().is_err();
        cc.insert("view_a".to_string(), json!(["*"]));
        true
    });
    assert!(r.is_ok(), "{r:?}");
    assert!(held, "the record's lock was free while the edit ran");
}

#[test]
fn a_refused_can_call_shape_writes_nothing() {
    let kernel = Kernel::new();
    kernel.register(Agent::new(
        AgentId::from("cc_guarded"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::from("/nonexistent"),
        false,
    ));
    let before = kernel
        .agents
        .get(&AgentId::from("cc_guarded"))
        .unwrap()
        .record()
        .meta
        .get("can_call")
        .cloned();
    let refused = kernel.edit_can_call(&AgentId::from("cc_guarded"), |cc| {
        cc.insert("x".to_string(), json!(5));
        true
    });
    assert!(refused.is_err(), "{refused:?}");
    let cc = kernel
        .agents
        .get(&AgentId::from("cc_guarded"))
        .unwrap()
        .record()
        .meta
        .get("can_call")
        .cloned();
    assert_eq!(cc, before, "a refused edit writes nothing");
}

#[test]
fn bootstrap_one_shot_does_not_acquire_lock() {
    let tmp = TempDir::new().unwrap();
    // First boot acquires.
    let _b1 = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .unwrap();
    // One-shot must NOT contend with the held lock.
    let opts = BootstrapOptions::one_shot(tmp.path());
    let booted = bootstrap(with_loader(BundleRegistry::new()), opts).expect("one-shot ok");
    assert!(booted.kernel.root().is_some());
    shutdown(tmp.path()).unwrap();
}

#[test]
fn bootstrap_weak_loads_unknown_handler_modules() {
    let tmp = TempDir::new().unwrap();
    // Plant a ghost agent on disk before booting.
    let ghost = tmp.path().join(".fantastic/agents/ghost_1");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(
        ghost.join("agent.yaml"),
        r#"{"id":"ghost_1","handler_module":"unknown.tools","parent_id":"kernel_state"}"#,
    )
    .unwrap();
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .expect("boot");
    // Skipped — ghost_1 not registered.
    assert!(!booted.kernel.agents.contains_key(&AgentId::from("ghost_1")));
    // Record still on disk for the next runtime.
    assert!(ghost.join("agent.yaml").exists());
    shutdown(tmp.path()).unwrap();
}

/// Helper: pull the root instance guid out of a booted kernel's live root meta.
fn root_guid(booted: &BootedKernel) -> String {
    booted
        .kernel
        .root()
        .expect("root")
        .meta
        .read()
        .unwrap()
        .get("guid")
        .and_then(Value::as_str)
        .expect("root carries a guid")
        .to_string()
}

#[test]
fn boot_mints_a_32hex_guid_and_persists_it_stably() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let rt = tokio::runtime::Runtime::new().unwrap();
    // First boot mints the guid IN THE LOADER'S `boot` and lets the record
    // writer carry it to the store (python: `kernel.update` → `updated` event →
    // write_stream), which is why the store is wired before the root boots.
    let g1 = {
        let mut reg = with_loader(BundleRegistry::new());
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
        let kernel = Arc::clone(&booted.kernel);
        let g = rt.block_on(async {
            crate::test_support::wire_fake_store(&kernel, &store_root).await;
            boot_root(&kernel).await;
            kernel.flush_records().await;
            root_guid(&booted)
        });
        shutdown(tmp.path()).unwrap();
        g
    };
    assert_eq!(g1.len(), 32);
    assert!(g1
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    // Guid is persisted on the root record.
    let raw = std::fs::read_to_string(tmp.path().join(".fantastic/agent.yaml")).unwrap();
    assert!(raw.contains(&g1), "guid must be persisted to agent.yaml");
    // Second boot of the SAME dir reads it back verbatim — minted ONCE.
    let g2 = {
        let mut reg = with_loader(BundleRegistry::new());
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot2");
        let kernel = Arc::clone(&booted.kernel);
        let g = rt.block_on(async {
            boot_root(&kernel).await;
            kernel.flush_records().await;
            root_guid(&booted)
        });
        shutdown(tmp.path()).unwrap();
        g
    };
    assert_eq!(g1, g2, "guid must be identical across boots");
}

#[test]
fn in_memory_boot_mints_an_ephemeral_guid() {
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::in_memory(),
    )
    .expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(boot_root(&kernel));
    let g = root_guid(&booted);
    assert_eq!(g.len(), 32);
    assert!(g
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
}

/// A virgin dir seeds the ROOT the loader owns — id `kernel_state`,
/// handler_module `kernel_state.tools` — and the record written is the
/// CONSTRUCTED root, so its meta is the structural grant baseline in order.
#[test]
fn root_is_the_kernel_state_loader_and_the_seed_is_the_constructed_record() {
    let tmp = TempDir::new().unwrap();
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .expect("boot");
    let root = booted.kernel.root().expect("root");
    assert_eq!(root.id.0, "kernel_state");
    assert_eq!(root.handler_module.as_deref(), Some("kernel_state.tools"));
    let raw = std::fs::read_to_string(tmp.path().join(".fantastic/agent.yaml")).unwrap();
    let rec: Value = crate::yaml_io::load(&raw).unwrap();
    assert_eq!(rec["id"], json!("kernel_state"));
    assert_eq!(rec["handler_module"], json!("kernel_state.tools"));
    let keys: Vec<&str> = rec
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.iter()
            .filter(|k| matches!(**k, "can_call" | "callable_by"))
            .copied()
            .collect::<Vec<_>>(),
        vec!["can_call", "callable_by"],
        "grant baseline in python's order: {keys:?}"
    );
    shutdown(tmp.path()).unwrap();
}

/// The root's meta order is `can_call, callable_by, guid` — the guid is
/// APPENDED by the loader's boot, after the stamped grants.
#[test]
fn root_meta_order_is_grants_then_guid() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let mut reg = with_loader(BundleRegistry::new());
    crate::test_support::register_fake_store(&mut reg, &store_root);
    let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        crate::test_support::wire_fake_store(&kernel, &store_root).await;
        boot_root(&kernel).await;
        kernel.flush_records().await;
    });
    let meta = kernel.root().unwrap().meta.read().unwrap().clone();
    let keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["can_call", "callable_by", "guid"], "{keys:?}");
    shutdown(tmp.path()).unwrap();
}

/// A root record that is MISSING or not JSON reseeds like a virgin dir and
/// STILL hydrates the children under `agents/` (python `read_tree`).
#[test]
fn a_missing_root_record_reseeds_and_keeps_children() {
    let tmp = TempDir::new().unwrap();
    let fdir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(fdir.join("agents/child_a")).unwrap();
    std::fs::write(
        fdir.join("agents/child_a/agent.yaml"),
        r#"{"id":"child_a","handler_module":"noop.tools"}"#,
    )
    .unwrap();
    let mut reg = with_loader(BundleRegistry::new());
    reg.register("noop.tools", Noop);
    let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
    assert_eq!(booted.kernel.root().unwrap().id.0, "kernel_state");
    assert_eq!(
        booted.loaded,
        vec![AgentId::from("child_a")],
        "children survive a reseed"
    );
    let raw = std::fs::read_to_string(fdir.join("agent.yaml")).unwrap();
    let rec: Value = crate::yaml_io::load(&raw).expect("reseeded record parses");
    assert_eq!(rec["id"], json!("kernel_state"));
    shutdown(tmp.path()).unwrap();
}

/// A root record that is THERE but does not parse is never written over: the
/// boot refuses and the file stays as it was (python `main._bootstrap`).
#[test]
fn an_unreadable_root_record_refuses_the_boot_and_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    let fdir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&fdir).unwrap();
    std::fs::write(fdir.join("agent.yaml"), "{").unwrap();
    let Err(e) = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    ) else {
        panic!("a corrupt root record booted");
    };
    assert!(e.to_string().contains("unreadable"), "{e}");
    assert_eq!(
        std::fs::read_to_string(fdir.join("agent.yaml")).unwrap(),
        "{"
    );
    let _ = shutdown(tmp.path());
}

/// THE COUNTER COMES BACK WITH THE GUID: a root record's reserved bound
/// (`next_id`) is where the next run starts, a block is reserved in memory, and
/// the load writes nothing (save → load → save is identical). There is no
/// message tape to hydrate.
#[test]
fn the_root_records_next_id_resumes_the_counter_and_the_load_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let fdir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&fdir).unwrap();
    let body = "id: kernel_state\nhandler_module: kernel_state.tools\nguid: g-1\nnext_id: 5000\n";
    std::fs::write(fdir.join("agent.yaml"), body).unwrap();
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .expect("boot");
    assert_eq!(booted.kernel.rows.next_id(), 5000);
    assert_eq!(
        booted.kernel.rows.id_reserved(),
        5000 + crate::rows::ID_BLOCK
    );
    assert!(
        booted.kernel.mint_id() >= 5000,
        "ids never repeat across a restart"
    );
    assert_eq!(
        std::fs::read_to_string(fdir.join("agent.yaml")).unwrap(),
        body,
        "load never edits a record"
    );
    assert!(
        !fdir.join("messages.yaml").exists(),
        "no tape is ever written"
    );
    shutdown(tmp.path()).unwrap();
}

/// python `_bootstrap` is `kernel.load(read_tree(…))`, and `load` VALIDATES
/// before it builds: a record on disk with an id `create_agent` would refuse,
/// a reserved key, or a duplicate refuses the boot instead of loading as-is.
#[test]
fn bootstrap_refuses_a_tree_that_fails_validation() {
    for (dir, body, want) in [
        (
            "kernel",
            "id: kernel\n",
            "record 'kernel': load: id ",
        ),
        (
            "rp",
            "id: rp\nroot_path: /elsewhere\n",
            "record 'rp' carries reserved key(s) ['root_path'] — these are Agent constructor parameters",
        ),
        ("seven", "id: 7\n", "record 7: load: id must be a non-empty string"),
    ] {
        let tmp = TempDir::new().unwrap();
        let at = tmp.path().join(".fantastic/agents").join(dir);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join("agent.yaml"), body).unwrap();
        let err = match bootstrap(
            with_loader(BundleRegistry::new()),
            BootstrapOptions::one_shot(tmp.path()),
        ) {
            Ok(_) => panic!("{dir}: a bad tree booted"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(want), "{dir}: {err}");
    }
    // A duplicate id across two dirs.
    let tmp = TempDir::new().unwrap();
    for dir in ["one", "two"] {
        let at = tmp.path().join(".fantastic/agents").join(dir);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join("agent.yaml"), "id: twin\n").unwrap();
    }
    let err = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::one_shot(tmp.path()),
    )
    .err()
    .expect("a duplicate refuses the boot")
    .to_string();
    assert!(
        err.contains("duplicate agent id 'twin' in snapshot"),
        "{err}"
    );
}

/// python `Kernel.load` reads `children_dir` off the ROOT record and every
/// agent's address follows it: hydration walks `<root>/<children_dir>/`, and a
/// child created later lands under it too.
#[test]
fn the_root_records_children_dir_places_the_whole_tree() {
    let tmp = TempDir::new().unwrap();
    let fantastic = tmp.path().join(".fantastic");
    std::fs::create_dir_all(fantastic.join("host_agents/web")).unwrap();
    std::fs::write(
        fantastic.join("agent.yaml"),
        "id: kernel_state\nhandler_module: kernel_state.tools\nchildren_dir: host_agents\n",
    )
    .unwrap();
    std::fs::write(fantastic.join("host_agents/web/agent.yaml"), "id: web\n").unwrap();
    let mut reg = with_loader(BundleRegistry::new());
    reg.register("noop.tools", Noop);
    let booted = bootstrap(reg, BootstrapOptions::one_shot(tmp.path())).expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    let web = kernel
        .agents
        .get(&AgentId::from("web"))
        .map(|e| Arc::clone(&e));
    let web = web.expect("hydrated from host_agents/");
    assert_eq!(web.root_path, fantastic.join("host_agents/web"));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": "web", "id": "leaf",
                       "handler_module": "noop.tools"}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], "leaf", "{r}");
    });
    let leaf = kernel
        .agents
        .get(&AgentId::from("leaf"))
        .map(|e| e.root_path.clone());
    assert_eq!(
        leaf,
        Some(fantastic.join("host_agents/web/host_agents/leaf"))
    );
}

#[test]
fn daemon_at_keeps_the_lock_the_root_agents_and_the_store_in_state_dir() {
    let tmp = TempDir::new().unwrap();
    let w = tmp.path().join("app");
    std::fs::create_dir_all(&w).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut reg = with_loader(BundleRegistry::new());
    reg.register("noop.tools", Noop);
    crate::test_support::register_fake_store(&mut reg, &w);
    let booted = bootstrap(reg, BootstrapOptions::daemon_at(&w, &w)).expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    assert_eq!(kernel.workdir().unwrap(), w);
    assert_eq!(kernel.root().unwrap().root_path, w);
    rt.block_on(async {
        boot_root(&kernel).await;
        // a RELATIVE store root resolves against the workdir: `.` IS the state dir
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type": "create_agent", "parent_id": "kernel",
                    "handler_module": "file_bridge.tools", "allow": ["."],
                    "id": "store", "root": ".", "ingress_rule": "allow_all_ingress",
                }),
            )])
            .await;
        assert_eq!(
            crate::persistence::find_store(&kernel),
            Some(AgentId::from("store"))
        );
        let _ = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"child_a"}))])
            .await;
        kernel.flush_records().await;
    });
    assert!(w.join("lock.yaml").is_file());
    assert!(w.join("agent.yaml").is_file());
    assert!(w.join("agents/child_a/agent.yaml").is_file());
    assert!(w.join("agents/store/agent.yaml").is_file());
    assert!(
        !w.join(".fantastic").exists(),
        "no .fantastic inside the state dir"
    );
    assert!(!tmp.path().join(".fantastic").exists());
    assert_eq!(lock::held_at(&w).unwrap(), Some(std::process::id()));
    shutdown_at(&w).unwrap();
    assert!(!w.join("lock.yaml").exists());
}

#[test]
fn the_guard_refuses_a_state_dir_that_is_the_home() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join(".fantastic");
    let err = refuse_home_as_state_dir(tmp.path(), &home, Some(&home)).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "{}: its .fantastic is the fantastic home ({}) — run fantastic in a project folder",
            tmp.path().display(),
            home.display()
        )
    );
    // folded lexically: `<home>/../.fantastic` is the home too
    let odd = tmp.path().join("x/../.fantastic");
    assert!(refuse_home_as_state_dir(tmp.path(), &odd, Some(&home)).is_err());
    // the home kernel's own state dir is `<home>/app`, never the home
    let app = home.join("app");
    refuse_home_as_state_dir(&app, &app, Some(&home)).unwrap();
    // a project folder, and no home at all
    let proj = tmp.path().join("proj");
    refuse_home_as_state_dir(&proj, &proj.join(".fantastic"), Some(&home)).unwrap();
    refuse_home_as_state_dir(tmp.path(), &home, None).unwrap();
}

#[test]
fn edit_grants_refused_in_one_map_writes_neither() {
    let kernel = Kernel::new();
    kernel.register(Agent::new(
        AgentId::from("both_guarded"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::from("/nonexistent"),
        false,
    ));
    let id = AgentId::from("both_guarded");
    let meta = |k: &Kernel| {
        k.agents
            .get(&AgentId::from("both_guarded"))
            .unwrap()
            .record()
            .meta
    };
    let before = meta(&kernel);
    // a good can_call entry and a malformed callable_by one: the whole step is refused
    let refused = kernel.edit_grants(&id, |cc, cb| {
        cc.insert("peer".to_string(), json!(["*"]));
        cb.insert("x".to_string(), json!(5));
        (true, true)
    });
    assert!(refused.is_err(), "{refused:?}");
    let m = meta(&kernel);
    assert_eq!(
        m.get("can_call"),
        before.get("can_call"),
        "the can_call edit is not written: {m:?}"
    );
    assert_eq!(m.get("callable_by"), before.get("callable_by"), "{m:?}");
    assert!(m["can_call"].get("peer").is_none(), "{m:?}");
    // the same step with a good callable_by writes both
    let ok = kernel.edit_grants(&id, |cc, cb| {
        cc.insert("peer".to_string(), json!(["*"]));
        cb.insert("x".to_string(), json!(["send"]));
        (true, true)
    });
    assert_eq!(ok, Ok(true));
    let m = meta(&kernel);
    assert_eq!(m["can_call"]["peer"], json!(["*"]));
    assert_eq!(m["callable_by"]["x"], json!(["send"]));
}
