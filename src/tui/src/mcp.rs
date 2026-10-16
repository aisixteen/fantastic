//! `fantastic mcp` — THE MCP FACE: the kernel of this directory, driven by an
//! MCP client (a coding CLI) over stdio. It takes no arguments.
//!
//! ONE COMMAND, TWO SHAPES, decided by the directory's lock (never a fallback):
//!
//! - A LIVE KERNEL HOLDS THE LOCK (the terminal UI, the canvas, `--headless`,
//!   or another `fantastic mcp`): this process ATTACHES — it relays stdio to
//!   that kernel's MCP socket, `.fantastic/cli/mcp.sock`, and runs no kernel.
//! - NO LIVE KERNEL: this process RUNS IT — boots (a fresh directory is seeded
//!   first, like every command), serves stdio, and opens the socket so other
//!   clients can attach.
//!
//! Every process that owns a kernel serves the socket ([`listen`]); what MCP
//! means lives in the `fantastic-web-mcp` crate. Each connection, stdio or
//! socket, is served the same way ([`serve_conn`]).
//!
//! THE LEG. `mcp` is a persisted, first-class agent, created once by code on the
//! first connection to a kernel that lacks it (never by the blueprint), with
//! god-mode grants (`kern/reach`) — the human reshapes them with
//! `update_agent`; the record is the truth after that.
//!
//! STDIO HYGIENE (running the kernel): stdout is the protocol. Before anything
//! boots, the real stdout is duplicated into a private descriptor only the
//! writer uses, and fds 1 and 2 are pointed at the kernel log — so no
//! `println!` anywhere in the process can corrupt a frame. The SHAPE (what this
//! is, how to register it) goes to the ORIGINAL stderr on start, so a coding
//! CLI that runs `fantastic mcp` in a shell reads it.
//!
//! SHUTDOWN, in the kernel's order: `Kernel::shutdown` (drain, shutdown verbs,
//! cancel what remains, final ledger flush), then the lock release — on stdin
//! EOF, SIGTERM or SIGINT (Ctrl-Break or Ctrl-C on Windows), and the process
//! does not exit before both finish.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fantastic_kernel::TaskId;
use fantastic_web_mcp::{McpCore, HANDLER_MODULE, INSTRUCTIONS, LEAD_IN, SHAPE};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, OnceCell};

use crate::env::Paths;
use crate::kern::host::Host;

/// The leg's id. Its parent is the root; its handler is `web_mcp.tools`.
pub const LEG: &str = "mcp";

/// The kernel's MCP socket, RELATIVE to the directory: every process runs in
/// the directory it serves, and a relative path never meets the ~104-byte
/// limit on a unix socket's address. (On Windows the path is a private file
/// naming a loopback port — `sys`.)
pub const SOCKET: &str = ".fantastic/cli/mcp.sock";

/// Entry point, called from `main` before any runtime exists.
pub fn main(argv: &[String]) -> ! {
    if let Some(a) = argv.first() {
        eprintln!("fantastic mcp: takes no arguments (got `{a}`) — its grants live in the `{LEG}` record; change them with update_agent");
        std::process::exit(2);
    }
    let paths = match Paths::here() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fantastic mcp: no working directory: {e}");
            std::process::exit(2);
        }
    };
    // the OS lock, not the pid in the file: a stale lock.yaml naming a reused pid is not held
    match fantastic_kernel::lock::held(&paths.cwd) {
        Ok(Some(pid)) if pid != std::process::id() => attach(pid),
        Ok(_) => own(paths),
        Err(e) => {
            eprintln!("fantastic mcp: {e}");
            std::process::exit(1);
        }
    }
}

/// ATTACH: relay stdio to the running kernel's socket until it closes.
fn attach(pid: u32) -> ! {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fantastic mcp: no runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = rt.block_on(async move {
        let sock = match crate::sys::connect_local(SOCKET).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fantastic mcp: a kernel (pid {pid}) runs in this directory, but its MCP socket `{SOCKET}` does not answer: {e}");
                return 1;
            }
        };
        let _ = std::io::stderr().write_all(shape().as_bytes());
        let (mut r, mut w) = sock.into_split();
        // up: the client's requests; when stdin ends, the socket's write half
        // closes and the kernel finishes what is in flight, then closes.
        let up = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
            let _ = w.shutdown().await;
        });
        let mut out = tokio::io::stdout();
        let _ = tokio::io::copy(&mut r, &mut out).await;
        let _ = out.flush().await;
        up.abort();
        0
    });
    std::process::exit(code);
}

