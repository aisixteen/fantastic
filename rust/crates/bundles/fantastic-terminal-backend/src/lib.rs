//! PTY shell session as an agent — VSCode-class terminal robustness.
//!
//! Mirrors `python/bundled_agents/terminal/terminal_backend/src/terminal_backend/tools.py`
//! verb-for-verb. One PTY per agent. Process-memory state only; no
//! `.fantastic/` sidecars (paste blobs go to the OS tempdir).
//!
//! ## Verbs
//!
//! - `reflect`     → identity + live PTY state
//! - `boot`        → if `record.auto_start` (default true), spawn PTY. Idempotent.
//! - `spawn`       → start PTY child (`cmd`, `cwd?`, `env?`, `cols?`, `rows?`)
//! - `write`       → `{data:str}` write UTF-8 bytes to PTY master (serialized per-agent)
//! - `ack`         → `{count:int}` decrement unacked-byte counter; resumes reader past 100K
//! - `resize`      → `{cols, rows}` SIGWINCH
//! - `paste_image` → `{data, mime?}` save image to OS tempdir; cap 5 MB; type path
//! - `interrupt`   → SIGINT to child
//! - `signal`      → `{signal}` named or numeric signal
//! - `stop`        → SIGKILL; close master; remove from TERMINALS map
//!
//! ## Flow control
//!
//! The reader buffers; if `unacked_bytes` exceeds [`FLOW_PAUSE_THRESHOLD`]
//! (100 K), it sets `paused=true` and awaits a `Notify`. `ack` decrements
//! the counter; once below the threshold, it flips `paused=false` and
//! calls `Notify::notify_one` to resume the reader. Per-agent
//! `Arc<Mutex<FlowControl>>` is the single source of truth.
//!
//! ## Image paste
//!
//! `paste_image` accepts EITHER raw bytes via [`Bundle::handle_binary`]
//! (zero-copy fast path used by the WS binary-frame channel) OR a
//! base64-encoded string in `payload["data"]` (fallback for the
//! text-only verb path). Both routes converge on `paste_image_impl`.

#![deny(missing_docs)]

use async_trait::async_trait;
use base64::Engine as _;
use encoding_rs::UTF_8;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "terminal_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Maximum bytes accepted by `paste_image`. Matches Python's
/// `MAX_PASTE_IMAGE = 5 * 1024 * 1024`.
pub const MAX_PASTE_IMAGE: usize = 5 * 1024 * 1024;

/// Threshold at which the PTY reader pauses. Matches Python's
/// `HIGH_WATERMARK`. The reader resumes once `unacked_bytes` drops
/// below this value (we use one watermark, not two — Python's
/// `LOW_WATERMARK = 5_000` is a cheap optimization not worth porting
/// here; the cost of a spurious resume is one extra `Notify::notify_one`).
pub const FLOW_PAUSE_THRESHOLD: usize = 100_000;

/// Default PTY column count (matches Python's `DEFAULT_COLS`).
pub const DEFAULT_COLS: u16 = 200;

/// Default PTY row count (matches Python's `DEFAULT_ROWS`).
pub const DEFAULT_ROWS: u16 = 50;

/// Default reader chunk size — matches the Python implementation's
/// `os.read(fd, 4096)` ceiling on the upper bound; we go one step
/// larger because the reader is on a tokio blocking thread and the
/// extra 4 K saves a syscall per chunk on bursty output.
pub const READ_CHUNK_BYTES: usize = 8192;

// ── per-agent state map ──────────────────────────────────────────────

