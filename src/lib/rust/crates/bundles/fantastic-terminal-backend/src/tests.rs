//! Unit tests for terminal_backend — the python `terminal_backend.tools`
//! twins (plan §2 A6).
//!
//! Tests drive through `kernel.send`. Each test mounts its own agent and
//! rebinds that agent's inbox so the reader task's emits land on an
//! `mpsc::Receiver` we own (events go to SELF, python parity).
//!
//! `PROCS` is a process-global static; per-test agent ids derive from each
//! test's TempDir so parallel runs don't collide.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

/// Build a unique agent id from a tempdir's basename — no collision across
/// parallel tests.
fn agent_id_for(tmp: &TempDir, prefix: &str) -> String {
    let base = tmp
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .replace('.', "_");
    format!("{prefix}_{base}")
}

/// Kernel + one terminal agent seeded with `meta`, STOPPED (create_agent
/// auto-boots, python parity), with its inbox rebound so the reader's emits
/// are observable from the first `boot` the test itself sends.
async fn mk_term(
    tmp: &TempDir,
    prefix: &str,
    meta: Value,
) -> (Arc<Kernel>, AgentId, fantastic_kernel::inbox::InboxReceiver) {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, TerminalBackendBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let term_id_str = agent_id_for(tmp, prefix);
    let mut payload = Map::new();
    payload.insert("type".to_string(), json!("create_agent"));
    payload.insert("parent_id".to_string(), json!("kernel"));
    payload.insert("handler_module".to_string(), json!(HANDLER_MODULE));
    payload.insert("id".to_string(), json!(term_id_str));
    if let Some(obj) = meta.as_object() {
        for (k, v) in obj {
            payload.insert(k.clone(), v.clone());
        }
    }
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            Value::Object(payload),
        )])
        .await
        .remove(0)
        .result;
    let term = AgentId::from(term_id_str.as_str());
    // create_agent auto-boots; stop so the test owns the PTY lifecycle.
    let _ = kernel
        .send(vec![Item::new(term.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    let rx = kernel.ensure_inbox(&term);
    (kernel, term, rx)
}

/// `boot`, then python's fixture settle (`await asyncio.sleep(0.3)`) — an
/// interactive shell that has not reached its first prompt re-echoes the line
/// readline was still initialising over.
async fn boot(kernel: &Arc<Kernel>, term: &AgentId) -> Value {
    let r = call(kernel, term, json!({"type": "boot"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    r
}

async fn call(kernel: &Arc<Kernel>, term: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(term.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// Collect events up to `deadline`, stopping early on `should_stop`.
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
                events.push(v.payload);
                if should_stop(&events) {
                    return events;
                }
            }
            Ok(None) => return events,
            Err(_) => {
                while let Some(v) = rx.try_recv() {
                    events.push(v.payload);
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
        if e.get("type").and_then(Value::as_str) == Some("output") {
            if let Some(t) = e.get("data").and_then(Value::as_str) {
                s.push_str(t);
            }
        }
    }
    s
}

/// Poll `output` until it contains `needle` (or the budget runs out).
async fn wait_for_output(
    kernel: &Arc<Kernel>,
    term: &AgentId,
    needle: &str,
    budget: Duration,
) -> String {
    let start = std::time::Instant::now();
    let mut last = String::new();
    while start.elapsed() < budget {
        last = call(kernel, term, json!({"type": "output"})).await["output"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if last.contains(needle) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    last
}

// ── A6.2 readme ──────────────────────────────────────────────────────

/// A6.2 — the vendored readme is python's, byte for byte.
#[test]
fn terminal_readme_is_python_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../..")
        .join("src/lib/python/bundled_agents/terminal/src/terminal_backend/readme.md");
    let Ok(src) = std::fs::read_to_string(&py) else {
        eprintln!("skipping: python tree not present at {}", py.display());
        return;
    };
    assert_eq!(src, README, "readme has drifted from {}", py.display());
}

// ── A6.6 / A6.7 / A6.8 reflect ───────────────────────────────────────

/// A6.8 (r2) — reflect's keys are python's, in python's order.
#[tokio::test]
async fn reflect_keys_are_python_keys_in_python_order() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "refl", json!({})).await;
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    // (the kernel appends its own gate keys after the bundle's body)
    let keys: Vec<&str> = r
        .as_object()
        .expect("reflect is an object")
        .keys()
        .map(String::as_str)
        .take(12)
        .collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "command",
            "running",
            "pid",
            "cols",
            "rows",
            "scrollback_bytes",
            "paused",
            "unacked",
            "verbs",
            "emits",
        ]
    );
    assert_eq!(r["sentence"], json!("PTY shell session."));
    assert_eq!(r["pid"], Value::Null);
    assert_eq!(r["running"], json!(false));
    assert_eq!(r["cols"], json!(200));
    assert_eq!(r["rows"], json!(50));
    assert_eq!(r["scrollback_bytes"], json!(0));
    assert_eq!(r["paused"], json!(false));
    assert_eq!(r["unacked"], json!(0));

    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert!(
        r["pid"].is_i64(),
        "a booted terminal reports an int pid: {r}"
    );
    assert_eq!(r["running"], json!(true));
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.6 — the 11 verb docs are python's first docstring lines, in VERBS order.
#[tokio::test]
async fn reflect_verbs_are_python_docstrings_verbatim() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "vrb", json!({})).await;
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    let verbs = r["verbs"].as_object().expect("verbs");
    let names: Vec<&str> = verbs.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec![
            "reflect",
            "boot",
            "write",
            "ack",
            "paste_image",
            "shell",
            "resize",
            "output",
            "restart",
            "signal",
            "stop",
        ]
    );
    assert_eq!(
        verbs["boot"],
        json!("Idempotent. Spawns the PTY child if not already running; clears stale flow-control state so a freshly-mounted streamer starts clean. Returns {running:true}.")
    );
    assert_eq!(
        verbs["signal"],
        json!("args: signal:int? (default SIGINT=2). os.kill(pid, signal). Returns {signal:int} or {error}.")
    );
    assert_eq!(
        verbs["shell"],
        json!("args: cmd:str (req), timeout:float? (default 30s). Synchronous run via done-token; returns {cmd, output, completed:bool, error?}.")
    );
}

/// A6.7 — `emits` is python's two entries, verbatim; no `exited`/`error`.
#[tokio::test]
async fn reflect_emits_is_python_verbatim() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "emt", json!({})).await;
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(
        r["emits"],
        json!({
            "output": "{type:'output', data:str} — every PTY read chunk; UTF-8-decoded bytes",
            "closed": "{type:'closed'} — child process exited (EOF on the PTY fd)",
        })
    );
}

