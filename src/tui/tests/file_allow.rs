//! RUNTIME EXPOSURE of a served directory: a file leg's `allow` list grows and
//! shrinks on need, by the agents under the canvas's ws leg ($subtree +
//! a heritable can_call), and persists.


/// Drive everything through the binary's own modules by re-exec'ing? No — the
/// crate is a binary, so its modules are not importable. Instead this test drives
/// the kernel crates directly with the SAME registry and the SAME records the shell
/// seeds, which is what makes it a test of the contract rather than of the wiring.
use fantastic_kernel::{
    bootstrap::{self, BootstrapOptions},
    send::with_sender,
    AgentId, BundleRegistry, Item, Kernel,
};
use serde_json::{json, Value};

fn registry() -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    reg.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    reg.register(
        fantastic_sqlite_store::HANDLER_MODULE,
        fantastic_sqlite_store::SqliteStoreBundle::default(),
    );
    reg.register(
        fantastic_exec_bridge::HANDLER_MODULE,
        fantastic_exec_bridge::ExecBridgeBundle,
    );
    reg.register(
        fantastic_static_loader::HANDLER_MODULE,
        fantastic_static_loader::StaticLoaderBundle,
    );
    reg.register(
        fantastic_scenario_probes::USER_MODULE,
        fantastic_scenario_probes::UserProbe::default(),
    );
    reg
}

fn write(path: &std::path::Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, fantastic_kernel::yaml_io::dump(v)).unwrap();
}

/// The records `boot::seed::write_all` writes, duplicated here as the CONTRACT
/// this test pins. If the seed changes shape, this fails and says so.
/// A project with a read-only, DENY-ALL file leg `docs` over `docs/` exposing
/// only `a.txt` (the shape the converter gives every served dir), the kernel's
/// record store, and the canvas's ws leg `ws` with a terminal `term` under it
/// and `deep` under that. The wiring is the canvas recipe: the leg ACCEPTS the
/// ws subtree (`$subtree:ws`), and `ws` HANDS DOWN the verbs to its
/// descendants (a heritable `can_call`) — never using them itself. `outsider`
/// is not under `ws`.
fn seed(dir: &std::path::Path) {
    let a = dir.join(".fantastic").join("agents");
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    for f in ["a.txt", "b.txt"] {
        std::fs::write(dir.join("docs").join(f), format!("this is {f}")).unwrap();
    }
    const VERBS: [&str; 4] = ["read_stream", "allow", "disallow", "allowed"];
    write(
        &a.join("memory").join("agent.yaml"),
        &json!({"id":"memory","handler_module":"sqlite_store.tools",
                "path":".fantastic/memory.db","operator":true,"admins":["kernel"]}),
    );
    write(
        &a.join("store").join("agent.yaml"),
        &json!({"id":"store","handler_module":"file_bridge.tools","root":".fantastic","allow":["."],
                "ingress_rule":"allow_all_ingress"}),
    );
    write(
        &a.join("docs").join("agent.yaml"),
        &json!({"id":"docs","handler_module":"file_bridge.tools","root":"docs","readonly":true,
                "allow":["a.txt"],"ingress_rule":"allow_all_ingress",
                "callable_by":{"$subtree:ws": VERBS, "outsider": VERBS}}),
    );
    let ws = a.join("ws");
    write(
        &ws.join("agent.yaml"),
        &json!({"id":"ws","can_call":{"docs":{"verbs": VERBS, "for":"descendants"}}}),
    );
    write(&ws.join("agents").join("term").join("agent.yaml"), &json!({"id":"term","handler_module":"user_probe.tools"}));
    write(
        &ws.join("agents").join("term").join("agents").join("deep").join("agent.yaml"),
        &json!({"id":"deep","handler_module":"user_probe.tools"}),
    );
    write(
        &a.join("outsider").join("agent.yaml"),
        &json!({"id":"outsider","handler_module":"user_probe.tools","can_call":{"docs":["read_stream","allowed"]}}),
    );
    std::fs::write(dir.join(".fantastic").join("kernel.yaml"), "agents: []\n").unwrap();
}

