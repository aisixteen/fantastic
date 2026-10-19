//! The OS layer against the real OS.

use std::time::Duration;

use fantastic_home::Canvas;
use fantastic_kernel::lock;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use super::*;

fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().expect("a temp dir");
    let p = std::fs::canonicalize(d.path()).expect("it resolves");
    (d, p)
}

#[test]
fn lock_holder_none_without_a_lock_and_self_with_one() {
    let (_d, dir) = temp();
    let path = dir.display().to_string();
    assert_eq!(lock_holder(&path), None);
    lock::acquire(&dir).expect("the lock");
    assert_eq!(lock_holder(&path), Some(std::process::id()));
    lock::release(&dir).expect("released");
    assert_eq!(lock_holder(&path), None);
    // a stale file whose pid is dead is no holder
    std::fs::write(dir.join(".fantastic/lock.yaml"), "pid: 2147483646\n").unwrap();
    assert_eq!(lock_holder(&path), None);
}

#[test]
fn run_file_parses_only_a_valid_file() {
    let (_d, dir) = temp();
    let path = dir.display().to_string();
    assert_eq!(run_file(&path), None, "no file");
    let file = dir.join(RUN_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "pid: 77\nport: 8123\nurl: http://127.0.0.1:8123/canvas_page/file/index.html\n",
    )
    .unwrap();
    let run = run_file(&path).expect("a valid run file");
    assert_eq!((run.pid, run.port), (77, 8123));
    std::fs::write(&file, "pid: 77\nport: 8124\nurl: http://127.0.0.1:8123/\n").unwrap();
    assert_eq!(run_file(&path), None, "a url on another port");
    std::fs::write(&file, [0xff, 0xfe]).unwrap();
    assert_eq!(run_file(&path), None, "not text");
}

#[test]
fn host_run_file_parses_only_a_valid_file() {
    let good = "pid: 4242\nport: 8999\nurl: http://127.0.0.1:8999/canvas_page/file/home.html\n";
    assert_eq!(
        parse_host_run_file(good),
        Some(HostRun {
            pid: 4242,
            port: 8999,
            url: "http://127.0.0.1:8999/canvas_page/file/home.html".into()
        })
    );
    for bad in [
        "pid: 4242\nport: 8999\nurl: http://127.0.0.1:9000/\n",
        "pid: 4242\nport: 8999\nurl: http://localhost:8999/\n",
        "pid: -1\nport: 8999\nurl: http://127.0.0.1:8999/\n",
        "pid: 4294967296\nport: 8999\nurl: http://127.0.0.1:8999/\n",
        "pid: 4242\nport: 0\nurl: http://127.0.0.1:0/\n",
        "pid: 4242\nport: 70000\nurl: http://127.0.0.1:70000/\n",
        "pid: '4242'\nport: 8999\nurl: http://127.0.0.1:8999/\n",
        "port: 8999\nurl: http://127.0.0.1:8999/\n",
        "- pid: 1\n",
        "",
    ] {
        assert_eq!(parse_host_run_file(bad), None, "{bad}");
    }
    let (_d, home) = temp();
    assert_eq!(read_host_run_file(&home), None, "no file");
    write_atomic(&host_run_file(&home), good.as_bytes()).unwrap();
    assert_eq!(read_host_run_file(&home).map(|r| r.port), Some(8999));
    remove_file(&host_run_file(&home)).unwrap();
    remove_file(&host_run_file(&home)).expect("a missing file is removed already");
}

/// A listener answering every connection with `reply` (`None`: close at once;
/// `Some("")`: stay silent).
async fn http_stub(reply: Option<&'static str>) -> u16 {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                match reply {
                    None => {}
                    Some("") => tokio::time::sleep(Duration::from_secs(10)).await,
                    Some(r) => {
                        let _ = s.write_all(r.as_bytes()).await;
                    }
                }
            });
        }
    });
    port
}

fn canvas(port: u16) -> Canvas {
    Canvas {
        url: format!("http://127.0.0.1:{port}/canvas_page/file/index.html"),
        port,
    }
}

