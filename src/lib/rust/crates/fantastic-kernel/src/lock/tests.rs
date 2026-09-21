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
    // A very high pid unlikely to exist.
    let probably_dead = u32::MAX - 1;
    assert!(!pid_alive(probably_dead));
}

#[test]
fn pid_zero_is_not_alive() {
    // C83: python `_pid_alive`: `pid <= 0` is dead, no syscall at all — unlike
    // unix `kill(0, 0)`, which signals the CALLER'S OWN process group and
    // would read back as "alive" without the explicit `pid == 0` guard.
    assert!(!pid_alive(0));
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
fn release_keeps_a_malformed_lock() {
    // C83: python `release_lock`'s `_read_lock()` returns `None` on ANY parse
    // failure, so `cur and cur.get("pid") == os.getpid()` is false and the
    // file is left ALONE — release must never blindly `remove_file` a lock it
    // could not verify it owns.
    let tmp = TempDir::new().unwrap();
    let lock_path = tmp.path().join(crate::persistence::LOCK_PATH);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    std::fs::write(&lock_path, "{NOT_JSON").unwrap();
    release(tmp.path()).unwrap();
    assert!(lock_path.exists(), "a malformed lock survives release()");
}

#[test]
fn lock_held_text_is_python_verbatim() {
    // C81: python `_lock.py:52-64`'s exact 3 lines — `RCLI/src/main.rs`
    // prints this as `[kernel] {e}`, never the generic error wrapper.
    let err = KernelError::LockHeld { pid: 4242 };
    assert_eq!(
        err.to_string(),
        "another fantastic owns this dir: pid=4242\n  \
         -> kill it:  kill 4242\n  \
         -> or, if stale, remove the lock:  rm .fantastic/lock.json"
    );
}

#[test]
fn lock_bytes_are_python_json_dumps() {
    // C50: python `json.dumps({"pid": os.getpid()})` uses the DEFAULT
    // separators — `: ` after the key — not serde_json's compact `:`.
    let tmp = TempDir::new().unwrap();
    let path = acquire(tmp.path()).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert_eq!(raw, format!("{{\"pid\": {}}}", std::process::id()));
}

#[test]
fn current_holder_returns_pid() {
    let tmp = TempDir::new().unwrap();
    assert!(current_holder(tmp.path()).unwrap().is_none());
    acquire(tmp.path()).unwrap();
    let pid = current_holder(tmp.path()).unwrap().expect("locked");
    assert_eq!(pid, std::process::id());
}