/// Per-agent live state (process-only, never persisted).
struct PtyState {
    /// The master half of the PTY. Wrapped in a tokio Mutex so
    /// `resize` (sync verb) can lock briefly.
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// The child process handle. Locked when calling `kill`/`wait`.
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Cloneable killer — survives across the child mutex so
    /// `interrupt`/`signal`/`stop` don't deadlock if `wait` is in flight.
    killer: StdMutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    /// Process id (for `os.kill`-style signal delivery on Unix).
    pid: Option<u32>,
    /// Serializes writes so concurrent `write`/`paste_image` can't
    /// interleave bytes mid-bracketed-paste.
    write_lock: Mutex<()>,
    /// Flow control + pause notifier.
    flow: Arc<StdMutex<FlowControl>>,
    /// Notifier the reader awaits while paused. `ack` calls
    /// `notify_one` once the backlog drains.
    resume_notify: Arc<Notify>,
    /// Per-session incremental UTF-8 decoder. Buffers a partial
    /// multi-byte char across read boundaries.
    decoder: Mutex<encoding_rs::Decoder>,
    /// Lazily-created scratch dir for pasted images (one per agent).
    paste_dir: Mutex<Option<PathBuf>>,
    /// Current column count (atomically updated by `resize`).
    cols: AtomicU16,
    /// Current row count.
    rows: AtomicU16,
    /// Cumulative bytes still un-acked by a streaming consumer.
    /// Mirrors `FlowControl::unacked_bytes` but exposed as an atomic
    /// for the cheap `reflect` read path.
    unacked_atomic: AtomicUsize,
    /// Command vector (used by reflect; never re-spawned from this).
    cmd: Vec<String>,
    /// Working directory the child was spawned in.
    cwd: Option<String>,
    /// Environment overrides applied at spawn time.
    env: Vec<(String, String)>,
    /// Handle to the spawned reader task. Aborted on `stop`/`on_delete`.
    reader_task: Mutex<Option<JoinHandle<()>>>,
    /// PTY writer (taken once from the master at spawn time; reused for
    /// every `write` / `paste_image` since `take_writer` errors on
    /// second call).
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    /// Scrollback ring — a client calls `output` on connect to
    /// fetch what was emitted before it attached. Ring-trimmed to
    /// `MAX_SCROLLBACK_BYTES` to bound memory.
    scrollback: StdMutex<VecDeque<String>>,
    /// Sum of UTF-8-encoded byte counts of all chunks in `scrollback`.
    /// Tracked separately so the ring-trim doesn't recompute every push.
    scrollback_bytes: AtomicUsize,
    /// Set true by `cleanup_on_exit` after the child has died. The
    /// state stays in `TERMINALS` so `output` can still return the
    /// scrollback; `reflect` reports `running:false`. Cleared only by
    /// a fresh `spawn` (which replaces the entry) or by agent delete.
    exited: std::sync::atomic::AtomicBool,
}

/// Scrollback ring size cap (matches Python's MAX_SCROLLBACK).
pub const MAX_SCROLLBACK_BYTES: usize = 256 * 1024;

/// Flow-control accounting. Mutated under a sync `Mutex` because the
/// critical section is microseconds and the reader/ack paths must not
/// `.await` while holding it.
struct FlowControl {
    /// Total bytes emitted but not yet ack'd by a consumer.
    unacked_bytes: usize,
    /// Reader is currently parked on `resume_notify` because
    /// `unacked_bytes` crossed [`FLOW_PAUSE_THRESHOLD`].
    paused: bool,
}

/// Per-agent state map. The wrapper lets us declare a `const fn new`
/// `static` while still using `Mutex<HashMap<...>>` internally.
struct TerminalsMap(OnceLock<StdMutex<HashMap<AgentId, Arc<PtyState>>>>);

impl TerminalsMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Arc<PtyState>>> {
        self.0
            .get_or_init(|| StdMutex::new(HashMap::new()))
            .lock()
            .expect("TERMINALS poisoned")
    }
}

/// Live PTY state keyed by agent id. Populated by `spawn`/`boot`;
/// drained by `stop`/`on_delete`/reader-EOF.
static TERMINALS: TerminalsMap = TerminalsMap::new();

// ── bundle impl ──────────────────────────────────────────────────────

/// The terminal_backend bundle.
pub struct TerminalBackendBundle;

#[async_trait]
impl Bundle for TerminalBackendBundle {
    fn name(&self) -> &str {
        "terminal_backend"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" => boot_reply(agent_id, kernel).await,
            "spawn" => spawn_reply(agent_id, payload, kernel).await,
            "write" => write_reply(agent_id, payload).await,
            "ack" => ack_reply(agent_id, payload),
            "resize" => resize_reply(agent_id, payload).await,
            "paste_image" => paste_image_text_reply(agent_id, payload).await,
            "interrupt" => signal_reply(agent_id, libc::SIGINT),
            "signal" => signal_verb_reply(agent_id, payload),
            "stop" => stop_reply(agent_id).await,
            "output" => output_reply(agent_id, payload),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "paste_image" {
            let mime = header
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            return Ok(Some(paste_image_impl(agent_id, blob, mime).await));
        }
        // Anything else: fall back to base64+handle path (default).
        let mut payload = header;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&blob);
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("data".to_string(), Value::String(encoded));
        } else {
            let mut map = Map::new();
            map.insert("data".to_string(), Value::String(encoded));
            payload = Value::Object(map);
        }
        self.handle(agent_id, &payload, kernel).await
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let _ = stop_reply(agent_id).await;
        Ok(())
    }
}

// ── verb implementations ────────────────────────────────────────────

