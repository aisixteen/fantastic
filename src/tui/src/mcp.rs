//! `fantastic mcp` — THE HEADLESS MODE: the full kernel, in this process, driven
//! by an MCP client over stdio.
//!
//! This file is only the stdio CARRIER. It reads JSON-RPC lines from stdin, hands
//! each to the protocol core in the `fantastic-web-mcp` crate, and writes the
//! replies. What MCP means lives in that crate; what the kernel is lives in the
//! kernel. The carrier owns three things and nothing else:
//!
//! 1. STDIO HYGIENE. stdout is the protocol. Before anything boots, the real
//!    stdout is duplicated into a private descriptor only the writer uses, and
//!    fds 1 and 2 are pointed at the kernel log — so no `println!` anywhere in
//!    the process can corrupt a frame.
//! 2. THE LEG. `mcp` is a persisted, first-class agent. It is created once, by
//!    code, the first time this command runs in a store (never by the
//!    blueprint), and bound by id on every run after. Its grants and bound come
//!    from flags ONLY when it is created; after that the record is the truth.
//! 3. SHUTDOWN, in the kernel's order: `Kernel::shutdown` (drain, shutdown
//!    verbs, cancel what remains, final ledger flush), then the lock release —
//!    on stdin EOF, SIGTERM or SIGINT, and the process does not exit before both
//!    finish.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::FromRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fantastic_kernel::TaskId;
use fantastic_web_mcp::{McpCore, BOUND_FIELD, HANDLER_MODULE};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::env::Paths;
use crate::kern::host::Host;

/// The leg's id. Its parent is the root; its handler is `web_mcp.tools`.
pub const LEG: &str = "mcp";

/// The flags `fantastic mcp` takes. Both apply ONLY when the leg is created.
#[derive(Debug, Default, PartialEq)]
pub struct Flags {
    pub can_call: Option<Vec<String>>,
    pub bound_ms: Option<u64>,
}

impl Flags {
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        let mut f = Flags::default();
        let mut it = argv.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--can-call" => {
                    let v = it.next().ok_or("--can-call needs a list: --can-call files,exec,user")?;
                    let ids: Vec<String> =
                        v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
                    if ids.is_empty() {
                        return Err("--can-call needs at least one agent id".into());
                    }
                    f.can_call = Some(ids);
                }
                "--settle-bound-ms" => {
                    let v = it.next().ok_or("--settle-bound-ms needs a number of milliseconds")?;
                    let n: u64 = v
                        .parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("--settle-bound-ms must be a positive integer, got `{v}`"))?;
                    f.bound_ms = Some(n);
                }
                other => {
                    return Err(format!(
                        "fantastic mcp: unknown argument `{other}` (takes --can-call a,b,c and --settle-bound-ms N)"
                    ))
                }
            }
        }
        Ok(f)
    }
}

/// Entry point, called from `main` before any runtime exists.
pub fn main(argv: &[String]) -> ! {
    let flags = match Flags::parse(argv) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let paths = match Paths::here() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fantastic mcp: no working directory: {e}");
            std::process::exit(2);
        }
    };

    // Whether this store needs INIT must be read BEFORE the log is opened: the
    // log lives inside `.fantastic`, so opening it first would make every store
    // look initialised.
    let fresh = paths.is_fresh();

    // ── stdio hygiene ────────────────────────────────────────────────────────
    // SAFETY: plain descriptor duplication; 1 and 2 are open at process start.
    let (proto_fd, err_fd) = unsafe { (libc::dup(1), libc::dup(2)) };
    if proto_fd < 0 || err_fd < 0 {
        eprintln!("fantastic mcp: could not duplicate stdout/stderr");
        std::process::exit(1);
    }
    // SAFETY: `err_fd` was just duplicated and is owned by this File from here on.
    let mut err_out = unsafe { std::fs::File::from_raw_fd(err_fd) };
    if let Err(e) = crate::tty::redirect_std_to(&paths.cli.join("kernel.log")) {
        let _ = writeln!(err_out, "fantastic mcp: {e}");
        std::process::exit(1);
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err_out, "fantastic mcp: no runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = rt.block_on(serve(paths, fresh, flags, proto_fd, &mut err_out));
    drop(rt);
    std::process::exit(code);
}

