//! THE SWITCH IS ON DISK BEFORE THE HOME IS TOUCHED: `register_home_folder` and
//! `name_home_folder` answer only once the `user_store` record has landed, so a
//! kill right after the answer cannot leave a home folder that the next boot
//! does not know about (the record writer is debounced; the verbs drain it).
//!
//! Its own integration binary: the `store` file_bridge clamps against the process
//! cwd, so the test must `chdir`, and chdir is process-global (see `inproc.rs`).

use fantastic_kernel::{
    bootstrap::{self, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_user_home_store::user_store::{self, UserStore};
use serde_json::{json, Value};

fn write(path: &std::path::Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, fantastic_kernel::yaml_io::dump(v)).unwrap();
}

/// The shell's records that matter here: the kernel's RAM store, `user`, the
/// `store` the root persists through, and `user_store` under `user`.
fn seed(dir: &std::path::Path) {
    let a = dir.join(".fantastic").join("agents");
    let under = a.join("user").join("agents");
    write(
        &a.join("kernel_memory").join("agent.yaml"),
        &json!({"id":"kernel_memory","handler_module":"ram_store.tools","admins":["kernel"]}),
    );
    write(
        &a.join("user").join("agent.yaml"),
        &json!({"id":"user","handler_module":"user.tools",
                "can_call":{"store":"*","user_store":"*",bootstrap::DEFAULT_ROOT_ID:"*"}}),
    );
    write(
        &under.join("store").join("agent.yaml"),
        &json!({"id":"store","handler_module":"file_bridge.tools","root":".fantastic","allow":["."],
                "ingress_rule":"allow_all_ingress","callable_by":{"user":"*"}}),
    );
    write(
        &under.join("user_store").join("agent.yaml"),
        &json!({"id":"user_store","handler_module":user_store::HANDLER_MODULE,"callable_by":{"user":"*"}}),
    );
}

fn registry(home: &std::path::Path, here: &std::path::Path) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    reg.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    reg.register(
        fantastic_user::HANDLER_MODULE,
        fantastic_user::UserBundle::default(),
    );
    reg.register(
        user_store::HANDLER_MODULE,
        UserStore::new(Ok(home.to_path_buf()), here.to_path_buf()),
    );
    reg
}

/// As the human: `user.forward`.
async fn act(kernel: &std::sync::Arc<Kernel>, target: &str, payload: Value) -> Value {
    let item = Item::new(
        AgentId::from("user"),
        json!({"type":"forward","target":target,"payload":payload}),
    );
    kernel.send(vec![item]).await.remove(0).result
}

/// The `user_store` record as it is ON DISK right now.
fn record_on_disk(dir: &std::path::Path) -> Value {
    let p = dir.join(".fantastic/agents/user/agents/user_store/agent.yaml");
    fantastic_kernel::yaml_io::load(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_switch_and_the_name_are_on_disk_when_the_verbs_answer() {
    let base = std::env::temp_dir().join(format!("fc-durable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (dir, home) = (base.join("proj"), base.join("home"));
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir);
    std::env::set_current_dir(&dir).unwrap();

    let booted =
        bootstrap::bootstrap(registry(&home, &dir), BootstrapOptions::daemon(&dir)).expect("boots");
    let kernel = std::sync::Arc::clone(&booted.kernel);
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":bootstrap::DEFAULT_ROOT_ID,"memory_store_id":"kernel_memory"}),
        )])
        .await;
    let out = kernel.start(false).await;
    assert_eq!(out["started"], json!(true), "kernel_start: {out}");
    for id in &booted.loaded {
        if !out["booted"].as_array().unwrap().contains(&json!(id.0)) {
            kernel
                .send(vec![Item::new(id.clone(), json!({"type":"boot"}))])
                .await;
        }
    }
    let root = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"reflect","tree":"none"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        root["persistence"]["provider"], "store",
        "records persist through `store`: {root}"
    );
    let guid = root["guid"].as_str().expect("a guid").to_string();

    // ON: the record has landed by the time the answer is here — no wait
    let r = act(
        &kernel,
        "user_store",
        json!({"type": user_store::SWITCH, "value": true}),
    )
    .await;
    assert_eq!(r[user_store::SWITCH], true, "{r}");
    assert_eq!(
        record_on_disk(&dir)[user_store::SWITCH],
        true,
        "the switch is on disk when the verb answers"
    );
    assert!(
        home.join("fantastics")
            .join(&guid)
            .join(user_store::INFO)
            .is_file(),
        "and the folder is in the home"
    );

    // NAMED: the same
    let r = act(
        &kernel,
        "user_store",
        json!({"type": "name_home_folder", "name": "durable"}),
    )
    .await;
    assert_eq!(r["folder"], "durable", "{r}");
    assert_eq!(
        record_on_disk(&dir)[user_store::NAME],
        "durable",
        "the name is on disk when the verb answers"
    );
    assert!(home
        .join("fantastics/durable")
        .join(user_store::INFO)
        .is_file());

    // OFF: the same
    let r = act(
        &kernel,
        "user_store",
        json!({"type": user_store::SWITCH, "value": false}),
    )
    .await;
    assert_eq!(r["unstored"], true, "{r}");
    assert_eq!(record_on_disk(&dir)[user_store::SWITCH], false);
    assert!(!home.join("fantastics/durable").exists());

    kernel.shutdown().await;
    let _ = bootstrap::shutdown(&dir);
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&base);
}