/// `true` iff a live (non-exited) PtyState exists for this agent.
/// Used by `reflect`, `boot`, and `spawn` to decide whether to start a
/// fresh PTY or refuse the duplicate.
fn is_running(agent_id: &AgentId) -> bool {
    TERMINALS
        .lock()
        .get(agent_id)
        .map(|s| !s.exited.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let state = TERMINALS.lock().get(agent_id).cloned();
    let rec_cmd = meta_string_array(agent_id, kernel, "cmd");
    let rec_cwd = meta_string(agent_id, kernel, "cwd");
    let rec_cols = meta_u64(agent_id, kernel, "cols").unwrap_or(DEFAULT_COLS as u64) as u16;
    let rec_rows = meta_u64(agent_id, kernel, "rows").unwrap_or(DEFAULT_ROWS as u64) as u16;
    let running = state
        .as_ref()
        .map(|s| !s.exited.load(Ordering::Relaxed))
        .unwrap_or(false);
    let (cmd, cwd, env, cols, rows, unacked, paste_dir) = if let Some(st) = state.as_ref() {
        let paste = st
            .paste_dir
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|p| p.to_string_lossy().to_string()));
        (
            st.cmd.clone(),
            st.cwd.clone(),
            st.env.clone(),
            st.cols.load(Ordering::Relaxed),
            st.rows.load(Ordering::Relaxed),
            st.unacked_atomic.load(Ordering::Relaxed),
            paste,
        )
    } else {
        (
            rec_cmd.unwrap_or_else(default_cmd),
            rec_cwd,
            Vec::<(String, String)>::new(),
            rec_cols,
            rec_rows,
            0usize,
            None,
        )
    };
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(agent_id.as_str()));
    obj.insert("sentence".to_string(), json!("PTY shell session."));
    obj.insert("cmd".to_string(), json!(cmd));
    obj.insert(
        "cwd".to_string(),
        cwd.map(Value::String).unwrap_or(Value::Null),
    );
    obj.insert(
        "env".to_string(),
        Value::Array(
            env.iter()
                .map(|(k, v)| json!([k, v]))
                .collect::<Vec<Value>>(),
        ),
    );
    obj.insert("cols".to_string(), json!(cols));
    obj.insert("rows".to_string(), json!(rows));
    obj.insert("running".to_string(), json!(running));
    obj.insert("in_flight_bytes".to_string(), json!(unacked));
    obj.insert("unacked".to_string(), json!(unacked));
    let scrollback_bytes = state
        .as_ref()
        .map(|s| s.scrollback_bytes.load(Ordering::Relaxed))
        .unwrap_or(0);
    obj.insert("scrollback_bytes".to_string(), json!(scrollback_bytes));
    if let Some(s) = state.as_ref() {
        if let Some(p) = s.pid {
            obj.insert("pid".to_string(), json!(p));
        }
    }
    if let Some(p) = paste_dir {
        obj.insert("paste_dir".to_string(), json!(p));
    }
    obj.insert("verbs".to_string(), json!({
        "reflect": "Identity + live PTY state. No args.",
        "boot": "Idempotent. Spawn the PTY via record.cmd if record.auto_start (default true).",
        "spawn": "args: cmd:[str], cwd?, env?, cols?, rows?. Start PTY child; override record meta if payload args present.",
        "write": "args: data:str. Write UTF-8 bytes to PTY master. Serialized per-agent.",
        "ack": "args: count:int. Decrement unacked-byte counter; resume reader if it drops below 100K.",
        "resize": "args: cols:int, rows:int. SIGWINCH so TUI apps redraw.",
        "paste_image": "args: data (bytes or base64-string), mime?. Save image to OS tempdir; cap 5 MB; type path + space into PTY.",
        "interrupt": "SIGINT to PTY child.",
        "signal": "args: signal:str|int. Send named or numeric signal to child.",
        "stop": "SIGKILL the child; close master; remove from TERMINALS map.",
    }));
    obj.insert("emits".to_string(), json!({
        "output": "{type:'output', data:str} — every decoded read chunk emitted to this agent's OWN inbox; a client watches it",
        "closed": "{type:'closed'} — child process exited (EOF on PTY)",
        "exited": "{type:'exited', exit_code:i32} — child died (EOF on the PTY master)",
        "error": "{type:'error', error:str} — read or write failure",
    }));
    Value::Object(obj)
}

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let auto_start = meta_bool(agent_id, kernel, "auto_start").unwrap_or(true);
    if !auto_start {
        return json!({"running": false, "auto_start": false});
    }
    if is_running(agent_id) {
        return json!({"running": true, "already_booted": true});
    }
    // Existing-but-dead state? Drop it before spawn replaces.
    if TERMINALS.lock().contains_key(agent_id) {
        TERMINALS.lock().remove(agent_id);
    }
    let cmd = meta_string_array(agent_id, kernel, "cmd").unwrap_or_else(default_cmd);
    let cwd = meta_string(agent_id, kernel, "cwd");
    let env = meta_env(agent_id, kernel);
    let cols = meta_u64(agent_id, kernel, "cols").unwrap_or(DEFAULT_COLS as u64) as u16;
    let rows = meta_u64(agent_id, kernel, "rows").unwrap_or(DEFAULT_ROWS as u64) as u16;
    match spawn_pty(agent_id, kernel, cmd, cwd, env, cols, rows).await {
        Ok(_) => json!({"running": true}),
        Err(e) => json!({"error": format!("boot: {e}")}),
    }
}

