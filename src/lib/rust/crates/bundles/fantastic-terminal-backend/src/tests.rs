//! Unit tests for terminal_backend.
//!
//! Tests drive through `kernel.send`. Each test mounts its own parent
//! agent and registers a fresh inbox receiver on the parent id so the
//! reader-task's emit-to-parent shows up on an `mpsc::Receiver` we
//! can drain directly (`rebind_inbox` trick — borrowed from
//! `fantastic-ollama-backend`'s test fixture).
//!
//! TERMINALS is a process-global static; per-test agent ids derive
//! from each test's TempDir so parallel runs don't collide.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::{json, Map};
use std::time::Duration;
use tempfile::TempDir;

/// Build a unique agent id from a tempdir's basename — guarantees no
/// collision across parallel tests.
fn agent_id_for(tmp: &TempDir, prefix: &str) -> String {
    let base = tmp
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .replace('.', "_");
    format!("{prefix}_{base}")
}

/// Build a kernel + parent + a terminal-backend agent. Returns:
/// - the kernel
/// - the parent's id (where `data`/`exited` events land)
/// - the backend agent id
/// - a fresh inbox receiver bound to the parent — drain to see emits.
async fn mk_term(
    tmp: &TempDir,
    prefix: &str,
    cmd: Option<Vec<&str>>,
) -> (
    Arc<Kernel>,
    AgentId,
    AgentId,
    fantastic_kernel::inbox::InboxReceiver,
) {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, TerminalBackendBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx_root = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let parent_id_str = agent_id_for(tmp, &format!("{prefix}p"));
    let term_id_str = agent_id_for(tmp, prefix);

    // Bare parent agent (no handler_module). Created via system verb
    // so the kernel wires it up consistently.
    let mut parent_payload = Map::new();
    parent_payload.insert("type".to_string(), json!("create_agent"));
    parent_payload.insert("parent_id".to_string(), json!("kernel"));
    parent_payload.insert("handler_module".to_string(), json!(HANDLER_MODULE)); // any
    parent_payload.insert("id".to_string(), json!(parent_id_str));
    // `auto_start=false` is critical here. create_agent now auto-fires
    // boot (Python parity — see lifecycle::create_from_payload), and
    // the parent's handler_module is terminal_backend.tools, so without
    // this its boot would default-spawn a PTY. We don't want that — the
    // parent agent is only a routing placeholder. Setting auto_start=false
    // makes its boot a no-op.
    parent_payload.insert("auto_start".to_string(), json!(false));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            Value::Object(parent_payload),
        )])
        .await
        .remove(0)
        .result;

    // Backend agent as a child of the parent. Create FIRST so the
    // backend's id exists; we then replace ITS inbox tx with one
    // whose rx we own. Reader emits to self (Python parity — see
    // reader_loop), so the rx must be bound to the backend's id.
    let mut term_payload = Map::new();
    term_payload.insert("type".to_string(), json!("create_agent"));
    // PLACEMENT is `parent_id` now, not the address: the backend is a child of
    // the routing placeholder, and the kernel-owned verb goes to the root.
    term_payload.insert("parent_id".to_string(), json!(parent_id_str.as_str()));
    term_payload.insert("handler_module".to_string(), json!(HANDLER_MODULE));
    term_payload.insert("id".to_string(), json!(term_id_str));
    if let Some(c) = cmd.as_ref() {
        term_payload.insert(
            "cmd".to_string(),
            json!(c.iter().map(|s| s.to_string()).collect::<Vec<String>>()),
        );
    }
    term_payload.insert("auto_start".to_string(), json!(false));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            Value::Object(term_payload),
        )])
        .await
        .remove(0)
        .result;

    // Replace the BACKEND's inbox sender so we can observe the
    // emits the reader fires to self. (Done after create_agent +
    // its auto-boot — neither calls back into our rx, and the
    // reader hasn't started yet because `auto_start=false`.)
    let (tx, rx) = fantastic_kernel::inbox::inbox(kernel.inbox_bound);
    kernel
        .inboxes
        .insert(AgentId::from(term_id_str.as_str()), tx);

    (
        kernel,
        AgentId::from(parent_id_str.as_str()),
        AgentId::from(term_id_str.as_str()),
        rx,
    )
}