#[tokio::test]
async fn canvas_up_reads_the_status_line() {
    let ok = http_stub(Some("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")).await;
    assert!(canvas_up(&canvas(ok)).await);
    let moved = http_stub(Some("HTTP/1.0 302 Found\r\nLocation: /\r\n\r\n")).await;
    assert!(canvas_up(&canvas(moved)).await);
    let broken = http_stub(Some("HTTP/1.1 500 Internal Server Error\r\n\r\n")).await;
    assert!(!canvas_up(&canvas(broken)).await);
    let garbage = http_stub(Some("SSH-2.0-OpenSSH\r\n")).await;
    assert!(!canvas_up(&canvas(garbage)).await);
    let closed = http_stub(None).await;
    assert!(!canvas_up(&canvas(closed)).await);
    let silent = http_stub(Some("")).await;
    let t0 = std::time::Instant::now();
    assert!(!canvas_up(&canvas(silent)).await);
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "the timeout is 1.5 s"
    );
    // nothing listens
    let free = free_port().unwrap();
    assert!(!canvas_up(&canvas(free)).await);
    assert!(
        canvas_up(&Canvas {
            url: format!("http://localhost:{ok}/x?y=1#z"),
            port: ok
        })
        .await
    );
}

#[test]
fn the_status_line_is_http_1x_and_2xx_or_3xx() {
    assert!(status_ok("HTTP/1.1 200 OK"));
    assert!(status_ok("HTTP/1.0 399"));
    assert!(!status_ok("HTTP/1.1 199 Early"));
    assert!(!status_ok("HTTP/1.1 400 Bad"));
    assert!(!status_ok("HTTP/2 200"));
    assert!(!status_ok("HTTP/1.1 2000"));
}