async fn spawn_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if is_running(agent_id) {
        return json!({"error": "already running; call stop first"});
    }
    // Existing-but-dead state (scrollback retention). Drop it so spawn
    // can install a fresh PtyState.
    if TERMINALS.lock().contains_key(agent_id) {
        TERMINALS.lock().remove(agent_id);
    }
    let cmd = payload
        .get("cmd")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<String>>()
        })
        .filter(|v| !v.is_empty())
        .or_else(|| meta_string_array(agent_id, kernel, "cmd"))
        .unwrap_or_else(default_cmd);
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| meta_string(agent_id, kernel, "cwd"));
    let env = payload
        .get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<Vec<(String, String)>>()
        })
        .unwrap_or_else(|| meta_env(agent_id, kernel));
    let cols = payload
        .get("cols")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_COLS as u64) as u16;
    let rows = payload
        .get("rows")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_ROWS as u64) as u16;
    match spawn_pty(agent_id, kernel, cmd.clone(), cwd, env, cols, rows).await {
        Ok(pid) => json!({"spawned": true, "pid": pid, "cmd": cmd, "cols": cols, "rows": rows}),
        Err(e) => json!({"error": format!("spawn: {e}")}),
    }
}

async fn write_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let Some(state) = TERMINALS.lock().get(agent_id).cloned() else {
        return json!({"error": "not running"});
    };
    let data = payload.get("data").and_then(Value::as_str).unwrap_or("");
    let bytes = data.as_bytes().to_vec();
    let n = bytes.len();
    // Serialize concurrent writes per-agent so a bracketed-paste
    // sequence can't be interleaved with another write.
    let _w = state.write_lock.lock().await;
    let mut writer_guard = state.writer.lock().await;
    // write_all blocks while the PTY pipe drains. The writer is owned
    // by the state's Mutex; we can't move it across spawn_blocking
    // without giving up the lock. Inline blocking write is OK here —
    // PTY master buffers are kilobyte-sized, so writes don't sit long.
    let res = writer_guard
        .write_all(&bytes)
        .and_then(|_| writer_guard.flush());
    match res {
        Ok(()) => json!({"written": n}),
        Err(e) => json!({"error": format!("write: {e}")}),
    }
}

fn ack_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let Some(state) = TERMINALS.lock().get(agent_id).cloned() else {
        return json!({"error": "not running"});
    };
    // Python's wire uses `chars`; older callers may pass `count`. Accept either.
    let count = payload
        .get("chars")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("count").and_then(Value::as_u64))
        .unwrap_or(0) as usize;
    let (unacked, paused) = {
        let mut flow = state.flow.lock().expect("flow poisoned");
        flow.unacked_bytes = flow.unacked_bytes.saturating_sub(count);
        if flow.paused && flow.unacked_bytes < FLOW_PAUSE_THRESHOLD {
            flow.paused = false;
            state.resume_notify.notify_one();
        }
        (flow.unacked_bytes, flow.paused)
    };
    state.unacked_atomic.store(unacked, Ordering::Relaxed);
    json!({"unacked": unacked, "paused": paused})
}

async fn resize_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let Some(state) = TERMINALS.lock().get(agent_id).cloned() else {
        return json!({"error": "not running"});
    };
    let cols = payload
        .get("cols")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_COLS as u64) as u16;
    let rows = payload
        .get("rows")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_ROWS as u64) as u16;
    let master = state.master.lock().await;
    let res = master.resize(PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    });
    drop(master);
    match res {
        Ok(()) => {
            state.cols.store(cols, Ordering::Relaxed);
            state.rows.store(rows, Ordering::Relaxed);
            json!({"resized": true, "cols": cols, "rows": rows})
        }
        Err(e) => json!({"error": format!("resize: {e}")}),
    }
}

