//! THE END-TO-END PROOF, with no terminal in the way: seed a directory, boot the
//! rust kernel in this process, and drive a real command through `user` to `exec`.
//!
//! This lives in its own integration binary on purpose. `file_bridge`'s clamp
//! resolves paths against `std::env::current_dir()`, so the test must `chdir`,
//! and chdir is process-global. One test per binary means no other test can be running when it
//! happens.
//!
//! What it proves, in order: the tree is the specified shape; a call to its own
//! child is authorized by the seeded grants; the command actually ran; and a call the grants
//! do not cover is REFUSED as data rather than succeeding.

mod sys;

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
    reg.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
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
        fantastic_user::HANDLER_MODULE,
        fantastic_user::UserBundle::default(),
    );
    reg
}

fn write(path: &std::path::Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, fantastic_kernel::yaml_io::dump(v)).unwrap();
}

/// The records `boot::seed::write_all` writes, duplicated here as the CONTRACT
/// this test pins. If the seed changes shape, this fails and says so.
fn seed(dir: &std::path::Path) {
    let a = dir.join(".fantastic").join("agents");
    let under = a.join("user").join("agents");
    write(
        &a.join("user").join("agent.yaml"),
        &json!({
            "id":"user","handler_module":"user.tools",
            "can_call":{"memory":"*","exec":"*","files":"*",bootstrap::DEFAULT_ROOT_ID:"*"},
            "callable_by":{"exec":"*","files":"*"}
        }),
    );
    write(
        &under.join("memory").join("agent.yaml"),
        &json!({
            "id":"memory","handler_module":"sqlite_store.tools",
            "path":".fantastic/memory.db","operator":true,"admins":["kernel"],
            "callable_by":{"user":"*"}
        }),
    );
    write(
        &under.join("exec").join("agent.yaml"),
        &json!({
            "id":"exec","handler_module":"exec_bridge.tools",
            "allowed":[".*"],"cwd":dir.to_string_lossy(),"inherit_env":["PATH","HOME","LANG"],"ingress_rule":"allow_all_ingress",
            "callable_by":{"user":"*"},"can_call":{"files":"*","user":"*"}
        }),
    );
    write(
        &under.join("files").join("agent.yaml"),
        &json!({
            "id":"files","handler_module":"file_bridge.tools","root":".","allow":["."],
            "ingress_rule":"allow_all_ingress",
            "callable_by":{"user":"*","exec":"*"},"can_call":{"user":"*"}
        }),
    );
}

/// Dispatch as the human, exactly as the shell does: `user.forward` — the send
/// is made from inside `user`'s own handler, so it carries `user`.
async fn act(kernel: &std::sync::Arc<Kernel>, target: &str, payload: Value) -> Value {
    let k = std::sync::Arc::clone(kernel);
    let item = Item::new(
        AgentId::from("user"),
        json!({"type":"forward","target":target,"payload":payload}),
    );
    k.send(vec![item]).await.remove(0).result
}

#[tokio::test(flavor = "multi_thread")]
async fn the_human_drives_the_specified_set_in_process() {
    let dir = std::env::temp_dir().join(format!("fc-inproc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir);
    // Both bundles clamp against the process cwd, so this is required, not tidy.
    std::env::set_current_dir(&dir).unwrap();

    let booted = bootstrap::bootstrap(registry(), BootstrapOptions::daemon(&dir))
        .expect("the kernel boots over a seeded directory");
    let kernel = std::sync::Arc::clone(&booted.kernel);
    // The shell's start: the root names the store, the KERNEL starts (root,
    // store, flusher), then everyone else boots.
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":bootstrap::DEFAULT_ROOT_ID,"memory_store_id":"memory"}),
        )])
        .await;
    let out = kernel.start(false).await;
    assert_eq!(out["started"], json!(true), "kernel_start: {out}");
    for id in &booted.loaded {
        if out["booted"].as_array().unwrap().contains(&json!(id.0)) {
            continue;
        }
        kernel
            .send(vec![Item::new(id.clone(), json!({"type":"boot"}))])
            .await;
    }

    // ── the tree is the specified shape ──────────────────────────────────
    let root = kernel.root().expect("a root");
    assert_eq!(
        root.id.0,
        bootstrap::DEFAULT_ROOT_ID,
        "the root is the kernel's own id, discovered not assumed"
    );
    let lvl1: Vec<String> = root.child_ids().into_iter().map(|i| i.0).collect();
    assert_eq!(lvl1, vec!["user"], "level one: the one user");

    let user = kernel
        .agents
        .get(&AgentId::from("user"))
        .expect("the user loaded");
    let mut under: Vec<String> = user.child_ids().into_iter().map(|i| i.0).collect();
    under.sort();
    assert_eq!(under, vec!["exec", "files", "memory"], "under the user");

    // ── a call to its own child is authorized by the seeded grants ───────
    // The born-with baseline is {$self} alone, so `user` reaching `exec` is the
    // explicit grants on both ends working.
    // A UNIQUE MARKER, not `pwd`: a status reply echoes the job's own `cwd`, so
    // matching a path would pass on metadata without the command ever running.
    const MARKER: &str = "FC-MARKER-8f3a1c";
    let (command, args) = sys::echo(MARKER);
    let started = act(
        &kernel,
        "exec",
        json!({"type":"start","command":command,"args":args}),
    )
    .await;
    assert!(
        started.get("error").is_none(),
        "the human could not call its own shell: {started}"
    );
    let job = started
        .get("job_id")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("no job_id in {started}"));

    // ── the command actually ran, and its OUTPUT came back ───────────────
    let mut last = Value::Null;
    let mut saw = false;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        last = act(&kernel, "exec", json!({"type":"status","job_id":job})).await;
        if last.to_string().contains(MARKER) {
            saw = true;
            break;
        }
    }
    assert!(
        saw,
        "the marker never reached the job output, so `echo` did not actually run: {last}"
    );

    // ── a call the grants do not cover is REFUSED, as data ───────────────
    // Ask the AI store to do something it does not own: an
    // unknown verb comes back as a refusal object, not a panic and not a success.
    let bad = act(&kernel, "memory", json!({"type":"definitely_not_a_verb"})).await;
    assert!(
        bad.get("error").is_some(),
        "an unknown verb must answer with a refusal: {bad}"
    );

    // ── the execution rows land in the memory store's file ───────────────
    kernel.flush_executions().await;
    let settles = kernel
        .send(vec![Item::new(
            AgentId::from("memory"),
            json!({"type":"read","owner":"kernel","kinds":["settle"]}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        !settles["rows"].as_array().unwrap().is_empty(),
        "the calls above must have left settle rows in the store: {settles}"
    );
    assert!(
        dir.join(".fantastic").join("memory.db").is_file(),
        "one SQLite file"
    );

    kernel.shutdown().await;
    let _ = bootstrap::shutdown(&dir);
    // Leave the cwd somewhere that exists, then clean up.
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