// ── A6.11 spawn from the record ──────────────────────────────────────

/// A6.11 (r2) — the child is the record's `command` + `args`; the defaults are
/// python's (no `-l`, cwd = the kernel's, no record `env`).
#[tokio::test]
async fn spawn_uses_record_command_args_and_python_defaults() {
    // (a) command + args
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(
        &tmp,
        "spa",
        json!({"command": "/bin/sh", "args": ["-c", "printf hi; sleep 5"]}),
    )
    .await;
    let _ = boot(&kernel, &term).await;
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(r["command"], json!("/bin/sh"));
    let out = wait_for_output(&kernel, &term, "hi", Duration::from_secs(3)).await;
    assert!(
        out.contains("hi"),
        "expected the child's output, got {out:?}"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;

    // (b) falsy record fields fall back to python's defaults
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(
        &tmp,
        "spb",
        json!({"command": "", "cols": 0, "rows": 0, "cwd": ""}),
    )
    .await;
    std::env::set_var("SHELL", "/bin/sh");
    let _ = boot(&kernel, &term).await;
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(r["command"], json!("/bin/sh"));
    assert_eq!(r["cols"], json!(200));
    assert_eq!(r["rows"], json!(50));
    let pid = r["pid"].as_i64().expect("pid");
    let args = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    assert_eq!(
        String::from_utf8_lossy(&args.stdout).trim(),
        "/bin/sh",
        "python passes the shell alone — no `-l`"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;

    // (c) a record `env` never reaches the child (python reads no `env`).
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(
        &tmp,
        "spc",
        json!({"command": "/bin/sh", "env": {"FANTASTIC_A6_11": "1"}}),
    )
    .await;
    let _ = boot(&kernel, &term).await;
    let r = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "echo [${FANTASTIC_A6_11:-unset}]", "timeout": 5.0}),
    )
    .await;
    assert!(
        r["output"].as_str().unwrap_or("").contains("[unset]"),
        "record env must not reach the child: {r}"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.23 — a bad `cwd` leaves the child in the kernel's dir (not `$HOME`).
#[tokio::test]
async fn a_bad_cwd_falls_back_to_the_kernel_cwd() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(
        &tmp,
        "cwd",
        json!({"command": "/bin/sh", "cwd": "/no/such/dir"}),
    )
    .await;
    let _ = boot(&kernel, &term).await;
    let r = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "/bin/pwd", "timeout": 5.0}),
    )
    .await;
    let here = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        r["output"].as_str().unwrap_or("").contains(&here),
        "expected the kernel cwd {here:?} in {r}"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.1 — an exec failure happens in the CHILD: it writes python's