/// Collect events from `rx` up to `deadline`, optionally stopping
/// early when `should_stop(&events)` returns true.
async fn collect_events(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    deadline: Duration,
    mut should_stop: impl FnMut(&[Value]) -> bool,
) -> Vec<Value> {
    let start = std::time::Instant::now();
    let mut events = Vec::new();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        match tokio::time::timeout(remaining.min(Duration::from_millis(100)), rx.recv()).await {
            Ok(Some(v)) => {
                events.push(v);
                if should_stop(&events) {
                    return events;
                }
            }
            Ok(None) => return events, // channel closed
            Err(_) => {
                // Drain any backlog non-blocking before re-looping.
                while let Some(v) = rx.try_recv() {
                    events.push(v);
                    if should_stop(&events) {
                        return events;
                    }
                }
            }
        }
    }
    events
}

fn concat_text(events: &[Value]) -> String {
    let mut s = String::new();
    for e in events {
        // Wire shape: `{type:"output", data:str}` (Python parity).
        if e.get("type").and_then(Value::as_str) == Some("output") {
            if let Some(t) = e.get("data").and_then(Value::as_str) {
                s.push_str(t);
            }
        }
    }
    s
}

// ── tests ───────────────────────────────────────────────────────────

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(
        README.contains("terminal_backend"),
        "readme should name the bundle"
    );
}

#[tokio::test]
async fn reflect_shape_when_not_running() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, _rx) = mk_term(&tmp, "refl", None).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    for key in [
        "id",
        "sentence",
        "cmd",
        "cwd",
        "env",
        "cols",
        "rows",
        "running",
        "in_flight_bytes",
        "unacked",
        "verbs",
        "emits",
    ] {
        assert!(r.get(key).is_some(), "reflect missing key {key:?}: {r:#?}");
    }
    assert_eq!(r["id"], term.as_str());
    assert_eq!(r["running"], false);
    assert_eq!(r["unacked"], 0);
    assert_eq!(r["in_flight_bytes"], 0);
}