/// RUN THE KERNEL: boot it here, serve stdio, and open the socket.
fn own(paths: Paths) -> ! {
    // Whether this store needs INIT must be read BEFORE the log is opened: the
    // log lives inside `.fantastic`, so opening it first would make every store
    // look initialised.
    let fresh = paths.is_fresh();

    // ── stdio hygiene ────────────────────────────────────────────────────────
    let (std_out, mut err_out) = match (crate::sys::dup_stdout(), crate::sys::dup_stderr()) {
        (Ok(o), Ok(e)) => (o, e),
        _ => {
            eprintln!("fantastic mcp: could not duplicate stdout/stderr");
            std::process::exit(1);
        }
    };
    let _ = err_out.write_all(shape().as_bytes());
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
    let code = rt.block_on(serve(paths, fresh, std_out, &mut err_out));
    drop(rt);
    std::process::exit(code);
}

/// What a coding CLI that runs `fantastic mcp` in a shell reads on stderr: how
/// to register it, then what a connected client is told in `initialize`.
fn shape() -> String {
    format!("{SHAPE}{LEAD_IN}{INSTRUCTIONS}\n\n")
}

/// Say a fatal reason where the MCP client logs it (the ORIGINAL stderr) and in
/// the kernel log. Returns the exit code.
fn fatal(err_out: &mut std::fs::File, msg: &str) -> i32 {
    let _ = writeln!(err_out, "fantastic mcp: {msg}");
    tracing::error!("{msg}");
    1
}