/// `f"{cmd}: {exc}\r\n"` to the PTY and exits 127.
#[tokio::test]
async fn exec_failure_writes_pythons_line_in_the_child() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, mut rx) = mk_term(&tmp, "xfa", json!({"command": "/no/such/shell"})).await;
    let r = call(&kernel, &term, json!({"type": "boot"})).await;
    assert_eq!(r, json!({"running": true}));
    let events = collect_events(&mut rx, Duration::from_secs(3), |evs| {
        evs.iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("closed"))
    })
    .await;
    assert!(
        concat_text(&events)
            .contains("/no/such/shell: [Errno 2] No such file or directory: '/no/such/shell'"),
        "expected python's exec-failure line, got {:?}",
        concat_text(&events)
    );
}

// ── A6.12 boot ───────────────────────────────────────────────────────

/// A6.12 — boot is idempotent and replies exactly `{running:true}`.
#[tokio::test]
async fn boot_twice_replies_running_true() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "bt", json!({"command": "/bin/sh"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "boot"})).await,
        json!({"running": true})
    );
    assert_eq!(
        call(&kernel, &term, json!({"type": "boot"})).await,
        json!({"running": true})
    );
    let r = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(r["unacked"], json!(0));
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

// ── A6.4 / A6.5 / A6.10 dispatch edges ───────────────────────────────

/// A6.4 — a non-string `write.data` fails the dispatch exactly as python's
/// `data.encode` does.
#[tokio::test]
async fn a_non_string_write_data_is_a_failed_dispatch() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "wrd", json!({"command": "/bin/sh"})).await;
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    let r = call(&kernel, &term, json!({"type": "write", "data": 1})).await;
    assert_eq!(
        r,
        json!({"error": "AttributeError: 'int' object has no attribute 'encode'", "raised": true})
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.5 / A6.20 — an out-of-range `cols` fails the dispatch AFTER the resize
/// stored it, and the kernel keeps answering.
#[tokio::test]
async fn an_out_of_range_resize_fails_but_stores_first() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "rsz", json!({"command": "/bin/sh"})).await;
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    let r = call(
        &kernel,
        &term,
        json!({"type": "resize", "cols": 70000, "rows": 30}),
    )
    .await;
    assert_eq!(r["raised"], json!(true), "out of range must raise: {r}");
    assert_eq!(
        r["error"],
        json!("error: 'H' format requires 0 <= number <= 65535")
    );
    // stored first, and the kernel still answers.
    let refl = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(refl["cols"], json!(70000));
    assert_eq!(refl["rows"], json!(30));
    // A6.20 — an in-range resize always answers `{resized, cols, rows}`.
    let r = call(
        &kernel,
        &term,
        json!({"type": "resize", "cols": 100, "rows": 30}),
    )
    .await;
    assert_eq!(r, json!({"resized": true, "cols": 100, "rows": 30}));
    let refl = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(refl["cols"], json!(100));
    assert_eq!(refl["rows"], json!(30));
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.10 — an unknown verb (and `shutdown`) is python's `unknown type` text.
#[tokio::test]
async fn unknown_verb_is_pythons_unknown_type_text() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "unk", json!({})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "garbage"})).await,
        json!({"error": "terminal: unknown type 'garbage'"})
    );
    assert_eq!(
        call(&kernel, &term, json!({"type": "shutdown"})).await,
        json!({"error": "terminal: unknown type 'shutdown'"})
    );
    // `spawn` and `interrupt` are gone (A6.9).
    assert_eq!(
        call(&kernel, &term, json!({"type": "spawn"})).await,
        json!({"error": "terminal: unknown type 'spawn'"})
    );
    assert_eq!(
        call(&kernel, &term, json!({"type": "interrupt"})).await,
        json!({"error": "terminal: unknown type 'interrupt'"})
    );
}

