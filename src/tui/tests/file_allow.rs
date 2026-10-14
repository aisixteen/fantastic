//! EXPOSURE of a served directory: a file leg is DENY-ALL per path; its `allow`
//! list (the record's — `update_agent` from above, the operator here) decides
//! what it serves, to an agent granted `read_stream` by id on both ends, and it
//! persists across restarts.


/// Drive everything through the binary's own modules by re-exec'ing? No — the
/// crate is a binary, so its modules are not importable. Instead this test drives
/// the kernel crates directly with the SAME registry and the SAME records the shell
/// seeds, which is what makes it a test of the contract rather than of the wiring.
use fantastic_kernel::{
    bootstrap::{self, BootstrapOptions},
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
    reg.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
    reg
}

fn write(path: &std::path::Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, fantastic_kernel::yaml_io::dump(v)).unwrap();
}

/// A project with a read-only, DENY-ALL file leg `docs` over `docs/` exposing
/// only `a.txt`, the kernel's record store, and `term`, granted `read_stream` on
/// `docs` (both ends, by id).
fn seed(dir: &std::path::Path) {
    let a = dir.join(".fantastic").join("agents");
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    for f in ["a.txt", "b.txt"] {
        std::fs::write(dir.join("docs").join(f), format!("this is {f}")).unwrap();
    }
    write(
        &a.join("memory").join("agent.yaml"),
        &json!({"id":"memory","handler_module":"sqlite_store.tools","path":".fantastic/memory.db"}),
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
                "callable_by":{"term": ["read_stream"]}}),
    );
    write(
        &a.join("term").join("agent.yaml"),
        &json!({"id":"term","handler_module":"user.tools","can_call":{"docs": ["read_stream"]}}),
    );
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

/// `from` calls `docs`: a `forward` on `from` (a `user.tools` agent), sent from
/// inside its own handler — so the call carries `from`.
async fn call(kernel: &std::sync::Arc<Kernel>, from: &str, payload: Value) -> Value {
    let k = std::sync::Arc::clone(kernel);
    let item = Item::new(AgentId::from(from), json!({"type":"forward","target":"docs","payload":payload}));
    k.send(vec![item]).await.remove(0).result
}

fn refused(v: &Value) -> bool {
    v.get("error").is_some()
}

/// The operator sets `docs`'s allow list (the record's, written from above).
async fn expose(kernel: &std::sync::Arc<Kernel>, allow: Value) {
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel"), json!({"type":"update_agent","id":"docs","allow":allow}))])
        .await
        .remove(0)
        .result;
    assert!(!refused(&r), "{r}");
}

// A served directory serves exactly its allow list; widened and narrowed from
// above, it persists across restarts.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_leg_is_widened_and_narrowed_and_it_persists() {
    let dir = std::env::temp_dir().join(format!("fc-allow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir);
    std::env::set_current_dir(&dir).unwrap();
    let read = |p: &str| json!({"type":"read_stream","path":p});

    let kernel = boot(&dir).await;
    assert!(!refused(&call(&kernel, "term", read("a.txt")).await), "the listed file is served");
    assert!(refused(&call(&kernel, "term", read("b.txt")).await), "an unlisted file is not");
    // the agent itself may not widen it: grants and fields come from above
    let r = call(&kernel, "term", json!({"type":"update_agent","id":"docs","allow":["."]})).await;
    assert!(refused(&r), "{r}");
    expose(&kernel, json!(["a.txt", "b.txt"])).await;
    assert!(!refused(&call(&kernel, "term", read("b.txt")).await), "b.txt is served once allowed");

    // …and it survives a restart (the record is persisted through the store).
    kernel.shutdown().await;
    bootstrap::shutdown(&dir).unwrap();
    drop(kernel);
    let kernel = boot(&dir).await;
    assert!(!refused(&call(&kernel, "term", read("b.txt")).await), "after a restart");

    // narrowed: a.txt no longer served, and still not after a restart
    expose(&kernel, json!(["b.txt"])).await;
    assert!(refused(&call(&kernel, "term", read("a.txt")).await), "a.txt is no longer served");
    kernel.shutdown().await;
    bootstrap::shutdown(&dir).unwrap();
    drop(kernel);
    let kernel = boot(&dir).await;
    assert!(refused(&call(&kernel, "term", read("a.txt")).await), "after the second restart");
    assert!(!refused(&call(&kernel, "term", read("b.txt")).await));
    kernel.shutdown().await;
    bootstrap::shutdown(&dir).unwrap();
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
