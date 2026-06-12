//! Integration tests for the `fantastic` binary.
//!
//! Each test runs the binary in a fresh tempdir so the workdir lock
//! and on-disk state stay scoped.

use std::process::Command;

fn fantastic_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fantastic")
}

#[test]
fn no_args_in_virgin_dir_exits_zero_silently() {
    // No web agent persisted → the daemon mode boots, finds nothing
    // worth keeping the process alive, and exits 0.
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic");
    assert!(
        output.status.success(),
        "fantastic exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn reflect_returns_uniform_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .arg("reflect")
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic reflect");
    assert!(
        output.status.success(),
        "fantastic reflect exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Uniform reflect: id + tree (default all). No primer keys.
    assert!(
        stdout.contains("\"id\""),
        "reflect missing id key: {stdout}"
    );
    assert!(stdout.contains("\"tree\""), "reflect missing tree key");
    assert!(
        !stdout.contains("\"transports\""),
        "transports should have moved to the readme: {stdout}"
    );
    assert!(
        !stdout.contains("\"available_bundles\""),
        "available_bundles is now the bundles flag: {stdout}"
    );
}

#[test]
fn reflect_bundles_all_lists_catalog() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .args(["reflect", "bundles=all"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic reflect bundles=all");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"bundles\"") && stdout.contains("\"handler_module\""),
        "reflect bundles=all missing catalog: {stdout}"
    );
}

/// Wire the persistence provider: a `file_bridge` store rooted at the loader's
/// own `.fantastic`, opened (`ingress_rule=allow_all`). Persistence is now
/// provider-routed — without a store, records stay in RAM (lost on a one-shot's
/// exit). The store persists ITSELF through itself, so it survives to the next
/// one-shot, after which other agents persist through it.
fn create_store(dir: &std::path::Path) {
    let out = Command::new(fantastic_bin())
        .args([
            "core",
            "create_agent",
            "handler_module=file_bridge.tools",
            "id=store",
            "root=.fantastic",
            "ingress_rule=allow_all",
        ])
        .current_dir(dir)
        .output()
        .expect("create store");
    assert!(
        out.status.success(),
        "create store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn one_shot_create_agent_persists_record() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Wire the store first — it self-persists, so .fantastic/agents/store
    // survives this process and the NEXT one-shot loads it as the provider.
    create_store(tmp.path());
    assert!(
        tmp.path()
            .join(".fantastic/agents/store/agent.json")
            .exists(),
        "the store must persist itself through itself"
    );
    let output = Command::new(fantastic_bin())
        .args([
            "core",
            "create_agent",
            "handler_module=file_bridge.tools",
            "id=ff",
            "root=/tmp",
            "ingress_rule=allow_all",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("create_agent");
    assert!(
        output.status.success(),
        "create_agent exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let path = tmp.path().join(".fantastic/agents/ff/agent.json");
    assert!(
        path.exists(),
        "agent.json not written (through the provider)"
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("file_bridge.tools"));
    assert!(content.contains("/tmp"));
}

#[test]
fn one_shot_dispatch_returns_json_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Wire the provider, then create a file agent that persists through it.
    create_store(tmp.path());
    Command::new(fantastic_bin())
        .args([
            "core",
            "create_agent",
            "handler_module=file_bridge.tools",
            "id=ff",
            "root=/tmp",
            "ingress_rule=allow_all",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    // Now dispatch reflect on it (rehydrated from the persisted record).
    let output = Command::new(fantastic_bin())
        .args(["ff", "reflect"])
        .current_dir(tmp.path())
        .output()
        .expect("reflect on ff");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"id\""));
    assert!(stdout.contains("\"sentence\""));
    assert!(stdout.contains("Filesystem root"));
}