// ── A6.13 / A6.14 flow control + output ──────────────────────────────

/// A6.14 — `output` slices CODE POINTS the way python does: 0 is the whole
/// scrollback, a negative count drops that many leading code points.
#[tokio::test]
async fn output_slices_code_points_like_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "osl", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    // Three 3-byte chars: 9 bytes, 3 code points.
    let _ = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "printf '─────'", "timeout": 5.0}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let full = call(&kernel, &term, json!({"type": "output"})).await["output"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        full.contains('─'),
        "expected the box-drawing chars: {full:?}"
    );
    // 0 → the WHOLE string (python's `text[-0:]`).
    let zero = call(&kernel, &term, json!({"type": "output", "max_bytes": 0})).await;
    assert_eq!(zero["output"], json!(full));
    // -n → drop n leading code points.
    let chars: Vec<char> = full.chars().collect();
    let dropped: String = chars[3..].iter().collect();
    let neg = call(&kernel, &term, json!({"type": "output", "max_bytes": -3})).await;
    assert_eq!(neg["output"], json!(dropped));
    // n → the last n code points.
    let tail: String = chars[chars.len() - 4..].iter().collect();
    let cut = call(&kernel, &term, json!({"type": "output", "max_bytes": 4})).await;
    assert_eq!(cut["output"], json!(tail));
    // …and it never touches flow control. (`shell` clears the accounting on its
    // way out, so the unacked chars have to come from a plain `write`.)
    let _ = call(
        &kernel,
        &term,
        json!({"type": "write", "data": "echo osl\n"}),
    )
    .await;
    let _ = wait_for_output(&kernel, &term, "osl", Duration::from_secs(3)).await;
    let before = call(&kernel, &term, json!({"type": "reflect"})).await["unacked"]
        .as_u64()
        .unwrap_or(0);
    assert!(before > 0, "the echo is unacked");
    let _ = call(&kernel, &term, json!({"type": "output"})).await;
    let after = call(&kernel, &term, json!({"type": "reflect"})).await["unacked"]
        .as_u64()
        .unwrap_or(0);
    assert!(after >= before, "output must not ack: {before} -> {after}");
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.13 — the reader pauses past HIGH_WATERMARK and `ack` re-attaches it
/// below LOW_WATERMARK (python `::test_flow_control_pauses_and_resumes`).
#[tokio::test(flavor = "multi_thread")]
async fn flow_control_pauses_and_resumes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "flw", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let _ = call(
        &kernel,
        &term,
        json!({"type": "write", "data": "yes FANTASTIC_FLOW_CONTROL | head -c 400000\n"}),
    )
    .await;
    let mut refl = Value::Null;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        refl = call(&kernel, &term, json!({"type": "reflect"})).await;
        if refl["paused"] == json!(true) {
            break;
        }
    }
    assert_eq!(
        refl["paused"],
        json!(true),
        "the reader must pause once unacked > HIGH_WATERMARK: {refl}"
    );
    assert!(refl["unacked"].as_u64().unwrap_or(0) > 100_000, "{refl}");
    let mut r = Value::Null;
    for _ in 0..200 {
        r = call(&kernel, &term, json!({"type": "ack", "chars": 5000})).await;
        if r["paused"] == json!(false) {
            break;
        }
    }
    assert_eq!(
        r["paused"],
        json!(false),
        "ack must re-attach the reader: {r}"
    );
    assert!(r["unacked"].as_u64().unwrap_or(u64::MAX) < 5_000, "{r}");
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.13 — an ack from a streamer that never hit the watermark floors at zero
/// (python `::test_ack_on_unpaused_terminal_is_harmless`), and `ack` reads
/// `chars` ONLY.
#[tokio::test]
async fn ack_on_an_unpaused_terminal_is_harmless() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "ack", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let r = call(&kernel, &term, json!({"type": "ack", "chars": 999999})).await;
    assert_eq!(r, json!({"unacked": 0, "paused": false}));
    // `count` is not python's key — it is ignored, so nothing is acked.
    let _ = call(
        &kernel,
        &term,
        json!({"type": "write", "data": "echo ackk\n"}),
    )
    .await;
    let _ = wait_for_output(&kernel, &term, "ackk", Duration::from_secs(3)).await;
    let before = call(&kernel, &term, json!({"type": "reflect"})).await["unacked"]
        .as_u64()
        .unwrap_or(0);
    assert!(before > 0);
    let r = call(&kernel, &term, json!({"type": "ack", "count": 5})).await;
    assert_eq!(r["unacked"], json!(before), "only `chars` acks: {r}");
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