/// Text-path `paste_image`: accepts `payload["data"]` as a base64
/// string. The fast path is `handle_binary`, but text-only callers
/// (in-process tests, REST diagnostics, the CLI) take this route.
async fn paste_image_text_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let data_str = match payload.get("data").and_then(Value::as_str) {
        Some(s) => s,
        None => return json!({"error": "paste_image: data (base64 string) required"}),
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data_str) {
        Ok(b) => b,
        Err(e) => return json!({"error": format!("paste_image: invalid base64: {e}")}),
    };
    let mime = payload
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_string();
    paste_image_impl(agent_id, bytes, mime).await
}

/// Shared paste-image body — used by both the text-path
/// (base64 decode) and binary-path (raw bytes) entry points.
async fn paste_image_impl(agent_id: &AgentId, bytes: Vec<u8>, mime: String) -> Value {
    let Some(state) = TERMINALS.lock().get(agent_id).cloned() else {
        return json!({"error": "not running"});
    };
    if bytes.len() > MAX_PASTE_IMAGE {
        return json!({"error": format!(
            "paste_image: {} bytes exceeds the 5 MB cap", bytes.len()
        )});
    }
    let ext = match mime.to_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return json!({"error": format!("paste_image: unsupported image type {other:?}")}),
    };
    // Lazily mint a per-agent scratch dir in the OS tempdir; cached
    // on the state so a second paste reuses it.
    let dir = {
        let mut guard = state.paste_dir.lock().await;
        if guard.is_none() {
            let candidate = std::env::temp_dir().join(format!(
                "fantastic_paste_{}_{:x}",
                agent_id.as_str(),
                rand_hex()
            ));
            if let Err(e) = std::fs::create_dir_all(&candidate) {
                return json!({"error": format!("paste_image: mkdir: {e}")});
            }
            *guard = Some(candidate);
        }
        guard.as_ref().expect("paste_dir set").clone()
    };
    let path = dir.join(format!("paste_{:x}.{ext}", rand_hex()));
    if let Err(e) = std::fs::write(&path, &bytes) {
        return json!({"error": format!("paste_image: write: {e}")});
    }
    let path_str = path.to_string_lossy().to_string();
    // Type the absolute path + a single trailing space (no newline —
    // mirrors a drag-drop, not a submit).
    let inject = format!("{} ", path_str);
    let inject_bytes = inject.into_bytes();
    let _w = state.write_lock.lock().await;
    let mut writer_guard = state.writer.lock().await;
    if let Err(e) = writer_guard
        .write_all(&inject_bytes)
        .and_then(|_| writer_guard.flush())
    {
        return json!({"error": format!("paste_image: write: {e}")});
    }
    json!({"path": path_str, "bytes": bytes.len()})
}

fn signal_verb_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let sig = match payload.get("signal") {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => i as i32,
            None => return json!({"error": "signal: invalid number"}),
        },
        Some(Value::String(s)) => match parse_signal(s) {
            Some(n) => n,
            None => return json!({"error": format!("signal: unknown name {s:?}")}),
        },
        _ => libc::SIGINT,
    };
    signal_reply(agent_id, sig)
}

fn signal_reply(agent_id: &AgentId, sig: i32) -> Value {
    let Some(state) = TERMINALS.lock().get(agent_id).cloned() else {
        return json!({"error": "not running"});
    };
    let Some(pid) = state.pid else {
        return json!({"error": "no pid"});
    };
    #[cfg(unix)]
    {
        // Signal the process group so SIGINT reaches a foreground job
        // inside the shell (matches what Ctrl-C in a real terminal does).
        // -pid in libc::kill addresses the process group with pgid==pid.
        let pgid = -(pid as i32);
        let ret = unsafe { libc::kill(pgid, sig) };
        if ret != 0 {
            // Fall back to killing the direct child if the group send
            // failed (process group may not be set on some shells).
            let ret2 = unsafe { libc::kill(pid as i32, sig) };
            if ret2 != 0 {
                let err = std::io::Error::last_os_error();
                return json!({"error": format!("signal: {err}")});
            }
        }
        json!({"signal": sig, "pid": pid})
    }
    #[cfg(not(unix))]
    {
        let _ = sig;
        let _ = pid;
        json!({"error": "signal: unsupported on this platform"})
    }
}

