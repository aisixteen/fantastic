//! Unit tests for [`crate::lock`].

use super::*;
use tempfile::TempDir;

#[test]
fn pid_alive_self_returns_true() {
    let me = std::process::id();
    assert!(pid_alive(me));
}

#[test]
fn pid_alive_clearly_dead_returns_false() {
    // PID 0 is reserved on unix; kill(0, 0) signals the process
    // group and isn't a clean "is this PID alive" check. Pick a
    // very high pid unlikely to exist.
    let probably_dead = u32::MAX - 1;
    assert!(!pid_alive(probably_dead));
}

#[test]
fn acquire_writes_lock_file() {
    let tmp = TempDir::new().unwrap();
    let path = acquire(tmp.path()).unwrap();
    assert!(path.exists());
    let raw = std::fs::read_to_string(&path).unwrap();
    let lock: LockFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(lock.pid, std::process::id());
}

#[test]
fn acquire_refuses_when_live_pid_holds() {
    let tmp = TempDir::new().unwrap();
    // Pretend a live process (ourselves) owns the lock.
    acquire(tmp.path()).unwrap();
    let err = acquire(tmp.path()).unwrap_err();
    match err {
        KernelError::LockHeld { pid } => assert_eq!(pid, std::process::id()),
        other => panic!("expected LockHeld, got {other:?}"),
    }
}

#[test]
fn acquire_overwrites_stale_lock() {
    let tmp = TempDir::new().unwrap();
    // Plant a stale lock with a clearly-dead PID.
    let lock_path = tmp.path().join(crate::persistence::LOCK_PATH);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    std::fs::write(&lock_path, r#"{"pid": 4294967294}"#).unwrap();
    let acquired = acquire(tmp.path()).unwrap();
    assert_eq!(acquired, lock_path);
    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let lock: LockFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(lock.pid, std::process::id());
}

#[test]
fn release_removes_file() {
    let tmp = TempDir::new().unwrap();
    let path = acquire(tmp.path()).unwrap();
    assert!(path.exists());
    release(tmp.path()).unwrap();
    assert!(!path.exists());
}

#[test]
fn release_when_missing_is_noop() {
    let tmp = TempDir::new().unwrap();
    release(tmp.path()).unwrap();
}

#[test]
fn current_holder_returns_pid() {
    let tmp = TempDir::new().unwrap();
    assert!(current_holder(tmp.path()).unwrap().is_none());
    acquire(tmp.path()).unwrap();
    let pid = current_holder(tmp.path()).unwrap().expect("locked");
    assert_eq!(pid, std::process::id());
}