// ── A6.3 / A6.22 the decoder ─────────────────────────────────────────

/// A6.3 — every non-empty READ emits `{type:'output', data}`, even when the
/// bytes decode to `''` (a multi-byte char split across two reads).
#[tokio::test(flavor = "multi_thread")]
async fn a_split_multibyte_char_emits_an_empty_output_event() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, mut rx) = mk_term(&tmp, "spl", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    // The lead byte lands in its own read; the two continuation bytes follow
    // 300 ms later, so the first read decodes to the empty string.
    let _ = call(
        &kernel,
        &term,
        json!({"type": "write",
               "data": "/usr/bin/printf '\\342'; sleep 0.3; /usr/bin/printf '\\224\\200'\n"}),
    )
    .await;
    let events = collect_events(&mut rx, Duration::from_secs(4), |evs| {
        concat_text(evs).contains('─')
    })
    .await;
    assert!(
        events.iter().any(|e| {
            e.get("type").and_then(Value::as_str) == Some("output")
                && e.get("data") == Some(&json!(""))
        }),
        "expected an empty-data output event: {events:?}"
    );
    assert!(
        concat_text(&events).contains('─'),
        "the char must survive whole"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.22 — an invalid byte becomes exactly ONE U+FFFD; the decoder never drops
/// input on a full output buffer.
#[tokio::test]
async fn invalid_utf8_chunk_is_fully_replaced() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "bad", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let _ = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "printf 'A\\377\\376B'", "timeout": 5.0}),
    )
    .await;
    let out = wait_for_output(&kernel, &term, "A\u{fffd}\u{fffd}B", Duration::from_secs(3)).await;
    assert!(
        out.contains("A\u{fffd}\u{fffd}B"),
        "each invalid byte is one U+FFFD: {out:?}"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

// ── A6.15 paste_image ────────────────────────────────────────────────

/// A6.15 — the text channel carries no bytes; python says `data must be bytes`.
#[tokio::test]
async fn paste_image_on_the_text_channel_requires_bytes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "pib", json!({"command": "/bin/sh"})).await;
    // not running comes FIRST (python's order).
    assert_eq!(
        call(&kernel, &term, json!({"type": "paste_image", "data": "x"})).await,
        json!({"error": "not running"})
    );
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    assert_eq!(
        call(
            &kernel,
            &term,
            json!({"type": "paste_image", "data": "not-bytes"})
        )
        .await,
        json!({"error": "paste_image: data must be bytes"})
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.15 — the binary leg: the bytes land on disk, the path is typed into the
/// PTY, and the refusals are python's.
#[tokio::test]
async fn paste_image_writes_the_blob_and_types_its_path() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "pim", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let bundle = TerminalBackendBundle;
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    let (reply, body) = bundle
        .handle_binary(
            &term,
            json!({"type": "paste_image", "mime": "image/png"}),
            png.clone(),
            &kernel,
        )
        .await
        .expect("paste");
    // `paste_image` answers a status, never a chunk (C37).
    assert_eq!(body, None);
    let reply = reply.expect("a reply");
    assert_eq!(reply["bytes"], json!(png.len()));
    let path = reply["path"].as_str().expect("path").to_string();
    assert!(path.ends_with(".png"), "{path}");
    assert_eq!(std::fs::read(&path).expect("the blob landed"), png);
    let out = wait_for_output(&kernel, &term, &path, Duration::from_secs(3)).await;
    assert!(
        out.contains(&path),
        "the path is typed into the PTY: {out:?}"
    );

    // an unsupported type — python's repr, and NO `image/jpg`.
    let (reply, _) = bundle
        .handle_binary(
            &term,
            json!({"type": "paste_image", "mime": "image/jpg"}),
            png.clone(),
            &kernel,
        )
        .await
        .expect("paste");
    assert_eq!(
        reply.expect("reply")["error"],
        json!("paste_image: unsupported image type 'image/jpg'")
    );
    // an empty mime is python's `(mime or 'image/png')`.
    let (reply, _) = bundle
        .handle_binary(
            &term,
            json!({"type": "paste_image", "mime": ""}),
            png.clone(),
            &kernel,
        )
        .await
        .expect("paste");
    assert!(reply.expect("reply")["path"]
        .as_str()
        .unwrap_or("")
        .ends_with(".png"));
    // oversize
    let (reply, _) = bundle
        .handle_binary(
            &term,
            json!({"type": "paste_image"}),
            vec![0u8; MAX_PASTE_IMAGE + 1],
            &kernel,
        )
        .await
        .expect("paste");
    assert_eq!(
        reply.expect("reply")["error"],
        json!(format!(
            "paste_image: {} bytes exceeds the 5 MB cap",
            MAX_PASTE_IMAGE + 1
        ))
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

// ── A6.16 / A6.17 / A6.18 signal, stop, exit ─────────────────────────

/// A6.16 — `signal` answers `{signal}` and nothing else.
#[tokio::test]
async fn signal_replies_just_the_signal() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "sig", json!({"command": "/bin/sh"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "signal", "signal": 0})).await,
        json!({"error": "not running"})
    );
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "signal", "signal": 0})).await,
        json!({"signal": 0})
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.17 — `stop` with nothing to stop still answers `{stopped:true}`.
#[tokio::test]
async fn stop_with_nothing_to_stop_is_stopped_true() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "stp", json!({"command": "/bin/sh"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "stop"})).await,
        json!({"stopped": true})
    );
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "stop"})).await,
        json!({"stopped": true})
    );
    assert_eq!(
        call(&kernel, &term, json!({"type": "stop"})).await,
        json!({"stopped": true})
    );
}

