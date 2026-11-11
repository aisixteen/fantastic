//! `--statedir` and `--workdir` through the real binary (#563): a state folder far from
//! the work folder (a path too long for a unix socket address), a client that names it from
//! another directory, and a running kernel that refuses a work-folder change.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fantastic")
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-statedir-far-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn wait_for(path: &Path) {
    let end = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < end, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_far_state_folder_is_served_found_and_kept_apart_from_the_work_folder() {
    let t = scratch("far");
    let state = t
        .join("far")
        .join("d".repeat(60))
        .join("e".repeat(60))
        .join("state");
    let (work, other, elsewhere, home) = (
        t.join("w"),
        t.join("other"),
        t.join("elsewhere"),
        t.join("home"),
    );
    for d in [&work, &other, &elsewhere, &home] {
        std::fs::create_dir_all(d).unwrap();
    }

    let mut host = Command::new(bin())
        .args(["--headless", "--statedir"])
        .arg(&state)
        .arg("--workdir")
        .arg(&work)
        .current_dir(&t)
        .env("FANTASTIC_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the host starts");
    wait_for(&state.join("cli").join("mcp.yaml"));

    // a client in another directory finds the kernel by --statedir alone
    let sent = Command::new(bin())
        .args(["send", "--statedir"])
        .arg(&state)
        .arg(r#"{"target":"kernel","payload":{"type":"reflect","tree":"none"}}"#)
        .current_dir(&elsewhere)
        .env("FANTASTIC_HOME", &home)
        .output()
        .expect("send runs");
    let said = String::from_utf8_lossy(&sent.stdout).to_string();
    assert!(
        sent.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert!(said.contains("completed"), "{said}");

    // a second start naming another work folder cannot change the running kernel
    let again = Command::new(bin())
        .args(["--headless", "--workdir"])
        .arg(&other)
        .arg("--statedir")
        .arg(&state)
        .current_dir(&t)
        .env("FANTASTIC_HOME", &home)
        .stdin(Stdio::null())
        .output()
        .expect("the second start runs");
    assert_eq!(
        again.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(String::from_utf8_lossy(&again.stderr).contains("cannot change it while it runs"));

    // a polite stop; the work folder never held a state folder
    let _ = Command::new("kill").arg(host.id().to_string()).status();
    let end = Instant::now() + Duration::from_secs(30);
    while host.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = host.kill();
            panic!("the host did not stop on SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        state.join("agent.yaml").is_file(),
        "the state folder holds the root record"
    );
    assert!(
        !work.join(".fantastic").exists(),
        "no state folder in the work folder"
    );
    assert!(!other.join(".fantastic").exists());
    assert!(
        !state.join("cli").join("mcp.yaml").exists(),
        "the endpoint file is removed at a clean stop"
    );
    let _ = std::fs::remove_dir_all(&t);
}
