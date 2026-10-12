//! STRAY PROCESSES: closing a terminal (and stopping the kernel) must leave
//! nothing the shell started running.

use std::process::Command;

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
    reg.register(
        fantastic_static_loader::HANDLER_MODULE,
        fantastic_static_loader::StaticLoaderBundle,
    );
    reg.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
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
            "can_call":{"memory":"*","fs_loader":"*","exec":"*","files":"*",bootstrap::DEFAULT_ROOT_ID:"*"},
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
        &under.join("fs_loader").join("agent.yaml"),
        &json!({
            "id":"fs_loader","handler_module":"static_loader.tools",
            "manifest":".fantastic/kernel.yaml","callable_by":{"user":"*"}
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
    std::fs::write(dir.join(".fantastic").join("kernel.yaml"), "agents: []\n").unwrap();
}

/// Dispatch as the human, exactly as the shell does: `user.forward` — the send
/// is made from inside `user`'s own handler, so it carries `user`.
async fn act(kernel: &std::sync::Arc<Kernel>, target: &str, payload: Value) -> Value {
    let k = std::sync::Arc::clone(kernel);
    let item = Item::new(AgentId::from("user"), json!({"type":"forward","target":target,"payload":payload}));
    k.send(vec![item]).await.remove(0).result
}


/// Is a process whose command line contains `pat` alive?
fn alive(pat: &str) -> bool {
    let out = Command::new("pgrep").args(["-f", pat]).output().expect("pgrep runs");
    !out.stdout.is_empty()
}

/// Boot the seeded set, start `$SHELL` as a PTY job on `exec`, and have it start
/// a background, a foreground and a `nohup`ed child — the processes a human
/// leaves behind in a terminal. Returns the kernel and the three markers.
async fn shell_with_children(tag: &str) -> (std::sync::Arc<Kernel>, std::path::PathBuf, [String; 3]) {
    let dir = std::env::temp_dir().join(format!("fc-term-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir);
    std::env::set_current_dir(&dir).unwrap();
    let booted = bootstrap::bootstrap(registry(), BootstrapOptions::daemon(&dir)).expect("boots");
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
    let sh = std::env::var("SHELL").expect("SHELL is set");
    let started = act(&kernel, "exec", json!({"type":"start","command":sh,"pty":{"cols":80,"rows":24}})).await;
    let job = started["job_id"].as_i64().unwrap_or_else(|| panic!("{started}"));
    // Distinct per test and per run, so a leftover of another run never counts.
    let n = std::process::id() % 1000;
    let m: [String; 3] = [
        format!("sleep 71{n:03}{}", tag.len()),
        format!("sleep 72{n:03}{}", tag.len()),
        format!("sleep 73{n:03}{}", tag.len()),
    ];
    let line = format!("{} &\rnohup {} >/dev/null 2>&1 &\r{}\r", m[0], m[2], m[1]);
    let w = act(&kernel, "exec", json!({"type":"write","job_id":job,"data":line})).await;
    assert!(w.get("error").is_none(), "{w}");
    for _ in 0..100 {
        if m.iter().all(|p| alive(p)) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    for p in &m {
        assert!(alive(p), "the shell never started `{p}`");
    }
    (kernel, dir, m)
}

/// What survived, after a grace period for the kernel's SIGHUP to land.
async fn survivors(m: &[String; 3]) -> Vec<String> {
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let left: Vec<String> = m.iter().filter(|p| alive(p)).cloned().collect();
    for p in &left {
        let _ = Command::new("pkill").args(["-9", "-f", p]).status();
    }
    left
}

// ONE test, both paths in turn: each boot moves the process cwd, so two tests
// in this binary would race on it.
#[tokio::test(flavor = "multi_thread")]
async fn no_process_outlives_its_terminal() {
    // Closing the terminal (`t` again, a canvas frame's close): `delete_agent`.
    let (kernel, _dir, m) = shell_with_children("close").await;
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel"), json!({"type":"delete_agent","id":"exec"}))])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let left = survivors(&m).await;
    kernel.shutdown().await;
    assert!(left.is_empty(), "alive after delete_agent: {left:?}");

    // Quitting the CLI: the kernel's shutdown.
    let (kernel, _dir, m) = shell_with_children("shutdown").await;
    kernel.shutdown().await;
    let left = survivors(&m).await;
    assert!(left.is_empty(), "alive after kernel shutdown: {left:?}");
}