/// A6.18 — a child that exits drops the state: pid null, scrollback 0, empty
/// `output`, and `write` answers `not running`.
#[tokio::test(flavor = "multi_thread")]
async fn a_child_that_exits_drops_the_state() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, mut rx) = mk_term(
        &tmp,
        "xit",
        json!({"command": "/bin/sh", "args": ["-c", "printf bye; exit 0"]}),
    )
    .await;
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    let _ = collect_events(&mut rx, Duration::from_secs(3), |evs| {
        evs.iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("closed"))
    })
    .await;
    let refl = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(refl["running"], json!(false));
    assert_eq!(refl["pid"], Value::Null);
    assert_eq!(refl["scrollback_bytes"], json!(0));
    assert_eq!(
        call(&kernel, &term, json!({"type": "output"})).await,
        json!({"output": ""})
    );
    assert_eq!(
        call(&kernel, &term, json!({"type": "write", "data": "x"})).await,
        json!({"error": "not running"})
    );
}

/// A6.19 (r2) — `stop` emits no `closed`, and no `{type:'error'}` event ever
/// reaches the inbox; a child that exits by itself emits exactly one `closed`.
#[tokio::test(flavor = "multi_thread")]
async fn stop_emits_no_closed_and_never_an_error_event() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, mut rx) = mk_term(&tmp, "sec", json!({"command": "/bin/sh"})).await;
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "stop"})).await,
        json!({"stopped": true})
    );
    let events = collect_events(&mut rx, Duration::from_millis(500), |_| false).await;
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();
    assert!(
        !kinds.contains(&"closed") && !kinds.contains(&"exited") && !kinds.contains(&"error"),
        "stop is silent: {kinds:?}"
    );

    let tmp = TempDir::new().unwrap();
    let (kernel, term, mut rx) = mk_term(
        &tmp,
        "sed",
        json!({"command": "/bin/sh", "args": ["-c", "exit 0"]}),
    )
    .await;
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    let events = collect_events(&mut rx, Duration::from_secs(2), |_| false).await;
    let closed = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("closed"))
        .count();
    assert_eq!(closed, 1, "exactly one closed: {events:?}");
    assert!(
        !events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("error")),
        "never an error event: {events:?}"
    );
}