/// A `/user/ws` stub: each call it reads is answered by `answer`.
async fn ws_stub(answer: fn(&Value) -> Vec<Value>) -> u16 {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
                    return;
                };
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let call: Value = serde_json::from_str(&t).unwrap();
                    for frame in answer(&call) {
                        let _ = ws.send(Message::Text(frame.to_string())).await;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn forward_round_trips_skipping_other_frames() {
    let port = ws_stub(|call| {
        assert_eq!(call["type"], json!("call"));
        assert_eq!(call["target"], json!("user"));
        assert_eq!(call["payload"]["type"], json!("forward"));
        let id = call["id"].clone();
        vec![
            json!({"type": "event", "payload": {"type": "state"}}),
            json!({"type": "error", "id": "", "error": "the auto-watch, refused"}),
            json!({"type": "reply", "id": "someone-else", "data": {"no": 1}}),
            json!({"type": "reply", "id": id, "data": {"echo": call["payload"]["payload"], "target": call["payload"]["target"]}}),
        ]
    })
    .await;
    let r = forward(
        port,
        "home",
        json!({"type": "list"}),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(r, Ok(json!({"echo": {"type": "list"}, "target": "home"})));
    // ids are fresh per call
    let r = forward(
        port,
        "home",
        json!({"type": "list"}),
        Duration::from_secs(5),
    )
    .await;
    assert!(r.is_ok());

    let port =
        ws_stub(|call| vec![json!({"type": "error", "id": call["id"], "error": "no such route"})])
            .await;
    assert_eq!(
        forward(port, "home", json!({}), Duration::from_secs(5)).await,
        Err("no such route".into())
    );

    let port = ws_stub(|call| vec![json!({"type": "reply", "id": call["id"], "data": {"error": "denied", "reason": "unauthorized", "hint": "grant it"}})]).await;
    assert_eq!(
        forward(port, "home", json!({}), Duration::from_secs(5)).await,
        Err("denied — grant it".into())
    );
    let port = ws_stub(|call| {
        vec![json!({"type": "reply", "id": call["id"], "data": {"error": "denied"}})]
    })
    .await;
    assert_eq!(
        forward(port, "home", json!({}), Duration::from_secs(5)).await,
        Err("denied".into())
    );

    let port = ws_stub(|_| Vec::new()).await;
    assert_eq!(
        forward(port, "home", json!({}), Duration::from_millis(300)).await,
        Err("the fantastic did not answer".into())
    );

    let free = free_port().unwrap();
    assert!(forward(free, "home", json!({}), Duration::from_secs(2))
        .await
        .is_err());
}

#[test]
fn log_file_name_is_folder_and_hash() {
    let hash = Sha256::digest(b"/home/a/notes");
    let want = format!(
        "notes-{:02x}{:02x}{:02x}{:02x}.log",
        hash[0], hash[1], hash[2], hash[3]
    );
    assert_eq!(log_file_name("/home/a/notes"), want);
    assert_eq!(
        log_file_name("C:\\work\\atlas").split('-').next(),
        Some("atlas")
    );
    assert_ne!(
        log_file_name("/a/notes"),
        log_file_name("/b/notes"),
        "one name per folder"
    );
}

#[test]
fn the_tail_is_the_last_lines_cut_to_the_last_characters() {
    assert_eq!(tail_of("1\n2\n3\n4\n5\n6\n"), "3\n4\n5\n6");
    assert_eq!(tail_of("only"), "only");
    let long = "x".repeat(700);
    assert_eq!(tail_of(&long).chars().count(), 600);
    let wide = "é".repeat(700);
    assert_eq!(tail_of(&wide), "é".repeat(600));
}

#[cfg(unix)]
mod unix_only {
    use std::ffi::OsString;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    use super::super::*;
    use super::temp;

    #[test]
    fn login_env_parses_after_the_marker() {
        let printed = b"motd noise\n__FANTASTIC_LOGIN_ENV__PATH=/usr/bin:/bin\0HOME=/h\0=bad\0NOEQ\0EMPTY=\0WITH=a=b\0";
        let env = parse_login_env(printed).unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("PATH"), Some(OsString::from("/usr/bin:/bin")));
        assert_eq!(get("HOME"), Some(OsString::from("/h")));
        assert_eq!(get("EMPTY"), Some(OsString::from("")));
        assert_eq!(get("WITH"), Some(OsString::from("a=b")));
        assert_eq!(env.len(), 4);
        assert!(parse_login_env(b"no marker at all")
            .unwrap_err()
            .contains("printed none"));
        assert!(parse_login_env(b"__FANTASTIC_LOGIN_ENV__HOME=/h\0")
            .unwrap_err()
            .contains("no PATH"));
    }

    #[tokio::test]
    async fn terminate_pid_sends_sigterm() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep runs");
        let pid = child.id();
        assert_eq!(terminate_pid(Path::new("/unused"), pid).await, Ok(true));
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        // reaped: no such process
        assert_eq!(terminate_pid(Path::new("/unused"), pid).await, Ok(false));
    }

    #[tokio::test]
    async fn the_spawned_binary_is_config_fantastic_bin() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, dir) = temp();
        let out = dir.join("ran.txt");
        let bin = dir.join("fantastic");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s' \"$*\" \"$PWD\" \"$FANTASTIC_HOME\" > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let folder = dir.join("notes");
        std::fs::create_dir(&folder).unwrap();
        let home = dir.join("home");
        let log = dir.join("notes.log");
        let env = vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))];
        spawn(&bin, folder.to_str().unwrap(), &env, &home, &log)
            .await
            .expect("spawned");
        let mut text = String::new();
        for _ in 0..500 {
            if let Ok(t) = std::fs::read_to_string(&out) {
                if !t.is_empty() {
                    text = t;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            text,
            format!("canvas|{}|{}", folder.display(), home.display())
        );
        assert!(log.is_file(), "stderr goes to the log");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&log).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn folder_is_canonical_and_a_directory() {
        let (_d, dir) = temp();
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        assert_eq!(
            folder(dir.join("link").to_str().unwrap()),
            Ok(real.display().to_string())
        );
        std::fs::write(dir.join("file"), b"").unwrap();
        assert!(folder(dir.join("file").to_str().unwrap())
            .unwrap_err()
            .contains("is not a directory"));
        assert!(folder(dir.join("missing").to_str().unwrap()).is_err());
    }
}