/// `output` verb — returns the rolling scrollback buffer's tail as a
/// single string. A client calls this on connect so a late client
/// sees what was written before it attached. Optional `max_bytes`
/// argument trims the returned tail (default = full ring).
fn output_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let max_bytes = payload
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(MAX_SCROLLBACK_BYTES);
    let Some(state) = TERMINALS.lock().get(agent_id).cloned() else {
        return json!({"output": ""});
    };
    let sb = state.scrollback.lock().expect("scrollback poisoned");
    let full: String = sb.iter().cloned().collect();
    drop(sb);
    let trimmed = if full.len() > max_bytes {
        // Trim from the START (keep the tail) — match Python's `text[-max_bytes:]`.
        // UTF-8-safe: walk back to a char boundary.
        let start = full.len() - max_bytes;
        let start = (start..full.len())
            .find(|&i| full.is_char_boundary(i))
            .unwrap_or(full.len());
        full[start..].to_string()
    } else {
        full
    };
    // Reset flow-control state. A client calls `output` on connect to
    // populate its buffer from scrollback — the act of fetching
    // it IS an implicit ack of every prior byte. Without this reset,
    // a dropped client leaves the backend's `unacked` counter near
    // the 100K cap, the reader stays paused, and the next attached
    // client can't get echoes (terminal appears hung). Discovered
    // empirically: type 3 chars, term hangs; manual ack drains and
    // unsticks. This makes the unstick automatic.
    {
        let mut flow = state.flow.lock().expect("flow poisoned");
        if flow.unacked_bytes > 0 {
            flow.unacked_bytes = 0;
            if flow.paused {
                flow.paused = false;
                state.resume_notify.notify_one();
            }
        }
    }
    state.unacked_atomic.store(0, Ordering::Relaxed);
    json!({"output": trimmed})
}

async fn stop_reply(agent_id: &AgentId) -> Value {
    let removed = TERMINALS.lock().remove(agent_id);
    let Some(state) = removed else {
        return json!({"stopped": false, "reason": "not running"});
    };
    // SIGKILL via the cloneable killer (independent of any in-flight
    // child mutex hold).
    {
        let mut killer = state.killer.lock().expect("killer poisoned");
        let _ = killer.kill();
    }
    // Wake any paused reader so it can observe EOF and exit cleanly.
    state.resume_notify.notify_waiters();
    // Abort the reader task so it doesn't outlive the agent's record.
    if let Some(task) = state.reader_task.lock().await.take() {
        task.abort();
    }
    // Clean up the paste dir.
    let paste_dir = state.paste_dir.lock().await.take();
    if let Some(dir) = paste_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }
    json!({"stopped": true})
}

// ── spawn + reader ──────────────────────────────────────────────────

async fn spawn_pty(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    cmd: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
) -> Result<Option<u32>, String> {
    if cmd.is_empty() {
        return Err("empty cmd".to_string());
    }
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;
    let mut builder = CommandBuilder::new(&cmd[0]);
    for arg in cmd.iter().skip(1) {
        builder.arg(arg);
    }
    // PTY cwd: record-level override wins; otherwise default to the
    // daemon's current working directory. Python uses
    // `rec.get("cwd") or os.getcwd()`. Without this fallback,
    // portable-pty defaults to HOME and the user sees `~/` when they
    // `pwd` in the terminal — surprising when the daemon was started
    // from a project dir.
    match cwd.as_ref() {
        Some(cwd_str) => builder.cwd(cwd_str),
        None => {
            if let Ok(d) = std::env::current_dir() {
                builder.cwd(d);
            }
        }
    }
    // Inherit the parent process's env so the shell sees HOME, PATH,
    // USER, SHELL, etc. and reads the user's rcfile (~/.zshrc /
    // ~/.bashrc). Python's `os.environ.copy()` does the same. Without
    // this, portable-pty starts the shell in a near-empty env and bash
    // falls back to defaults (no PATH customizations, the
    // "default shell now zsh" macOS nag fires because $SHELL is unset).
    for (k, v) in std::env::vars() {
        builder.env(k, v);
    }
    builder.env("TERM", "xterm-256color");
    // Record-level env overrides win over inherited.
    for (k, v) in env.iter() {
        builder.env(k, v);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| format!("spawn: {e}"))?;
    // Drop slave so the child holds the only slave fd and the master
    // sees EOF when the child exits.
    drop(pair.slave);
    let pid = child.process_id();
    let killer = child.clone_killer();
    let master = pair.master;
    // Take the writer ONCE here — portable-pty's `take_writer` errors
    // on second call. Stash in PtyState so every `write` / `paste_image`
    // can reuse it without re-taking.
    let writer = master
        .take_writer()
        .map_err(|e| format!("take_writer: {e}"))?;
    let decoder = UTF_8.new_decoder();
    let flow = Arc::new(StdMutex::new(FlowControl {
        unacked_bytes: 0,
        paused: false,
    }));
    let resume_notify = Arc::new(Notify::new());
    let state = Arc::new(PtyState {
        master: Mutex::new(master),
        child: Mutex::new(child),
        killer: StdMutex::new(killer),
        pid,
        write_lock: Mutex::new(()),
        flow: Arc::clone(&flow),
        resume_notify: Arc::clone(&resume_notify),
        decoder: Mutex::new(decoder),
        paste_dir: Mutex::new(None),
        cols: AtomicU16::new(cols),
        rows: AtomicU16::new(rows),
        unacked_atomic: AtomicUsize::new(0),
        cmd: cmd.clone(),
        cwd: cwd.clone(),
        env: env.clone(),
        reader_task: Mutex::new(None),
        writer: Mutex::new(writer),
        scrollback: StdMutex::new(VecDeque::new()),
        scrollback_bytes: AtomicUsize::new(0),
        exited: std::sync::atomic::AtomicBool::new(false),
    });
    TERMINALS
        .lock()
        .insert(agent_id.clone(), Arc::clone(&state));
    // Clone a reader off the master before spawning the reader task —
    // `try_clone_reader` needs the master alive, and we want to hold
    // the read off the master mutex (it never blocks anyone else).
    let reader: Box<dyn std::io::Read + Send> = {
        let master = state.master.lock().await;
        master
            .try_clone_reader()
            .map_err(|e| format!("clone_reader: {e}"))?
    };
    let task = tokio::spawn(reader_loop(
        agent_id.clone(),
        Arc::clone(kernel),
        Arc::clone(&state),
        reader,
    ));
    *state.reader_task.lock().await = Some(task);
    Ok(pid)
}