/// Say a fatal reason where the MCP client logs it (the ORIGINAL stderr) and in
/// the kernel log. Returns the exit code.
fn fatal(err_out: &mut std::fs::File, msg: &str) -> i32 {
    let _ = writeln!(err_out, "fantastic mcp: {msg}");
    tracing::error!("{msg}");
    1
}

async fn serve(paths: Paths, fresh: bool, flags: Flags, proto_fd: i32, err_out: &mut std::fs::File) -> i32 {
    // ── init: the second of the two explicit triggers ────────────────────────
    if fresh {
        if let Err(e) = crate::boot::seed::write_all(&paths) {
            return fatal(err_out, &format!("init failed: {e}"));
        }
    }

    // ── boot / rehydrate ─────────────────────────────────────────────────────
    let host = match Host::boot(&paths, false).await {
        Ok(h) => h,
        // ONE INSTANCE PER DIRECTORY: the kernel's lock error names the owning
        // pid and the remedy. Attaching to a running kernel is future work.
        Err(e) => return fatal(err_out, &format!("{e}")),
    };

    // ── the leg ──────────────────────────────────────────────────────────────
    let bound = match ensure_leg(&host, &flags).await {
        Ok(b) => b,
        Err(e) => {
            let code = fatal(err_out, &e);
            host.shutdown(&paths).await;
            return code;
        }
    };
    tracing::info!("mcp leg bound: {LEG}, settle_bound_ms={}", bound.as_millis());

    let core = Arc::new(McpCore::new(
        Arc::clone(&host.kernel),
        fantastic_kernel::AgentId::from(LEG),
        bound,
        env!("CARGO_PKG_VERSION"),
    ));

    // ── the writer: the ONLY thing that touches the protocol descriptor ─────
    let (tx, mut rx) = mpsc::channel::<Value>(256);
    // SAFETY: `proto_fd` is the duplicated original stdout, owned from here on.
    let std_out = unsafe { std::fs::File::from_raw_fd(proto_fd) };
    let writer = tokio::spawn(async move {
        let mut out = tokio::fs::File::from_std(std_out);
        while let Some(v) = rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            if out.write_all(line.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });

    // request id → the task running it, so a cancel from the client finds it.
    let inflight: Arc<Mutex<HashMap<String, TaskId>>> = Arc::new(Mutex::new(HashMap::new()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => return fatal(err_out, &format!("no SIGTERM handler: {e}")),
    };

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => { tracing::info!("stdin closed — shutting down"); break; }
                    Err(e) => { tracing::warn!("stdin: {e} — shutting down"); break; }
                };
                if line.trim().is_empty() { continue; }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("Parse error: {e}")}})).await;
                        continue;
                    }
                };
                // On stdio, cancellation is a notification naming the request.
                if msg.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
                    let rid = msg.pointer("/params/requestId").map(|v| v.to_string()).unwrap_or_default();
                    let task = inflight.lock().expect("inflight poisoned").remove(&rid);
                    if let Some(t) = task {
                        let r = core.cancel(&t).await;
                        tracing::info!("cancelled request {rid} → task {}: {r}", t.get());
                    }
                    continue;
                }
                let core = Arc::clone(&core);
                let tx = tx.clone();
                let inflight = Arc::clone(&inflight);
                tokio::spawn(async move {
                    let rid = msg.get("id").map(|v| v.to_string());
                    let map = Arc::clone(&inflight);
                    let key = rid.clone();
                    let reply = core
                        .handle(&msg, move |t| {
                            if let Some(k) = key {
                                map.lock().expect("inflight poisoned").insert(k, t);
                            }
                        })
                        .await;
                    if let Some(k) = rid {
                        inflight.lock().expect("inflight poisoned").remove(&k);
                    }
                    if let Some(r) = reply {
                        let _ = tx.send(r).await;
                    }
                });
            }
            _ = term.recv() => { tracing::info!("SIGTERM — shutting down"); break; }
            _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT — shutting down"); break; }
        }
    }

    // ── shutdown, in the kernel's order ──────────────────────────────────────
    host.shutdown(&paths).await;
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    0
}

