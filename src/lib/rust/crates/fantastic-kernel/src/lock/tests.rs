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
    // Python `_pid_alive`: `pid <= 0` is dead, no syscall at all — unlike
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
    let lock: LockFile = parse_lock(&raw).unwrap();
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
fn a_live_pid_without_the_flock_is_taken_over() {
    // The pid in the file is for the message, never the decision: a file naming
    // a live pid that holds no flock (its holder is gone, the pid reused) is
    // taken over.
    let tmp = TempDir::new().unwrap();
    let lock_path = tmp.path().join(crate::persistence::LOCK_PATH);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    std::fs::write(&lock_path, format!("pid: {}\n", std::process::id())).unwrap();
    acquire(tmp.path()).unwrap();
    let raw = std::fs::read_to_string(&lock_path).unwrap();
    assert_eq!(raw, format!("pid: {}\n", std::process::id()));
    release(tmp.path()).unwrap();
}

#[test]
fn acquire_leaves_no_staged_file() {
    let tmp = TempDir::new().unwrap();
    let names = || {
        let mut v: Vec<String> = std::fs::read_dir(tmp.path().join(".fantastic"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    acquire(tmp.path()).unwrap();
    assert_eq!(names(), ["lock.yaml"]);
    assert!(acquire(tmp.path()).is_err());
    assert_eq!(names(), ["lock.yaml"]);
    release(tmp.path()).unwrap();
}

#[test]
fn release_lets_the_next_acquire_in() {
    let tmp = TempDir::new().unwrap();
    acquire(tmp.path()).unwrap();
    release(tmp.path()).unwrap();
    acquire(tmp.path()).unwrap();
    release(tmp.path()).unwrap();
}

#[test]
fn two_racing_acquisitions_exactly_one_wins() {
    // Two acquisitions racing in one dir: each racer opens its own descriptor,
    // and flock excludes descriptors of one process as it excludes processes —
    // so every
    // round, on a fresh dir and over a dead holder's lock alike, exactly one
    // wins and the other is refused. On a fresh dir the refusal names the
    // winner's pid; over a dead lock it may name the dead pid (the winner
    // holds the dead file while it replaces it).
    for planted in [None, Some("pid: 4294967294\n")] {
        for _ in 0..20 {
            let tmp = TempDir::new().unwrap();
            if let Some(body) = planted {
                let lock_path = tmp.path().join(crate::persistence::LOCK_PATH);
                std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
                std::fs::write(&lock_path, body).unwrap();
            }
            let gate = std::sync::Barrier::new(2);
            let results: Vec<KernelResult<PathBuf>> = std::thread::scope(|s| {
                let racers: Vec<_> = (0..2)
                    .map(|_| {
                        s.spawn(|| {
                            gate.wait();
                            acquire(tmp.path())
                        })
                    })
                    .collect();
                racers.into_iter().map(|r| r.join().unwrap()).collect()
            });
            let won = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(won, 1, "{results:?}");
            for r in &results {
                if let Err(e) = r {
                    assert!(
                        matches!(e, KernelError::LockHeld { pid }
                            if planted.is_some() || *pid == std::process::id()),
                        "{e:?}"
                    );
                }
            }
            release(tmp.path()).unwrap();
        }
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
    let lock: LockFile = parse_lock(&raw).unwrap();
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
    // Python `release_lock`'s `_read_lock()` returns `None` on ANY parse
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
    // Python `_lock.py:52-64`'s exact 3 lines — `RCLI/src/main.rs`
    // prints this as `[kernel] {e}`, never the generic error wrapper.
    let err = KernelError::LockHeld { pid: 4242 };
    assert_eq!(
        err.to_string(),
        "another fantastic owns this dir: pid=4242\n  \
         -> kill it:  kill 4242\n  \
         -> or, if stale, remove the lock:  rm .fantastic/lock.yaml"
    );
}

#[test]
fn lock_bytes_are_python_yaml_dump() {
    // python `yaml.dump({"pid": os.getpid()}, CSafeDumper, sort_keys=False)`.
    let tmp = TempDir::new().unwrap();
    let path = acquire(tmp.path()).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert_eq!(raw, format!("pid: {}\n", std::process::id()));
}

#[test]
fn current_holder_returns_pid() {
    let tmp = TempDir::new().unwrap();
    assert!(current_holder(tmp.path()).unwrap().is_none());
    acquire(tmp.path()).unwrap();
    let pid = current_holder(tmp.path()).unwrap().expect("locked");
    assert_eq!(pid, std::process::id());
}

#[test]
fn held_names_the_holder_while_the_flock_is_held() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(held(tmp.path()).unwrap(), None, "no lock file");
    acquire(tmp.path()).unwrap();
    assert_eq!(held(tmp.path()).unwrap(), Some(std::process::id()));
    // the probe took nothing: the holder still holds, a second acquire is refused
    assert!(matches!(
        acquire(tmp.path()).unwrap_err(),
        KernelError::LockHeld { .. }
    ));
    release(tmp.path()).unwrap();
    assert_eq!(held(tmp.path()).unwrap(), None, "after release");
}

#[test]
fn held_is_none_for_a_live_pid_without_the_flock() {
    // A file a dead holder left, naming a pid that is alive (reused): not held.
    let tmp = TempDir::new().unwrap();
    let lock_path = tmp.path().join(crate::persistence::LOCK_PATH);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    std::fs::write(&lock_path, format!("pid: {}\n", std::process::id())).unwrap();
    assert!(pid_alive(std::process::id()));
    assert_eq!(held(tmp.path()).unwrap(), None);
    // and the probe left it takeable
    acquire(tmp.path()).unwrap();
    release(tmp.path()).unwrap();
}