/// PTY reader loop. Runs on a tokio task; the blocking `read` is
/// shoved onto a blocking thread per chunk. Pushes through an
/// incremental UTF-8 decoder, emits `{type:"data", text}` to the
/// parent's inbox, and parks on `resume_notify` while flow-paused.
async fn reader_loop(
    agent_id: AgentId,
    kernel: Arc<Kernel>,
    state: Arc<PtyState>,
    mut reader: Box<dyn std::io::Read + Send>,
) {
    // Events emit to SELF (the terminal_backend's own inbox). A
    // client watches this agent's id, so watchers see the events.
    // Mirrors Python's design.
    let emit_target = agent_id.clone();
    loop {
        // Flow control: if we're past the threshold, wait for a resume.
        let paused = state.flow.lock().expect("flow poisoned").paused;
        if paused {
            state.resume_notify.notified().await;
            // Loop around to re-check (a notify could fire spuriously
            // after stop drained TERMINALS).
            if !TERMINALS.lock().contains_key(&agent_id) {
                return;
            }
            continue;
        }
        // Blocking read on a worker thread. Move the reader in and back
        // so we don't borrow across `.await`.
        let read_result = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; READ_CHUNK_BYTES];
            match reader.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    (reader, Ok(buf))
                }
                Err(e) => (reader, Err(e)),
            }
        })
        .await;
        let (returned_reader, chunk) = match read_result {
            Ok(t) => t,
            Err(e) => {
                kernel
                    .emit(
                        &emit_target,
                        json!({"type": "error", "error": format!("read join: {e}")}),
                    )
                    .await;
                cleanup_on_exit(&agent_id, &kernel, &emit_target, &state).await;
                return;
            }
        };
        reader = returned_reader;
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                // EIO on a closed PTY is the normal "child exited" path
                // on Linux; treat any read error as EOF.
                tracing::debug!(?e, "terminal_backend: reader EOF/error");
                cleanup_on_exit(&agent_id, &kernel, &emit_target, &state).await;
                return;
            }
        };
        if chunk.is_empty() {
            // EOF — child exited.
            cleanup_on_exit(&agent_id, &kernel, &emit_target, &state).await;
            return;
        }
        // Incremental UTF-8 decode. `encoding_rs::Decoder::decode_to_string`
        // buffers a trailing partial char until the next read completes it.
        let mut out = String::with_capacity(chunk.len() + 16);
        let mut decoder = state.decoder.lock().await;
        let (_result, _read, _had_replacements) = decoder.decode_to_string(&chunk, &mut out, false);
        drop(decoder);
        if !out.is_empty() {
            let nbytes = out.len();
            // Append to scrollback ring + trim past MAX_SCROLLBACK_BYTES.
            // A client calls `output` on connect to fetch this so
            // late-arriving clients see context, not just live tail.
            {
                let mut sb = state.scrollback.lock().expect("scrollback poisoned");
                sb.push_back(out.clone());
                let mut total =
                    state.scrollback_bytes.fetch_add(nbytes, Ordering::Relaxed) + nbytes;
                while total > MAX_SCROLLBACK_BYTES {
                    let Some(old) = sb.pop_front() else { break };
                    let drop_bytes = old.len();
                    total = total.saturating_sub(drop_bytes);
                    state
                        .scrollback_bytes
                        .fetch_sub(drop_bytes, Ordering::Relaxed);
                }
            }
            kernel
                .emit(&emit_target, json!({"type": "output", "data": out}))
                .await;
            let (new_unacked, became_paused) = {
                let mut flow = state.flow.lock().expect("flow poisoned");
                flow.unacked_bytes = flow.unacked_bytes.saturating_add(nbytes);
                let cross = !flow.paused && flow.unacked_bytes >= FLOW_PAUSE_THRESHOLD;
                if cross {
                    flow.paused = true;
                }
                (flow.unacked_bytes, cross)
            };
            state.unacked_atomic.store(new_unacked, Ordering::Relaxed);
            let _ = became_paused; // loop top re-checks `paused`
        }
    }
}