// ── A6.21 the reader's `ps` rows ─────────────────────────────────────

/// A6.21 — one `pty_reader` holder row per reader attach, surviving `stop`
/// (python's `_hold` task is never cancelled). Since C36 each attach is also a
/// `task` ROW on the terminal, so the count holds in the store as well as in
/// `ps` — and, unlike `ps`, the store keeps it after the holder ends.
#[tokio::test]
async fn pty_reader_rows_follow_python_holder_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "ptr", json!({"command": "/bin/sh"})).await;
    let rows = |k: &Arc<Kernel>, t: AgentId| {
        let k = Arc::clone(k);
        async move {
            let table = k
                .send(vec![Item::new(
                    AgentId::from("kernel"),
                    json!({"type": "ps"}),
                )])
                .await
                .remove(0)
                .result;
            table["tasks"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter(|r| {
                            r["agent"] == json!(t.as_str()) && r["verb"] == json!("pty_reader")
                        })
                        .count()
                })
                .unwrap_or(0)
        }
    };
    // create_agent auto-booted once already; python cancels the holder on detach
    // (2026-09-20), so nothing is live after that first `stop`.
    tokio::time::sleep(Duration::from_millis(150)).await; // the holder observes the detach on its next poll
    let base = rows(&kernel, term.clone()).await;
    assert_eq!(base, 0, "the auto-boot's holder ended with its stop");
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rows(&kernel, term.clone()).await,
        base + 1,
        "each (re)attach registers one more holder"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rows(&kernel, term.clone()).await,
        base,
        "the holder ends with the attachment"
    );
    // C36 — one `task` row per attach, and every live pid in `ps` is one of them.
    let store: Vec<_> = kernel
        .rows
        .for_agent(&term)
        .into_iter()
        .filter(|r| {
            r.get("kind") == Some(&json!("task")) && r.get("verb") == Some(&json!("pty_reader"))
        })
        .collect();
    // Two attaches so far (the auto-boot's and this one); the store keeps both.
    assert_eq!(store.len(), 2, "one row per (re)attach: {store:?}");
    let ids: Vec<i64> = store
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    let table = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    for row in table["tasks"].as_array().expect("tasks") {
        if row["agent"] == json!(term.as_str()) && row["verb"] == json!("pty_reader") {
            assert!(
                ids.contains(&row["id"].as_i64().unwrap_or(-1)),
                "a holder pid with no row: {row}"
            );
        }
    }
}

// ── A6.9 shell ───────────────────────────────────────────────────────