/// Bind the leg, creating it on the first run in this store. Returns its bound.
async fn ensure_leg(host: &Host, flags: &Flags) -> Result<Duration, String> {
    let existing = host.kernel.agents.get(&fantastic_kernel::AgentId::from(LEG)).map(|e| {
        let a = e.value();
        (a.handler_module.clone(), a.record().meta.clone())
    });

    if let Some((handler, meta)) = existing {
        if handler.as_deref() != Some(HANDLER_MODULE) {
            return Err(format!(
                "an agent `{LEG}` exists but runs {handler:?}, not {HANDLER_MODULE} — rename or delete it"
            ));
        }
        // The record is the truth now. A flag that would change it is refused
        // rather than silently ignored.
        if flags.can_call.is_some() || flags.bound_ms.is_some() {
            return Err(format!(
                "the `{LEG}` leg already exists; its grants and bound live in its record — change them with update_agent, not flags"
            ));
        }
        let ms = meta.get(BOUND_FIELD).and_then(Value::as_u64).ok_or_else(|| {
            format!("the `{LEG}` record has no `{BOUND_FIELD}` — set it with update_agent")
        })?;
        return Ok(Duration::from_millis(ms));
    }

    // First run in this store: create it. No defaults — both flags are required.
    let can_call = flags
        .can_call
        .clone()
        .ok_or("the mcp leg does not exist yet — create it with --can-call a,b,c (the agents it may call)")?;
    let ms = flags
        .bound_ms
        .ok_or("the mcp leg does not exist yet — create it with --settle-bound-ms N (how long a call waits before it answers with its task id)")?;

    // Every target must exist, and must be told to accept the leg. Read first,
    // so a typo refuses before anything is written.
    let mut merged: Vec<(String, Map<String, Value>)> = Vec::new();
    for t in &can_call {
        let agent = host
            .kernel
            .agents
            .get(&fantastic_kernel::AgentId::from(t.as_str()))
            .ok_or_else(|| format!("--can-call names `{t}`, which is not an agent in this kernel"))?;
        let mut cb = agent
            .value()
            .record()
            .meta
            .get("callable_by")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // MERGE, never replace: update_agent swaps a top-level field whole, so
        // sending only {mcp} would wipe the target's existing grants.
        cb.insert(LEG.into(), json!("*"));
        merged.push((t.clone(), cb));
    }

    let grants: Map<String, Value> = can_call.iter().map(|t| (t.clone(), json!("*"))).collect();
    let created = host
        .operator(
            "kernel",
            json!({
                "type": "create_agent",
                "parent_id": host.root,
                "id": LEG,
                "handler_module": HANDLER_MODULE,
                "can_call": grants,
                BOUND_FIELD: ms,
            }),
        )
        .await;
    if let Some(err) = created.result.get("error").and_then(Value::as_str) {
        return Err(format!("could not create the `{LEG}` leg: {err}"));
    }

    for (t, cb) in merged {
        let out = host
            .operator("kernel", json!({"type": "update_agent", "id": t, "callable_by": cb}))
            .await;
        if let Some(err) = out.result.get("error").and_then(Value::as_str) {
            return Err(format!("could not grant `{LEG}` on `{t}`: {err}"));
        }
    }
    let _ = host.operator(LEG, json!({"type": "boot"})).await;
    // The leg and the grants it needs are durable BEFORE the first request: the
    // record writer is debounced, and a client that connects and leaves inside
    // that window must still leave the leg behind.
    Ok(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn no_flags_is_a_valid_bind_to_an_existing_leg() {
        assert_eq!(Flags::parse(&[]).unwrap(), Flags::default());
    }

    #[test]
    fn both_flags_parse() {
        let f = Flags::parse(&argv(&["--can-call", "files, exec ,user", "--settle-bound-ms", "20000"])).unwrap();
        assert_eq!(f.can_call.unwrap(), vec!["files", "exec", "user"]);
        assert_eq!(f.bound_ms, Some(20000));
    }

    #[test]
    fn a_zero_bound_and_an_unknown_flag_are_refused() {
        assert!(Flags::parse(&argv(&["--settle-bound-ms", "0"])).is_err());
        assert!(Flags::parse(&argv(&["--port", "9"])).is_err());
        assert!(Flags::parse(&argv(&["--can-call"])).is_err());
    }
}