async fn cleanup_on_exit(
    _agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    parent: &AgentId,
    state: &Arc<PtyState>,
) {
    // Best-effort: wait for the child so the exit code lands. The
    // child mutex may be contended on hot stop paths; non-blocking
    // `try_wait` first, then a brief block.
    let code = {
        let mut child = state.child.lock().await;
        match child.try_wait() {
            Ok(Some(s)) => s.exit_code() as i32,
            _ => match child.wait() {
                Ok(s) => s.exit_code() as i32,
                Err(_) => -1,
            },
        }
    };
    // Mark the state as exited but keep it in TERMINALS so the
    // scrollback ring survives for `output` queries. A subsequent
    // `spawn` will replace the entry with a fresh PtyState.
    state.exited.store(true, Ordering::Relaxed);
    // `parent` here is actually the emit_target (= self, the
    // terminal_backend's own id). Mirrors Python's `{type:"closed"}`
    // on the agent's own inbox; a client's `closed` listener catches it.
    kernel.emit(parent, json!({"type": "closed"})).await;
    let _ = code;
}

// ── meta + helpers ───────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

fn meta_u64(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<u64> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_u64)
}

fn meta_bool(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<bool> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_bool)
}

fn meta_string_array(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<Vec<String>> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    let arr = meta.get(key)?.as_array()?;
    let v: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn meta_env(agent_id: &AgentId, kernel: &Kernel) -> Vec<(String, String)> {
    let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
        return Vec::new();
    };
    let meta = agent.meta.read().expect("meta poisoned");
    let Some(obj) = meta.get("env").and_then(Value::as_object) else {
        return Vec::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

/// Detect the user's login shell. Mirrors Python's `_detect_shell`:
/// honour `$SHELL` if it points at an existing file, otherwise probe
/// the standard locations. Falls back to `/bin/sh` as a last resort.
///
/// Pass `-l` (login flag) so the spawned shell sources the user's
/// profile (`.zprofile` / `.bash_profile` / `.profile`). The shell is
/// already interactive (attached to a PTY), so `.zshrc` / `.bashrc`
/// also load via the shell's normal interactive-detection path.
/// Without `-l`, opening a fantastic terminal would have a barebones
/// PATH and no aliases — surprising compared to Terminal.app, which
/// always spawns its shell as a login shell.
///
/// `/bin/sh` doesn't need `-l` since it has no profile (and POSIX sh
/// doesn't accept `-l` anyway), so it's the one branch without the flag.
fn default_cmd() -> Vec<String> {
    if cfg!(windows) {
        return vec!["powershell".to_string()];
    }
    if let Ok(sh) = std::env::var("SHELL") {
        if !sh.is_empty() && std::path::Path::new(&sh).is_file() {
            if sh.ends_with("/sh") {
                return vec![sh];
            }
            return vec![sh, "-l".to_string()];
        }
    }
    for cand in ["/bin/zsh", "/bin/bash"] {
        if std::path::Path::new(cand).is_file() {
            return vec![cand.to_string(), "-l".to_string()];
        }
    }
    vec!["/bin/sh".to_string()]
}

/// Mint a short hex token from a mix of clock + stack + pid. Good
/// enough for "two paste files in the same session don't collide";
/// this is NOT a security primitive.
fn rand_hex() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let pid = std::process::id() as u64;
    nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ stack_ptr ^ pid.wrapping_shl(17)
}

fn parse_signal(s: &str) -> Option<i32> {
    let up = s.trim_start_matches("SIG").to_uppercase();
    let n = match up.as_str() {
        "INT" => libc::SIGINT,
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "WINCH" => libc::SIGWINCH,
        _ => return s.parse::<i32>().ok(),
    };
    Some(n)
}

// pull the write_all extension into scope for the verb impls
use std::io::Write as _;

#[cfg(test)]
mod tests;