/// A6.9 — the done-token round trip (python `::test_shell_done_token_fast_command`).
#[tokio::test(flavor = "multi_thread")]
async fn shell_runs_a_fast_command_via_the_done_token() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "shf", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let t0 = std::time::Instant::now();
    let r = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "echo hello-world"}),
    )
    .await;
    assert_eq!(r["completed"], json!(true), "{r}");
    assert!(
        r["output"].as_str().unwrap_or("").contains("hello-world"),
        "{r}"
    );
    assert_eq!(r["cmd"], json!("echo hello-world"));
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "no 30s default wait"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.9 — a timeout answers `completed:false, error:"timeout"` and the next
/// call still works (python `::test_shell_timeout`, `::test_shell_recovers_after_timeout`).
#[tokio::test(flavor = "multi_thread")]
async fn shell_times_out_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "sht", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let ready = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "echo ready", "timeout": 10.0}),
    )
    .await;
    assert_eq!(ready["completed"], json!(true), "{ready}");
    let first = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "sleep 60", "timeout": 2.0}),
    )
    .await;
    assert_eq!(first["completed"], json!(false), "{first}");
    assert_eq!(first["error"], json!("timeout"));
    let r = call(
        &kernel,
        &term,
        json!({"type": "shell", "cmd": "echo recovered", "timeout": 10.0}),
    )
    .await;
    assert_eq!(r["completed"], json!(true), "first={first} second={r}");
    assert!(
        r["output"].as_str().unwrap_or("").contains("recovered"),
        "{r}"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.9 — python's two `shell` refusals.
#[tokio::test]
async fn shell_requires_a_cmd_and_a_booted_pty() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "shr", json!({"command": "/bin/sh"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "shell", "cmd": "echo x"})).await,
        json!({"error": "shell: not running; call boot first"})
    );
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    assert_eq!(
        call(&kernel, &term, json!({"type": "shell"})).await,
        json!({"error": "shell: cmd required"})
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.9 — `restart` kills and respawns with the same record.
#[tokio::test(flavor = "multi_thread")]
async fn restart_respawns_the_same_command() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "rst", json!({"command": "/bin/sh"})).await;
    let _ = call(&kernel, &term, json!({"type": "boot"})).await;
    let first = call(&kernel, &term, json!({"type": "reflect"})).await["pid"]
        .as_i64()
        .expect("pid");
    assert_eq!(
        call(&kernel, &term, json!({"type": "restart"})).await,
        json!({"restarted": true})
    );
    let refl = call(&kernel, &term, json!({"type": "reflect"})).await;
    assert_eq!(refl["command"], json!("/bin/sh"));
    assert_ne!(refl["pid"].as_i64().expect("pid"), first);
    assert_eq!(
        refl["scrollback_bytes"],
        json!(0),
        "a fresh PTY starts empty"
    );
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A6.9 — `write` delivers the bytes in full (python `::test_write_delivers_bytes`).
#[tokio::test(flavor = "multi_thread")]
async fn write_delivers_every_byte() {
    let tmp = TempDir::new().unwrap();
    let (kernel, term, _rx) = mk_term(&tmp, "wrl", json!({"command": "/bin/sh"})).await;
    let _ = boot(&kernel, &term).await;
    let body: String = (0..8000).map(|i| format!("L{i:05}")).collect();
    let payload = format!("echo START_{body}_END\n");
    let r = call(
        &kernel,
        &term,
        json!({"type": "write", "data": payload.clone()}),
    )
    .await;
    assert_eq!(r, json!({"written": payload.len()}), "short write");
    // (what this guards is the WRITE loop — a single `write()` on a non-blocking
    // PTY fd short-writes and silently drops the tail. What the interactive
    // shell's readline then ECHOES of a 64 KB line is its own business.)
    let out = wait_for_output(&kernel, &term, "START_L00000", Duration::from_secs(10)).await;
    assert!(out.contains("START_L00000"), "the PTY never saw the write");
    let _ = call(&kernel, &term, json!({"type": "stop"})).await;
}

/// A burst the child writes in ONE `write` arrives in ONE `output` event when
/// it fits python's read size, and the concatenation of every event is the text
/// verbatim. Rust polled per byte-arrival and split `echo wirecap` into three
/// events (`"echo wirecap\r\n"`, `"wirecap\r\n"`, `"sh-3.2$ "`) where python's
/// one `os.read(fd, 4096)` made one (r3 capture, `wire/frames.jsonl[185]`).
#[tokio::test]
async fn a_single_child_write_arrives_as_one_output_event() {
    let tmp = TempDir::new().expect("tmp");
    let (kernel, term, mut rx) = mk_term(&tmp, "burst", json!({})).await;
    boot(&kernel, &term).await;
    // Drain the shell banner + first prompt.
    let _ = collect_events(&mut rx, Duration::from_millis(900), |_| false).await;
    // ONE write by the child: printf emits all four lines in a single syscall.
    let line = "printf 'a1\\nb2\\nc3\\nd4\\n'\n";
    call(&kernel, &term, json!({"type": "write", "data": line})).await;
    // `d4\r\n` is the PRINTED line; the command echo carries a literal `d4\\n`.
    let events = collect_events(&mut rx, Duration::from_secs(6), |es| {
        concat_text(es).contains("d4\r\n")
    })
    .await;
    let text = concat_text(&events);
    assert!(text.contains("a1\r\nb2\r\nc3\r\nd4\r\n"), "text: {text:?}");
    let burst: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("data")
                .and_then(Value::as_str)
                .is_some_and(|d| d.contains("a1\r\n"))
        })
        .collect();
    assert_eq!(
        burst.len(),
        1,
        "the burst must arrive in ONE event, got {burst:?}"
    );
    let d = burst[0]["data"].as_str().expect("data");
    assert!(
        d.contains("a1\r\nb2\r\nc3\r\nd4\r\n"),
        "one event must hold the whole burst: {d:?}"
    );
}