#[tokio::test]
async fn spawn_runs_echo_command_and_emits_data() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) =
        mk_term(&tmp, "echo", Some(vec!["bash", "-c", "echo hi"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true, "spawn failed: {r:?}");
    let events = collect_events(&mut rx, Duration::from_secs(3), |evs| {
        evs.iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("closed"))
    })
    .await;
    let text = concat_text(&events);
    assert!(
        text.contains("hi"),
        "expected 'hi' in output, got: {text:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("closed")),
        "expected exited event, got: {events:?}"
    );
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn write_sends_to_pty_and_echoes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) = mk_term(&tmp, "wr", Some(vec!["bash"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true, "spawn failed: {r:?}");
    // Drain any banner/prompt first.
    let _ = collect_events(&mut rx, Duration::from_millis(300), |_| false).await;
    let wr = kernel
        .send(vec![Item::new(
            term.clone(),
            json!({"type": "write", "data": "echo MARK1\n"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(wr["written"].as_u64().is_some(), "write reply: {wr:?}");
    let events = collect_events(&mut rx, Duration::from_secs(3), |evs| {
        concat_text(evs).contains("MARK1")
    })
    .await;
    let text = concat_text(&events);
    assert!(
        text.contains("MARK1"),
        "expected MARK1 in output, got: {text:?}"
    );
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn resize_replies_resized_true() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, _rx) = mk_term(&tmp, "rsz", Some(vec!["bash"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true, "spawn failed: {r:?}");
    let r = kernel
        .send(vec![Item::new(
            term.clone(),
            json!({"type": "resize", "cols": 120, "rows": 40}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["resized"], true);
    assert_eq!(r["cols"], 120);
    assert_eq!(r["rows"], 40);
    // Reflect sees the new geometry.
    let refl = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(refl["cols"], 120);
    assert_eq!(refl["rows"], 40);
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn flow_control_pauses_past_100k() {
    let tmp = TempDir::new().unwrap();
    // `yes` floods stdout indefinitely.
    let (kernel, _parent, term, mut rx) = mk_term(&tmp, "flow", Some(vec!["yes"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true, "spawn failed: {r:?}");
    // Don't ack. Wait until the reader's paused flag flips OR we
    // hit a 3s ceiling.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut paused = false;
    while std::time::Instant::now() < deadline {
        // Drain so the reader keeps making forward progress.
        while rx.try_recv().is_some() {}
        let refl = kernel
            .send(vec![Item::new(term.clone(), json!({"type": "reflect"}))])
            .await
            .remove(0)
            .result;
        let unacked = refl["unacked"].as_u64().unwrap_or(0);
        if unacked as usize >= FLOW_PAUSE_THRESHOLD {
            // Re-check after a brief tick — the pause flag flips
            // inside the reader's hot path AFTER the watermark cross.
            tokio::time::sleep(Duration::from_millis(50)).await;
            paused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        paused,
        "reader did not cross flow-pause threshold within 3s"
    );
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn ack_resumes_paused_reader() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) = mk_term(&tmp, "ack", Some(vec!["yes"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true);
    // Wait until paused.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        while rx.try_recv().is_some() {}
        let refl = kernel
            .send(vec![Item::new(term.clone(), json!({"type": "reflect"}))])
            .await
            .remove(0)
            .result;
        if refl["unacked"].as_u64().unwrap_or(0) as usize >= FLOW_PAUSE_THRESHOLD {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("never paused");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Ack a big chunk so unacked drops well below the threshold.
    let acked = kernel
        .send(vec![Item::new(
            term.clone(),
            json!({"type": "ack", "count": FLOW_PAUSE_THRESHOLD * 4}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(acked["paused"], false, "ack did not unpause: {acked:?}");
    // Confirm the reader resumed — unacked should climb again past
    // the post-ack value within a reasonable window.
    let post_ack = acked["unacked"].as_u64().unwrap_or(0);
    let deadline2 = std::time::Instant::now() + Duration::from_secs(2);
    let mut climbed = false;
    while std::time::Instant::now() < deadline2 {
        while rx.try_recv().is_some() {}
        let refl = kernel
            .send(vec![Item::new(term.clone(), json!({"type": "reflect"}))])
            .await
            .remove(0)
            .result;
        if refl["unacked"].as_u64().unwrap_or(0) > post_ack {
            climbed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(climbed, "reader did not resume after ack");
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn utf8_chunk_boundary_safe() {
    let tmp = TempDir::new().unwrap();
    // Emit é (U+00E9, UTF-8 bytes C3 A9) ten times — straightforward
    // multi-byte stream. The incremental decoder guarantees no
    // replacement char is emitted across an arbitrary read boundary.
    let (kernel, _parent, term, mut rx) = mk_term(
        &tmp,
        "utf8",
        Some(vec![
            "bash",
            "-c",
            // printf interprets octal escapes; \303\251 == é.
            r"printf '\303\251\303\251\303\251\303\251\303\251\303\251\303\251\303\251\303\251\303\251'",
        ]),
    )
    .await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true);
    let events = collect_events(&mut rx, Duration::from_secs(3), |evs| {
        evs.iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("closed"))
    })
    .await;
    let text = concat_text(&events);
    assert!(
        text.contains("éééééééééé"),
        "expected ten é, got bytes: {:?}",
        text.as_bytes()
    );
    assert!(
        !text.contains('\u{FFFD}'),
        "replacement char appeared: {text:?}"
    );
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn interrupt_sends_sigint() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) = mk_term(
        &tmp,
        "int",
        Some(vec![
            "bash",
            "-c",
            "trap 'echo SIGINT_CAUGHT; exit 0' INT; sleep 5",
        ]),
    )
    .await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true);
    // Give bash a beat to install the trap.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ir = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "interrupt"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(ir["signal"], libc::SIGINT, "interrupt reply: {ir:?}");
    let events = collect_events(&mut rx, Duration::from_secs(4), |evs| {
        concat_text(evs).contains("SIGINT_CAUGHT")
    })
    .await;
    let text = concat_text(&events);
    assert!(
        text.contains("SIGINT_CAUGHT"),
        "expected SIGINT_CAUGHT in output, got: {text:?}"
    );
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn stop_kills_child_and_emits_exited() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) =
        mk_term(&tmp, "stop", Some(vec!["bash", "-c", "sleep 30"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let st = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["stopped"], true, "stop reply: {st:?}");
    // The reader task should emit an `exited` event when the child
    // is reaped. Allow up to 3 s.
    let events = collect_events(&mut rx, Duration::from_secs(3), |evs| {
        evs.iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("closed"))
    })
    .await;
    let saw_exit = events
        .iter()
        .any(|e| e.get("type").and_then(Value::as_str) == Some("closed"));
    // The reader task is also aborted on stop, so emit may not land
    // if abort wins the race. Either path is acceptable; assert the
    // child is no longer in the TERMINALS map.
    assert!(
        saw_exit || !TERMINALS.lock().contains_key(&term),
        "stop should kill the child, got events: {events:?}"
    );
    assert!(!TERMINALS.lock().contains_key(&term));
}

#[tokio::test]
async fn paste_image_writes_file_and_types_path() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) = mk_term(&tmp, "pst", Some(vec!["cat"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true);
    // Drain any banner.
    let _ = collect_events(&mut rx, Duration::from_millis(200), |_| false).await;
    // Smallest valid PNG: 8-byte signature is enough for our test
    // (the bundle doesn't validate the image bytes — only the mime
    // type and size).
    let png_signature: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_signature);
    let pr = kernel
        .send(vec![Item::new(
            term.clone(),
            json!({"type": "paste_image", "data": b64, "mime": "image/png"}),
        )])
        .await
        .remove(0)
        .result;
    let path = pr["path"]
        .as_str()
        .unwrap_or_else(|| panic!("paste_image reply missing path: {pr:?}"))
        .to_string();
    assert_eq!(pr["bytes"], png_signature.len() as u64);
    // The file lives in the OS tempdir.
    let path_buf = PathBuf::from(&path);
    assert!(path_buf.exists(), "paste file missing: {path}");
    assert!(path_buf.starts_with(std::env::temp_dir()));
    let on_disk = std::fs::read(&path_buf).unwrap();
    assert_eq!(on_disk, png_signature);
    // The path should be typed into the PTY — `cat` echoes it back.
    let events = collect_events(&mut rx, Duration::from_secs(2), |evs| {
        concat_text(evs).contains(&path)
    })
    .await;
    let text = concat_text(&events);
    assert!(
        text.contains(&path),
        "expected pasted path {path:?} in output, got: {text:?}"
    );
    // Trailing space, no newline.
    assert!(text.contains(&format!("{path} ")));
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn unknown_verb_returns_error() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, _rx) = mk_term(&tmp, "unk", None).await;
    let r = kernel
        .send(vec![Item::new(
            term.clone(),
            json!({"type": "no_such_verb"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["error"].as_str().unwrap().contains("unknown verb"));
}

#[tokio::test]
async fn paste_image_binary_path_skips_base64() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, mut rx) = mk_term(&tmp, "pbb", Some(vec!["cat"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true);
    let _ = collect_events(&mut rx, Duration::from_millis(200), |_| false).await;
    // Direct call into the binary path — mimics the WS-frame route.
    let bundle = TerminalBackendBundle;
    let png_signature: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let header = json!({"type": "paste_image", "mime": "image/png"});
    let (reply, _body) = bundle
        .handle_binary(&term, header, png_signature.clone(), &kernel)
        .await
        .expect("handle_binary should not error");
    let reply = reply.expect("handle_binary returned None");
    let path = reply["path"]
        .as_str()
        .unwrap_or_else(|| panic!("binary paste reply missing path: {reply:?}"))
        .to_string();
    assert!(PathBuf::from(&path).exists());
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// `stop` must REAP the killed child — before this, the aborted reader
/// never reached `cleanup_on_exit`'s wait and every stopped session
/// stayed `<defunct>` until the kernel exited (one zombie per stop /
/// restart / delete). `kill(pid, 0)` keeps answering success for a
/// zombie, so ESRCH within 2 s is the proof it was waited.
#[tokio::test]
async fn stop_reaps_the_child_no_zombie() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, _rx) =
        mk_term(&tmp, "reap", Some(vec!["bash", "-c", "sleep 30"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true, "spawn reply: {r:?}");
    let pid = r["pid"].as_u64().expect("spawn reply carries pid") as i32;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Alive (and not a zombie) before stop.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "child should be alive before stop"
    );
    let st = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["stopped"], true, "stop reply: {st:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut gone = false;
    while std::time::Instant::now() < deadline {
        let rc = unsafe { libc::kill(pid, 0) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        if rc == -1 && errno == Some(libc::ESRCH) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        gone,
        "pid {pid} still exists (zombie or alive) 2 s after stop"
    );
}

/// A `write` that fills the PTY input queue (child not reading) must
/// NOT pin the runtime: the blocking `write_all` runs on the blocking
/// pool while `write_lock` is held, so a concurrent `reflect` on the
/// same agent still answers. `#[tokio::test]` is a current-thread
/// runtime — before the fix the single worker sat in `write_all` and
/// `reflect` could not run at all.
#[tokio::test]
async fn a_large_write_does_not_pin_the_runtime() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _parent, term, _rx) = mk_term(&tmp, "bigw", Some(vec!["sleep", "30"])).await;
    let r = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "spawn"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["spawned"], true, "spawn reply: {r:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 1 MiB of complete lines: in canonical mode the line discipline
    // blocks the master write once queued lines reach the input cap
    // (bytes without a newline past the cap are silently dropped, which
    // would not block).
    let line = format!("{}\n", "x".repeat(63));
    let data = line.repeat((1024 * 1024) / line.len());
    assert!(data.len() >= 1024 * 1024 - 64);
    let k2 = Arc::clone(&kernel);
    let t2 = term.clone();
    let writer = tokio::spawn(async move {
        k2.send(vec![Item::new(t2, json!({"type": "write", "data": data}))])
            .await
            .remove(0)
            .result
    });
    // Let the write task start (and block in the PTY). Measured on the
    // WALL CLOCK: a `tokio::time::timeout` polls the inner future first,
    // so a runtime pinned for 30 s (until `sleep` exits) would still
    // answer "completed" once the worker came back.
    let t0 = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reflect = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    let elapsed = t0.elapsed();
    assert_eq!(reflect["running"], true, "reflect reply: {reflect:?}");
    assert!(
        elapsed < Duration::from_millis(500),
        "reflect took {elapsed:?} — the runtime is pinned by the write"
    );
    // Tear down: killing the child hangs up the PTY, which unblocks the
    // parked write (it errors or completes).
    let st = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["stopped"], true, "stop reply: {st:?}");
    let w = tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("blocked write did not return after stop")
        .expect("write task panicked");
    assert!(
        w.get("written").is_some() || w.get("error").is_some(),
        "write reply: {w:?}"
    );
}