async fn serve(paths: Paths, fresh: bool, std_out: std::fs::File, err_out: &mut std::fs::File) -> i32 {
    if fresh {
        if let Err(e) = crate::boot::seed::write_all(&paths) {
            return fatal(err_out, &format!("init failed: {e}"));
        }
    }

    // ── boot / rehydrate ─────────────────────────────────────────────────────
    let host = match Host::boot(&paths, false, None).await {
        Ok(h) => Arc::new(h),
        Err(e) => return fatal(err_out, &format!("{e}")),
    };
    let served = Served::new(Arc::clone(&host));
    // The leg now, not on the first request: a refusal is said at start.
    if let Err(e) = served.core().await {
        let code = fatal(err_out, &e);
        host.shutdown(&paths).await;
        return code;
    }
    // THE GRANT WATCH, once the boot's own grant writes are done (`kern/reach`)
    if let Err(e) = crate::kern::reach::start(&host).await {
        let code = fatal(err_out, &e);
        host.shutdown(&paths).await;
        return code;
    }
    if let Err(e) = listen(Arc::clone(&served)) {
        let code = fatal(err_out, &e);
        host.shutdown(&paths).await;
        return code;
    }

    let mut term = match crate::sys::terminate() {
        Ok(s) => s,
        Err(e) => return fatal(err_out, &format!("no SIGTERM handler: {e}")),
    };
    tokio::select! {
        _ = serve_conn(served, tokio::io::stdin(), tokio::fs::File::from_std(std_out)) => tracing::info!("stdin closed — shutting down"),
        _ = term.recv() => tracing::info!("SIGTERM — shutting down"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT — shutting down"),
    }

    // ── shutdown, in the kernel's order ──────────────────────────────────────
    host.shutdown(&paths).await;
    close_socket();
    0
}

/// What every connection to one kernel shares: the host, and the MCP core —
/// made on the first connection, which is when the leg is ensured.
pub struct Served {
    host: Arc<Host>,
    core: OnceCell<Arc<McpCore>>,
}

impl Served {
    pub fn new(host: Arc<Host>) -> Arc<Self> {
        Arc::new(Self { host, core: OnceCell::new() })
    }

    async fn core(&self) -> Result<Arc<McpCore>, String> {
        self.core
            .get_or_try_init(|| async {
                ensure_leg(&self.host).await?;
                Ok::<_, String>(Arc::new(McpCore::new(
                    Arc::clone(&self.host.kernel),
                    fantastic_kernel::AgentId::from(LEG),
                    env!("CARGO_PKG_VERSION"),
                )))
            })
            .await
            .cloned()
    }
}

/// Serve the kernel's MCP socket: every process that owns a kernel calls this
/// once it has booted. The caller holds the directory's lock, so a socket file
/// already there is stale.
pub fn listen(served: Arc<Served>) -> Result<(), String> {
    let _ = std::fs::remove_file(SOCKET);
    let l = crate::sys::bind_local(SOCKET).map_err(|e| format!("could not open the MCP socket `{SOCKET}`: {e}"))?;
    crate::sys::make_private(SOCKET)
        .map_err(|e| format!("could not make `{SOCKET}` private: {e}"))?;
    tokio::spawn(async move {
        loop {
            match l.accept().await {
                Ok((s, _)) => {
                    let (r, w) = s.into_split();
                    tokio::spawn(serve_conn(Arc::clone(&served), r, w));
                }
                Err(e) => {
                    tracing::warn!("mcp socket: {e}");
                    break;
                }
            }
        }
    });
    Ok(())
}

/// Remove the socket file, after the kernel has shut down.
pub fn close_socket() {
    let _ = std::fs::remove_file(SOCKET);
}

/// One MCP connection: JSON-RPC lines in, replies out, until the reader ends —
/// then what is in flight finishes and the writer closes.
async fn serve_conn<R, W>(served: Arc<Served>, reader: R, writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // the writer: the ONLY thing that touches this connection's output
    let (tx, mut rx) = mpsc::channel::<Value>(256);
    let out = tokio::spawn(async move {
        let mut w = writer;
        while let Some(v) = rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });

    // request id → the task running it, so a cancel from the client finds it.
    let inflight: Arc<Mutex<HashMap<String, TaskId>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut work = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("mcp: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("Parse error: {e}")}})).await;
                continue;
            }
        };
        let core = match served.core().await {
            Ok(c) => c,
            Err(e) => {
                if let Some(id) = msg.get("id") {
                    let _ = tx.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":e}})).await;
                }
                continue;
            }
        };
        // Cancellation is a notification naming the request.
        if msg.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            let rid = msg.pointer("/params/requestId").map(|v| v.to_string()).unwrap_or_default();
            let task = inflight.lock().expect("inflight poisoned").remove(&rid);
            if let Some(t) = task {
                let r = core.cancel(&t).await;
                tracing::info!("cancelled request {rid} → task {}: {r}", t.get());
            }
            continue;
        }
        let tx = tx.clone();
        let inflight = Arc::clone(&inflight);
        work.spawn(async move {
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
    while work.join_next().await.is_some() {}
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), out).await;
}

/// Bind the leg, creating it the first time; then god mode (`kern/reach`).
pub(crate) async fn ensure_leg(host: &Host) -> Result<(), String> {
    let existing = host.kernel.agents.get(&fantastic_kernel::AgentId::from(LEG)).map(|e| e.value().handler_module.clone());
    match existing {
        Some(handler) => {
            if handler.as_deref() != Some(HANDLER_MODULE) {
                return Err(format!(
                    "an agent `{LEG}` exists but runs {handler:?}, not {HANDLER_MODULE} — rename or delete it"
                ));
            }
        }
        None => {
            let created = host
                .operator(
                    "kernel",
                    json!({
                        "type": "create_agent",
                        "parent_id": host.cfg.user.as_str(),
                        "id": LEG,
                        "handler_module": HANDLER_MODULE,
                    }),
                )
                .await;
            if let Some(err) = created.result.get("error").and_then(Value::as_str) {
                return Err(format!("could not create the `{LEG}` leg: {err}"));
            }
            let _ = host.operator(LEG, json!({"type": "boot"})).await;
        }
    }
    // Both edges on every agent BEFORE the first request is served (the watch
    // for agents added later is started by the launcher, after its own grant
    // writes: `kern/reach`).
    crate::kern::reach::grant_all(host).await
}
