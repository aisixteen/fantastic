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
fn reflect_returns_primer_json() {
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
    assert!(
        stdout.contains("\"primitive\""),
        "reflect missing primitive key: {stdout}"
    );
    assert!(
        stdout.contains("send(target_id"),
        "reflect missing send signature"
    );
    assert!(stdout.contains("\"tree\""), "reflect missing tree key");
    assert!(stdout.contains("\"available_bundles\""));
}

#[test]
fn one_shot_create_agent_persists_record() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .args([
            "core",
            "create_agent",
            "handler_module=file.tools",
            "id=ff",
            "root=/tmp",
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
    assert!(path.exists(), "agent.json not written");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("file.tools"));
    assert!(content.contains("/tmp"));
}

#[test]
fn one_shot_dispatch_returns_json_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    // First create a file agent.
    Command::new(fantastic_bin())
        .args([
            "core",
            "create_agent",
            "handler_module=file.tools",
            "id=ff",
            "root=/tmp",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    // Now dispatch reflect on it.
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