async fn boot(dir: &std::path::Path) -> std::sync::Arc<Kernel> {
    let booted = bootstrap::bootstrap(registry(), BootstrapOptions::daemon(dir)).expect("boots");
    let kernel = std::sync::Arc::clone(&booted.kernel);
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":bootstrap::DEFAULT_ROOT_ID,"memory_store_id":"memory"}),
        )])
        .await;
    assert_eq!(kernel.start(false).await["started"], json!(true));
    for id in &booted.loaded {
        kernel.send(vec![Item::new(id.clone(), json!({"type":"boot"}))]).await;
    }
    kernel
}

async fn call(kernel: &std::sync::Arc<Kernel>, from: &str, payload: Value) -> Value {
    let k = std::sync::Arc::clone(kernel);
    let item = Item::new(AgentId::from("docs"), payload);
    with_sender(AgentId::from(from), async move { k.send(vec![item]).await })
        .await
        .remove(0)
        .result
}

fn refused(v: &Value) -> bool {
    v.get("error").is_some()
}

// A served directory's exposure is changed AT RUNTIME by the agents living
// under the ws leg — a terminal, and anything under it, spawned or not — with
// no grant naming them; the leg itself, and an agent outside it, cannot. The
// list survives a restart.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_leg_is_widened_and_narrowed_at_runtime_and_it_persists() {
    let dir = std::env::temp_dir().join(format!("fc-allow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir);
    std::env::set_current_dir(&dir).unwrap();
    let kernel = boot(&dir).await;
    let read = |p: &str| json!({"type":"read_stream","path":p});

    assert!(!refused(&call(&kernel, "term", read("a.txt")).await), "the listed file is served");
    assert!(refused(&call(&kernel, "term", read("b.txt")).await), "an unlisted file is not");

    // Outside the ws subtree: it may read, but it does not hold `allow`.
    let r = call(&kernel, "outsider", json!({"type":"allow","path":"b.txt"})).await;
    assert!(refused(&r), "an agent outside the ws leg cannot widen it: {r}");
    // The leg itself hands the verbs DOWN; it never uses them.
    let r = call(&kernel, "ws", json!({"type":"allow","path":"b.txt"})).await;
    assert!(refused(&r), "the holder of a heritable entry is refused: {r}");

    // A terminal under ws can — on a READ-ONLY leg too: exposure is not a disk write.
    let r = call(&kernel, "term", json!({"type":"allow","path":"b.txt"})).await;
    assert_eq!(r["allowed"], json!(true), "{r}");
    assert!(!refused(&call(&kernel, "outsider", read("b.txt")).await), "b.txt is served once allowed");

    // An agent CREATED under ws after the wiring is covered with no edit.
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel"), json!({"type":"create_agent","parent_id":"ws","id":"later","handler_module":"user_probe.tools"}))])
        .await
        .remove(0)
        .result;
    assert!(!refused(&r), "{r}");
    let r = call(&kernel, "later", json!({"type":"allowed"})).await;
    assert_eq!(r["paths"], json!(["a.txt", "b.txt"]), "{r}");
    // …and so is one two levels down.
    let r = call(&kernel, "deep", json!({"type":"allowed"})).await;
    assert_eq!(r["paths"], json!(["a.txt", "b.txt"]), "{r}");

    // …and it survives a restart (the record is persisted through the store).
    kernel.shutdown().await;
    bootstrap::shutdown(&dir).unwrap();
    drop(kernel);
    let kernel = boot(&dir).await;
    let r = call(&kernel, "outsider", json!({"type":"allowed"})).await;
    assert_eq!(r["paths"], json!(["a.txt", "b.txt"]), "after a restart: {r}");

    // Narrowed again by another agent under ws: refused on read, gone after a restart.
    let r = call(&kernel, "later", json!({"type":"disallow","path":"a.txt"})).await;
    assert_eq!(r["allowed"], json!(false), "{r}");
    assert!(refused(&call(&kernel, "outsider", read("a.txt")).await), "a.txt is no longer served");
    kernel.shutdown().await;
    bootstrap::shutdown(&dir).unwrap();
    drop(kernel);
    let kernel = boot(&dir).await;
    let r = call(&kernel, "outsider", json!({"type":"allowed"})).await;
    assert_eq!(r["paths"], json!(["b.txt"]), "after the second restart: {r}");
    kernel.shutdown().await;
}
