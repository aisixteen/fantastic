//! terminal bundle — PTY shell session as an agent.
//!
//! A port of python `bundled_agents/terminal/src/terminal_backend/tools.py`,
//! verb for verb. State is in-memory only (one PTY per agent). Output is
//! emitted as `{type:"output", data:"..."}` events on the agent's OWN inbox so
//! a browser tab watching the agent receives the stream live; `{type:"closed"}`
//! lands when the child exits.
//!
//! Flow control (VSCode's integrated terminal, as python ports it): the reader
//! counts every char it emits; once more than [`HIGH_WATERMARK`] chars sit
//! unacknowledged the PTY reader is DETACHED ("paused") so the kernel PTY
//! buffer fills and the shell naturally throttles. A streaming consumer acks
//! each chunk after xterm has parsed it via the `ack` verb; once the backlog
//! drains below [`LOW_WATERMARK`] the reader re-attaches. The synchronous
//! `shell` verb drains via scrollback, not the emit stream — it is exempt (no
//! pause while a done-token is pending; flow state is cleared around each call).
//!
//! `paste_image` rides the BINARY channel only: python's `data` is a `bytes`
//! value, and the text channel carries none (base64 on the wire is banned).

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{
    os_error_class, os_error_str, py_float, py_int, py_int_or, py_repr, py_repr_value, py_str,
    py_truthy, py_type_name, PyError,
};
use fantastic_file::fs;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "terminal_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// python `MAX_SCROLLBACK`.
pub const MAX_SCROLLBACK: i64 = 256 * 1024;

/// python `DEFAULT_COLS`.
pub const DEFAULT_COLS: i64 = 200;

/// python `DEFAULT_ROWS`.
pub const DEFAULT_ROWS: i64 = 50;

/// python `HIGH_WATERMARK` — pause the reader past this many unacked chars.
pub const HIGH_WATERMARK: usize = 100_000;

/// python `LOW_WATERMARK` — re-attach the reader once the backlog is below it.
pub const LOW_WATERMARK: usize = 5_000;

/// python `MAX_PASTE_IMAGE`.
pub const MAX_PASTE_IMAGE: usize = 5 * 1024 * 1024;

/// python `_IMAGE_EXT` — the formats Claude Code accepts, keyed by MIME.
const IMAGE_EXT: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

/// python `os.read(fd, 4096)`.
const READ_CHUNK_BYTES: usize = 4096;

// ── per-agent state ──────────────────────────────────────────────────

/// python's `_procs[id]` — per-agent runtime state, NEVER persisted.
struct PtyState {
    /// Child pid (python `state["pid"]`).
    pid: libc::pid_t,
    /// PTY master fd (python `state["fd"]`).
    fd: libc::c_int,
    /// The resolved command (python `state["cmd"]`).
    cmd: String,
    /// python `state["write_lock"]` — two `_write_all` loops on one
    /// non-blocking fd would interleave a bracketed-paste sequence.
    write_lock: Mutex<()>,
    /// Everything the reader and the verbs share.
    inner: StdMutex<Inner>,
}

/// The mutable half of [`PtyState`].
struct Inner {
    scrollback: VecDeque<String>,
    scrollback_bytes: usize,
    cols: i64,
    rows: i64,
    unacked: usize,
    paused: bool,
    reader_attached: bool,
    /// The reader's PRIVATE dup of the master. `_cleanup` may close the
    /// session's fd while the reader sits in `poll`, and a bare number could
    /// then be REUSED — so the reader polls its own, and detaching closes it.
    reader_fd: Option<libc::c_int>,
    paste_dir: Option<PathBuf>,
    /// python `state["decoder"]` — the bytes of a multi-byte char split
    /// across two reads.
    decoder: encoding_rs::Decoder,
}

/// python's `asyncio.Event` for one pending done-token.
struct DoneEvent {
    fired: AtomicBool,
    notify: Notify,
}

impl DoneEvent {
    fn new() -> Self {
        Self {
            fired: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
    fn set(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    /// python `await event.wait()`.
    async fn wait(&self) {
        loop {
            if self.fired.load(Ordering::SeqCst) {
                return;
            }
            let waiter = self.notify.notified();
            if self.fired.load(Ordering::SeqCst) {
                return;
            }
            waiter.await;
        }
    }
}

/// A process-global map behind a `const fn new` static.
struct Registry<T>(OnceLock<StdMutex<HashMap<AgentId, T>>>);

impl<T> Registry<T> {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, T>> {
        self.0
            .get_or_init(|| StdMutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// python `_procs`.
static PROCS: Registry<Arc<PtyState>> = Registry::new();

/// python `_pending_tokens`.
static PENDING_TOKENS: Registry<HashMap<String, Arc<DoneEvent>>> = Registry::new();

fn proc_of(agent_id: &AgentId) -> Option<Arc<PtyState>> {
    PROCS.lock().get(agent_id).cloned()
}

fn has_pending_tokens(agent_id: &AgentId) -> bool {
    PENDING_TOKENS
        .lock()
        .get(agent_id)
        .is_some_and(|m| !m.is_empty())
}

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
        let verb = payload.get("type").unwrap_or(&Value::Null);
        let reply = match verb.as_str().unwrap_or("") {
            "reflect" => reflect(agent_id, kernel),
            "boot" => boot(agent_id, kernel)?,
            "write" => write_verb(agent_id, payload).await?,
            "ack" => ack(agent_id, payload, kernel)?,
            // The image is the blob of a binary frame; the text channel carries
            // no bytes, and python says exactly this about a `data` that is not.
            "paste_image" => match proc_of(agent_id) {
                None => not_running(),
                Some(_) => json!({"error": "paste_image: data must be bytes"}),
            },
            "shell" => shell(agent_id, payload, kernel).await?,
            "resize" => resize(agent_id, payload)?,
            "output" => output(agent_id, payload)?,
            "restart" => restart(agent_id, kernel)?,
            "signal" => signal(agent_id, payload)?,
            "stop" => {
                cleanup(agent_id);
                json!({"stopped": true})
            }
            _ => json!({"error": format!("terminal: unknown type {}", py_repr_value(verb))}),
        };
        Ok(Some(reply))
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), BundleError> {
        if header.get("type").and_then(Value::as_str) == Some("paste_image") {
            let mime = header.get("mime").cloned().unwrap_or(Value::Null);
            return Ok((Some(paste_image(agent_id, blob, &mime).await?), Vec::new()));
        }
        let reply = self.handle(agent_id, &header, kernel).await?;
        Ok((reply, Vec::new()))
    }

    /// python `on_delete` — the PTY must not outlive its agent record.
    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        cleanup(agent_id);
        Ok(())
    }
}

fn not_running() -> Value {
    json!({"error": "not running"})
}

/// python's raised reply: the kernel renders `f"{type(e).__name__}: {e}"`.
fn raised(class: &str, message: impl std::fmt::Display) -> BundleError {
    format!("{class}: {message}").into()
}

fn raised_py(e: &PyError) -> BundleError {
    raised(e.class_name(), e.message())
}

// ── verbs ────────────────────────────────────────────────────────────

/// python `_reflect`.
fn reflect(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let state = proc_of(agent_id);
    let rec = record(agent_id, kernel);
    let snap = state.as_ref().map(|s| s.snapshot());
    let command = match state.as_ref().filter(|s| !s.cmd.is_empty()) {
        Some(s) => json!(s.cmd),
        None => match rec.get("command") {
            Some(v) if py_truthy(v) => v.clone(),
            _ => json!(detect_shell()),
        },
    };
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(agent_id.as_str()));
    obj.insert("sentence".to_string(), json!("PTY shell session."));
    obj.insert("command".to_string(), command);
    obj.insert("running".to_string(), json!(state.is_some()));
    obj.insert(
        "pid".to_string(),
        state
            .as_ref()
            .map_or(Value::Null, |s| json!(i64::from(s.pid))),
    );
    // A stopped terminal reports the RAW record value (python's
    // `rec.get("cols", DEFAULT_COLS)` — no `int()` on this path).
    obj.insert(
        "cols".to_string(),
        snap.as_ref().map_or_else(
            || rec.get("cols").cloned().unwrap_or(json!(DEFAULT_COLS)),
            |s| json!(s.cols),
        ),
    );
    obj.insert(
        "rows".to_string(),
        snap.as_ref().map_or_else(
            || rec.get("rows").cloned().unwrap_or(json!(DEFAULT_ROWS)),
            |s| json!(s.rows),
        ),
    );
    obj.insert(
        "scrollback_bytes".to_string(),
        json!(snap.as_ref().map_or(0, |s| s.scrollback_bytes)),
    );
    obj.insert(
        "paused".to_string(),
        json!(snap.as_ref().is_some_and(|s| s.paused)),
    );
    obj.insert(
        "unacked".to_string(),
        json!(snap.as_ref().map_or(0, |s| s.unacked)),
    );
    obj.insert("verbs".to_string(), verbs_doc());
    obj.insert(
        "emits".to_string(),
        json!({
            "output": "{type:'output', data:str} — every PTY read chunk; UTF-8-decoded bytes",
            "closed": "{type:'closed'} — child process exited (EOF on the PTY fd)",
        }),
    );
    Value::Object(obj)
}

/// The first docstring line of each verb, in python's `VERBS` order.
fn verbs_doc() -> Value {
    json!({
        "reflect": "Identity + PTY state. No args. `running`/`scrollback_bytes`/`paused`/`unacked` are process-local — read via the live serve to get truth.",
        "boot": "Idempotent. Spawns the PTY child if not already running; clears stale flow-control state so a freshly-mounted streamer starts clean. Returns {running:true}.",
        "write": "args: data:str (req). Writes UTF-8 bytes to PTY stdin IN FULL — loops over the non-blocking fd so large pastes aren't truncated; serialized per-agent so concurrent writes can't interleave a bracketed-paste sequence. Returns {written:int}.",
        "ack": "args: chars:int. Flow-control ack from a streaming consumer (terminal_view (the TS frontend) acks each chunk after xterm parses it). Decrements the unacked-char count; re-attaches the PTY reader once it drops below LOW_WATERMARK. Returns {unacked:int, paused:bool}.",
        "paste_image": "args: data:bytes (req), mime:str? (default image/png). Saves a pasted image to a per-agent scratch file and types its absolute path into the PTY. Bridges image paste for a CLI (e.g. claude) running in a browser xterm — the server can't reach the browser clipboard, so path injection mimics a file drag-drop. Returns {path:str, bytes:int}.",
        "shell": "args: cmd:str (req), timeout:float? (default 30s). Synchronous run via done-token; returns {cmd, output, completed:bool, error?}.",
        "resize": "args: cols:int, rows:int. Fires SIGWINCH so TUI apps redraw. Returns {resized, cols, rows}.",
        "output": "args: max_bytes:int? (default MAX_SCROLLBACK=256K). Returns {output:str} — tail of the PTY scrollback.",
        "restart": "No args. SIGKILLs the PTY child and respawns with the same command/cwd/cols/rows. Returns {restarted:true}.",
        "signal": "args: signal:int? (default SIGINT=2). os.kill(pid, signal). Returns {signal:int} or {error}.",
        "stop": "No args. Closes the PTY fd and SIGKILLs the child. Returns {stopped:true}.",
    })
}

/// python `_boot`.
fn boot(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<Value, BundleError> {
    spawn(agent_id, kernel)?;
    clear_flow_control(agent_id, kernel);
    Ok(json!({"running": true}))
}

/// python `_write`.
async fn write_verb(agent_id: &AgentId, payload: &Value) -> Result<Value, BundleError> {
    let Some(state) = proc_of(agent_id) else {
        return Ok(not_running());
    };
    // python `data.encode("utf-8")` on whatever `data` is — a non-string has no
    // `encode`, and the dispatch fails with python's AttributeError.
    let data = payload.get("data").unwrap_or(&Value::Null);
    let text = match data {
        Value::String(s) => s.clone(),
        Value::Null if payload.get("data").is_none() => String::new(),
        other => {
            return Err(raised(
                "AttributeError",
                format!("'{}' object has no attribute 'encode'", py_type_name(other)),
            ))
        }
    };
    let bytes = text.into_bytes();
    let _guard = state.write_lock.lock().await;
    match write_all(&state, bytes).await {
        Ok(n) => Ok(json!({ "written": n })),
        Err(errno) => Ok(json!({ "error": os_error_str(errno, None) })),
    }
}

/// python `_ack`.
fn ack(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Value, BundleError> {
    let Some(state) = proc_of(agent_id) else {
        return Ok(not_running());
    };
    let chars = py_int(payload.get("chars").unwrap_or(&json!(0))).map_err(|e| raised_py(&e))?;
    let (unacked, paused, resume) = {
        let mut inner = state.lock();
        inner.unacked = inner
            .unacked
            .saturating_sub(usize::try_from(chars.max(0)).unwrap_or(usize::MAX));
        let resume = inner.paused && inner.unacked < LOW_WATERMARK;
        if resume {
            inner.paused = false;
        }
        (inner.unacked, inner.paused, resume)
    };
    if resume {
        attach_reader(agent_id, kernel, &state);
    }
    Ok(json!({"unacked": unacked, "paused": paused}))
}

/// python `_paste_image` (binary channel: `data` IS bytes).
async fn paste_image(
    agent_id: &AgentId,
    data: Vec<u8>,
    mime_value: &Value,
) -> Result<Value, BundleError> {
    let Some(state) = proc_of(agent_id) else {
        return Ok(not_running());
    };
    if data.len() > MAX_PASTE_IMAGE {
        return Ok(json!({
            "error": format!("paste_image: {} bytes exceeds the 5 MB cap", data.len())
        }));
    }
    let mime = if py_truthy(mime_value) {
        py_str(mime_value).to_lowercase()
    } else {
        "image/png".to_string()
    };
    let Some((_, ext)) = IMAGE_EXT.iter().find(|(m, _)| *m == mime) else {
        return Ok(json!({
            "error": format!("paste_image: unsupported image type {}", py_repr(&mime))
        }));
    };
    // Per-agent scratch dir, created lazily — most terminals never paste an
    // image, so don't mint a temp dir for every spawn. A mkdtemp failure
    // RAISES (python lets the OSError out of the verb).
    let dir = {
        let existing = state.lock().paste_dir.clone();
        match existing {
            Some(d) => d,
            None => {
                let d = mkdtemp(&format!("fantastic_paste_{}_", agent_id.as_str()))
                    .map_err(|errno| raised(os_error_class(errno), os_error_str(errno, None)))?;
                state.lock().paste_dir = Some(d.clone());
                d
            }
        }
    };
    let fname = format!("paste_{}.{ext}", token_hex(4));
    // The scratch dir is a system temp dir (outside cwd by design) — write
    // through fs's external surface (the single disk module).
    if let Err(e) = fs::write_bytes(&dir, &fname, &data, false) {
        return Ok(json!({ "error": e }));
    }
    let path = dir.join(&fname).to_string_lossy().to_string();
    // Type the path at the PTY cursor with a trailing space (no newline —
    // pasting an image must not submit; it mirrors a drag-drop).
    let _guard = state.write_lock.lock().await;
    if let Err(errno) = write_all(&state, format!("{path} ").into_bytes()).await {
        return Ok(json!({ "error": os_error_str(errno, None) }));
    }
    Ok(json!({"path": path, "bytes": data.len()}))
}

/// python `_resize`.
fn resize(agent_id: &AgentId, payload: &Value) -> Result<Value, BundleError> {
    let Some(state) = proc_of(agent_id) else {
        return Ok(not_running());
    };
    let cols =
        py_int(payload.get("cols").unwrap_or(&json!(DEFAULT_COLS))).map_err(|e| raised_py(&e))?;
    let rows =
        py_int(payload.get("rows").unwrap_or(&json!(DEFAULT_ROWS))).map_err(|e| raised_py(&e))?;
    // python STORES first, then packs the winsize — an out-of-range value
    // raises out of `struct.pack`, after the record of it landed.
    {
        let mut inner = state.lock();
        inner.cols = cols;
        inner.rows = rows;
    }
    set_winsize(state.fd, cols, rows)?;
    Ok(json!({"resized": true, "cols": cols, "rows": rows}))
}

/// python `_output` — a code-point slice of the scrollback tail.
fn output(agent_id: &AgentId, payload: &Value) -> Result<Value, BundleError> {
    let max_bytes = py_int(payload.get("max_bytes").unwrap_or(&json!(MAX_SCROLLBACK)))
        .map_err(|e| raised_py(&e))?;
    let text = scrollback_text(agent_id);
    let out = if max_bytes < i64::try_from(text.len()).unwrap_or(i64::MAX) {
        // python `text[-max_bytes:]`: 0 is the WHOLE string, a negative count
        // drops that many leading code points.
        let chars: Vec<char> = text.chars().collect();
        let keep: usize = match max_bytes {
            0 => chars.len(),
            n if n > 0 => usize::try_from(n).unwrap_or(usize::MAX).min(chars.len()),
            n => chars
                .len()
                .saturating_sub(usize::try_from(-n).unwrap_or(usize::MAX)),
        };
        chars[chars.len() - keep..].iter().collect()
    } else {
        text
    };
    Ok(json!({ "output": out }))
}

/// python `_restart`.
fn restart(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<Value, BundleError> {
    cleanup(agent_id);
    spawn(agent_id, kernel)?;
    Ok(json!({"restarted": true}))
}

/// python `_signal` — `os.kill(state["pid"], sig)`, the CHILD pid only.
fn signal(agent_id: &AgentId, payload: &Value) -> Result<Value, BundleError> {
    let Some(state) = proc_of(agent_id) else {
        return Ok(not_running());
    };
    let sig =
        py_int(payload.get("signal").unwrap_or(&json!(libc::SIGINT))).map_err(|e| raised_py(&e))?;
    // python's `os.kill` takes a Py_ssize_t and casts it to C `int`.
    let rc = unsafe { libc::kill(state.pid, sig as libc::c_int) };
    if rc != 0 {
        return Ok(json!({ "error": os_error_str(errno(), None) }));
    }
    Ok(json!({ "signal": sig }))
}

/// python `_shell` — one command line, its output delimited by a done-token.
async fn shell(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    let Some(state) = proc_of(agent_id) else {
        return Ok(json!({"error": "shell: not running; call boot first"}));
    };
    let cmd = payload.get("cmd").cloned().unwrap_or(json!(""));
    if !py_truthy(&cmd) {
        return Ok(json!({"error": "shell: cmd required"}));
    }
    let timeout =
        py_float(payload.get("timeout").unwrap_or(&json!(30.0))).map_err(|e| raised_py(&e))?;

    // `shell` drains via scrollback, not the emit stream — it can't be left at
    // the mercy of a streamer's stale flow-control pause. Clear it on the way
    // in, and again on the way out.
    clear_flow_control(agent_id, kernel);
    let token = format!("__DONE_{}__", token_hex(8));
    let event = Arc::new(DoneEvent::new());
    PENDING_TOKENS
        .lock()
        .entry(agent_id.clone())
        .or_default()
        .insert(token.clone(), Arc::clone(&event));
    let out = shell_body(agent_id, &state, &cmd, timeout, &token, &event).await;
    {
        let mut pending = PENDING_TOKENS.lock();
        if let Some(map) = pending.get_mut(agent_id) {
            map.remove(&token);
            if map.is_empty() {
                pending.remove(agent_id);
            }
        }
    }
    clear_flow_control(agent_id, kernel);
    out
}

async fn shell_body(
    agent_id: &AgentId,
    state: &Arc<PtyState>,
    cmd: &Value,
    timeout: f64,
    token: &str,
    event: &Arc<DoneEvent>,
) -> Result<Value, BundleError> {
    let before_text = scrollback_text(agent_id);
    let line = format!("{}; printf '\\n%s\\n' '{token}'\n", py_str(cmd));
    {
        let _guard = state.write_lock.lock().await;
        if let Err(errno) = write_all(state, line.into_bytes()).await {
            return Ok(json!({ "error": os_error_str(errno, None) }));
        }
    }
    let completed = tokio::time::timeout(secs(timeout), event.wait())
        .await
        .is_ok();
    if !completed {
        // Cmd is still running — send Ctrl-C so the PTY shell is ready for the
        // next call, then wait briefly for a token that was already printing.
        // python keeps `completed = False` either way: the reply says the call
        // timed out even when the late token lands.
        unsafe { libc::write(state.fd, [0x03u8].as_ptr().cast(), 1) };
        let _ = tokio::time::timeout(secs(0.5), event.wait()).await;
    }
    let after_text = scrollback_text(agent_id);
    let before: Vec<char> = before_text.chars().collect();
    let after: Vec<char> = after_text.chars().collect();
    let delta: Vec<char> = if after.len() >= before.len() && after[..before.len()] == before[..] {
        after[before.len()..].to_vec()
    } else {
        after
    };
    // The token appears TWICE: once in the echoed command line (followed by
    // `'`), once as printf's output (followed by \r\n). Match the second one
    // and slice before it.
    let tok_idx = find_chars(&delta, &format!("{token}\r\n"))
        .or_else(|| find_chars(&delta, &format!("{token}\n")));
    let output: String = match tok_idx {
        Some(i) => {
            // Walk back to the \n that starts the printf's leading newline.
            let line_start = delta[..i].iter().rposition(|c| *c == '\n');
            let end = line_start.unwrap_or(i);
            let mut cut = &delta[..end];
            while matches!(cut.last(), Some('\r' | '\n')) {
                cut = &cut[..cut.len() - 1];
            }
            cut.iter().collect()
        }
        None => delta.iter().collect(),
    };
    if !completed {
        return Ok(json!({
            "cmd": cmd, "output": output, "completed": false, "error": "timeout"
        }));
    }
    Ok(json!({"cmd": cmd, "output": output, "completed": true}))
}

fn secs(t: f64) -> Duration {
    Duration::from_secs_f64(if t.is_finite() && t > 0.0 { t } else { 0.0 })
}

fn find_chars(hay: &[char], needle: &str) -> Option<usize> {
    let pat: Vec<char> = needle.chars().collect();
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    (0..=hay.len() - pat.len()).find(|&i| hay[i..i + pat.len()] == pat[..])
}

// ── spawn / cleanup ──────────────────────────────────────────────────

/// python `_spawn` — fork a PTY child from the agent's record.
fn spawn(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
    if PROCS.lock().contains_key(agent_id) {
        return Ok(());
    }
    let rec = record(agent_id, kernel);
    let cmd = match rec.get("command") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => detect_shell(),
    };
    let args: Vec<String> = rec
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(py_str).collect())
        .unwrap_or_default();
    let cwd = match rec.get("cwd") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
    };
    let cols = py_int_or(rec.get("cols").unwrap_or(&Value::Null), DEFAULT_COLS)
        .map_err(|e| raised_py(&e))?;
    let rows = py_int_or(rec.get("rows").unwrap_or(&Value::Null), DEFAULT_ROWS)
        .map_err(|e| raised_py(&e))?;

    let (pid, fd) = fork_pty(&cmd, &args, &cwd, cols, rows)
        .map_err(|errno| raised(os_error_class(errno), os_error_str(errno, None)))?;
    // python `_set_winsize` swallows the ioctl error; an out-of-range value
    // raised out of `struct.pack` before the fork's winsize was built.
    let _ = set_winsize(fd, cols, rows);
    let state = Arc::new(PtyState {
        pid,
        fd,
        cmd,
        write_lock: Mutex::new(()),
        inner: StdMutex::new(Inner {
            scrollback: VecDeque::new(),
            scrollback_bytes: 0,
            cols,
            rows,
            unacked: 0,
            paused: false,
            reader_attached: false,
            reader_fd: None,
            paste_dir: None,
            decoder: encoding_rs::UTF_8.new_decoder(),
        }),
    });
    PROCS.lock().insert(agent_id.clone(), Arc::clone(&state));
    attach_reader(agent_id, kernel, &state);
    Ok(())
}

/// python `_cleanup`.
fn cleanup(agent_id: &AgentId) {
    detach_reader(agent_id);
    let Some(state) = PROCS.lock().remove(agent_id) else {
        return;
    };
    unsafe {
        libc::close(state.fd);
        libc::kill(state.pid, libc::SIGKILL);
        // Reap so the child isn't left a zombie. Best-effort — and only AFTER
        // every fd of this PTY is closed (the reader's included, above): a
        // session leader stuck revoking a still-open controlling terminal never
        // finishes exiting, and this wait would never return.
        let mut status: libc::c_int = 0;
        libc::waitpid(state.pid, &mut status, 0);
    }
    // Drop the per-agent pasted-image scratch dir, if one was created (system
    // temp dir → fs's external surface; ignore if already gone).
    let dir = state.lock().paste_dir.take();
    if let Some(dir) = dir {
        let _ = fs::remove(&dir, "", true);
    }
}

/// python `_clear_flow_control` — reset the accounting, force-resume the reader.
fn clear_flow_control(agent_id: &AgentId, kernel: &Arc<Kernel>) {
    let Some(state) = proc_of(agent_id) else {
        return;
    };
    let resume = {
        let mut inner = state.lock();
        inner.unacked = 0;
        let was_paused = inner.paused;
        inner.paused = false;
        was_paused
    };
    if resume {
        attach_reader(agent_id, kernel, &state);
    }
}

/// python `_attach_reader` — attach the reader AND register it as this agent's
/// work, so a streaming PTY is visible in `ps` and reachable by `cancel`.
fn attach_reader(agent_id: &AgentId, kernel: &Arc<Kernel>, state: &Arc<PtyState>) {
    {
        let mut inner = state.lock();
        if inner.reader_attached {
            return;
        }
        let fd = unsafe { libc::fcntl(state.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return;
        }
        inner.reader_fd = Some(fd);
        inner.reader_attached = true;
    }
    let holder_agent = agent_id.clone();
    let holder_kernel = Arc::clone(kernel);
    let holder_state = Arc::clone(state);
    // The holder task OWNS the attachment: it reads while attached and then,
    // like python's `_hold`, lives until it is cancelled.
    kernel.spawn_task(agent_id, "pty_reader", async move {
        read_loop(&holder_agent, &holder_kernel, &holder_state).await;
        std::future::pending::<()>().await;
    });
}

/// python `_detach_reader`.
fn detach_reader(agent_id: &AgentId) {
    let Some(state) = proc_of(agent_id) else {
        return;
    };
    let mut inner = state.lock();
    inner.reader_attached = false;
    if let Some(fd) = inner.reader_fd.take() {
        unsafe { libc::close(fd) };
    }
}

// ── the reader (python `_on_readable`) ───────────────────────────────

async fn read_loop(agent_id: &AgentId, kernel: &Arc<Kernel>, state: &Arc<PtyState>) {
    loop {
        // python's `_on_readable` starts from `_procs.get(agent_id)`: a state
        // that was dropped or replaced ends this reader.
        match proc_of(agent_id) {
            Some(cur) if Arc::ptr_eq(&cur, state) => {}
            _ => break,
        }
        // The attachment OWNS the fd: a detach closes it, so a poll that was
        // already in flight must not consume what it read.
        let Some(fd) = ({
            let inner = state.lock();
            inner.reader_attached.then_some(inner.reader_fd).flatten()
        }) else {
            break;
        };
        let chunk = match read_ready(fd).await {
            Readable::Timeout => continue,
            Readable::Retry => continue,
            Readable::Eof => {
                // EOF — the child exited.
                detach_reader(agent_id);
                kernel.emit(agent_id, json!({"type": "closed"})).await;
                cleanup(agent_id);
                break;
            }
            Readable::Data(b) => b,
        };
        if !state.lock().reader_attached {
            break;
        }
        let text = decode(state, &chunk);
        append_scrollback(state, &text);
        // Signal any pending done-tokens. We look for `<token>\r\n` — which the
        // printf produces, but the command echo line does NOT.
        fire_tokens(agent_id);
        kernel
            .emit(agent_id, json!({"type": "output", "data": text}))
            .await;
        // Flow control: account the emitted chars and pause the reader if the
        // streaming consumer is falling behind. A `shell` call in flight drains
        // via scrollback and never acks — don't pause under it.
        let pause = {
            let mut inner = state.lock();
            inner.unacked += text.chars().count();
            !has_pending_tokens(agent_id) && !inner.paused && inner.unacked > HIGH_WATERMARK
        };
        if pause {
            state.lock().paused = true;
            detach_reader(agent_id);
        }
    }
}

enum Readable {
    Data(Vec<u8>),
    Eof,
    Retry,
    Timeout,
}

/// One `poll`-then-`read` step, off the runtime. `poll` is what python's
/// `loop.add_reader` does for us: nothing is read while the reader is detached.
async fn read_ready(fd: libc::c_int) -> Readable {
    tokio::task::spawn_blocking(move || {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
        if rc == 0 {
            return Readable::Timeout;
        }
        if rc < 0 {
            return if errno() == libc::EINTR {
                Readable::Retry
            } else {
                Readable::Eof
            };
        }
        let mut buf = vec![0u8; READ_CHUNK_BYTES];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            buf.truncate(usize::try_from(n).unwrap_or(0));
            return Readable::Data(buf);
        }
        if n == 0 {
            return Readable::Eof;
        }
        match errno() {
            libc::EAGAIN | libc::EINTR => Readable::Retry,
            // Linux spells a hung-up PTY master `EIO`; macOS returns 0.
            _ => Readable::Eof,
        }
    })
    .await
    .unwrap_or(Readable::Eof)
}

/// python's incremental UTF-8 decoder (`errors="replace"`): a partial tail is
/// buffered until the next read completes it; an invalid byte is ONE U+FFFD.
fn decode(state: &Arc<PtyState>, chunk: &[u8]) -> String {
    let mut inner = state.lock();
    let mut out = String::new();
    let mut rest = chunk;
    loop {
        let need = inner
            .decoder
            .max_utf8_buffer_length(rest.len())
            .unwrap_or(rest.len() * 3 + 3);
        out.reserve(need);
        let (result, read, _) = inner.decoder.decode_to_string(rest, &mut out, false);
        match result {
            encoding_rs::CoderResult::InputEmpty => return out,
            // The buffer could not take the whole chunk — keep going from where
            // the decoder stopped instead of dropping the tail.
            encoding_rs::CoderResult::OutputFull => rest = &rest[read..],
        }
    }
}

/// python `_append_scrollback`.
fn append_scrollback(state: &Arc<PtyState>, text: &str) {
    let mut inner = state.lock();
    inner.scrollback.push_back(text.to_string());
    inner.scrollback_bytes += text.len();
    while inner.scrollback_bytes > MAX_SCROLLBACK as usize && !inner.scrollback.is_empty() {
        let old = inner.scrollback.pop_front().unwrap_or_default();
        inner.scrollback_bytes -= old.len();
    }
}

fn scrollback_text(agent_id: &AgentId) -> String {
    match proc_of(agent_id) {
        None => String::new(),
        Some(state) => {
            let inner = state.lock();
            inner.scrollback.iter().fold(String::new(), |mut acc, s| {
                acc.push_str(s);
                acc
            })
        }
    }
}

fn fire_tokens(agent_id: &AgentId) {
    let pending: Vec<(String, Arc<DoneEvent>)> = {
        let map = PENDING_TOKENS.lock();
        match map.get(agent_id) {
            None => return,
            Some(m) => m.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect(),
        }
    };
    if pending.is_empty() {
        return;
    }
    let full = scrollback_text(agent_id);
    for (tok, ev) in pending {
        if full.contains(&format!("{tok}\r\n")) || full.contains(&format!("{tok}\n")) {
            ev.set();
        }
    }
}

// ── PTY plumbing ─────────────────────────────────────────────────────

impl PtyState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            cols: inner.cols,
            rows: inner.rows,
            scrollback_bytes: inner.scrollback_bytes,
            unacked: inner.unacked,
            paused: inner.paused,
        }
    }
}

struct Snapshot {
    cols: i64,
    rows: i64,
    scrollback_bytes: usize,
    unacked: usize,
    paused: bool,
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// python `_set_winsize` — the ioctl error is swallowed, but an out-of-range
/// value never reaches it: `struct.pack("HHHH", …)` raises first.
fn set_winsize(fd: libc::c_int, cols: i64, rows: i64) -> Result<(), BundleError> {
    let pack = |n: i64| -> Result<u16, BundleError> {
        u16::try_from(n).map_err(|_| {
            // `struct.error.__name__` is `error`.
            raised("error", "'H' format requires 0 <= number <= 65535")
        })
    };
    let ws = libc::winsize {
        ws_row: pack(rows)?,
        ws_col: pack(cols)?,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    Ok(())
}

/// python `_write_all` — write the FULL buffer to the non-blocking PTY fd.
/// A short write is resumed where it stopped; a full input buffer is waited out.
async fn write_all(state: &Arc<PtyState>, buf: Vec<u8>) -> Result<usize, i32> {
    let fd = state.fd;
    tokio::task::spawn_blocking(move || {
        let mut written = 0usize;
        while written < buf.len() {
            let n = unsafe { libc::write(fd, buf[written..].as_ptr().cast(), buf.len() - written) };
            if n > 0 {
                written += usize::try_from(n).unwrap_or(0);
                continue;
            }
            match errno() {
                libc::EAGAIN | libc::EINTR => {
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut pfd, 1, 50) };
                }
                e => return Err(e),
            }
        }
        Ok(written)
    })
    .await
    .unwrap_or(Err(libc::EIO))
}

/// python `_spawn`'s `pty.fork()` + the child's `os.execvpe`.
///
/// Everything the child needs is built BEFORE the fork; after it only
/// async-signal-safe calls run. The child must NEVER fall through into the
/// parent's code — an exec failure would otherwise keep running a second copy
/// of the kernel event loop. Exit 127 like a shell does; the message goes to
/// the PTY (fd 2 is the slave).
fn fork_pty(
    cmd: &str,
    args: &[String],
    cwd: &str,
    cols: i64,
    rows: i64,
) -> Result<(libc::pid_t, libc::c_int), i32> {
    let plan = ExecPlan::build(cmd, args, cwd);
    let (argv, envp) = plan.raw_vectors();
    let mut master: libc::c_int = 0;
    let mut ws = libc::winsize {
        ws_row: u16::try_from(rows).unwrap_or(0),
        ws_col: u16::try_from(cols).unwrap_or(0),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if pid < 0 {
        return Err(errno());
    }
    if pid == 0 {
        // ── child ──
        unsafe { plan.exec_or_die(&argv, &envp) };
    }
    unsafe {
        // python attaches the reader to a NON-BLOCKING fd…
        let flags = libc::fcntl(master, libc::F_GETFL, 0);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        // …and python's fds are NON-INHERITABLE (PEP 446). Without this every
        // later PTY child inherits THIS master, so killing this session never
        // releases its terminal and the session leader hangs in exit — a
        // `waitpid` that never returns.
        let fdf = libc::fcntl(master, libc::F_GETFD, 0);
        libc::fcntl(master, libc::F_SETFD, fdf | libc::FD_CLOEXEC);
    }
    Ok((pid, master))
}

/// Everything the forked child needs, allocated in the PARENT.
struct ExecPlan {
    cwd: CString,
    argv_owned: Vec<CString>,
    envp_owned: Vec<CString>,
    /// The candidates `os._execvpe` tries, in order, with the `str(exc)`
    /// filename python would print for each (a bare command searches PATH and
    /// reports the BYTES repr of the candidate it failed on).
    candidates: Vec<(CString, CString)>,
    /// `f"{cmd}: "` — the head of the child's failure line.
    prefix: CString,
}

impl ExecPlan {
    fn build(cmd: &str, args: &[String], cwd: &str) -> Self {
        let cstr = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
        let mut argv_owned: Vec<CString> = vec![cstr(cmd)];
        argv_owned.extend(args.iter().map(|a| cstr(a)));
        let mut envp_owned: Vec<CString> = std::env::vars()
            .filter(|(k, _)| k != "TERM")
            .map(|(k, v)| cstr(&format!("{k}={v}")))
            .collect();
        envp_owned.push(cstr("TERM=xterm-256color"));

        // python `os.execvpe`: a command with a directory part execs straight;
        // a bare one walks `PATH`, and its failure reports the BYTES repr of
        // the candidate (`_execvpe` fsencodes both the file and the path).
        let candidates = if cmd.contains('/') {
            vec![(cstr(cmd), cstr(&py_repr(cmd)))]
        } else {
            let path = std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_string());
            path.split(':')
                .map(|dir| {
                    let full = if dir.is_empty() {
                        cmd.to_string()
                    } else {
                        format!("{}/{cmd}", dir.trim_end_matches('/'))
                    };
                    (cstr(&full), cstr(&format!("b{}", py_repr(&full))))
                })
                .collect()
        };
        Self {
            cwd: cstr(cwd),
            argv_owned,
            envp_owned,
            candidates,
            prefix: cstr(&format!("{cmd}: ")),
        }
    }

    /// The `char *const []` pair `execve` wants — built in the PARENT, so the
    /// child allocates nothing.
    fn raw_vectors(&self) -> (Vec<*const libc::c_char>, Vec<*const libc::c_char>) {
        let ptrs = |v: &[CString]| {
            let mut out: Vec<*const libc::c_char> = v.iter().map(|s| s.as_ptr()).collect();
            out.push(std::ptr::null());
            out
        };
        (ptrs(&self.argv_owned), ptrs(&self.envp_owned))
    }

    /// Runs in the forked child; never returns.
    unsafe fn exec_or_die(&self, argv: &[*const libc::c_char], envp: &[*const libc::c_char]) -> ! {
        // Every OTHER fd of the kernel process — another session's master, a
        // socket, a log — is closed before the exec. python's fds are
        // non-inheritable (PEP 446), so its PTY children never hold a peer's
        // terminal open; ours must not either.
        let max_fd = libc::getdtablesize().min(4096);
        for fd in 3..max_fd {
            libc::close(fd);
        }
        // A bad cwd is python's `except OSError: pass` — the child stays in the
        // kernel's own dir.
        libc::chdir(self.cwd.as_ptr());
        let mut saved: Option<(i32, usize)> = None;
        let mut last: (i32, usize) = (libc::ENOENT, 0);
        for (i, (candidate, _)) in self.candidates.iter().enumerate() {
            libc::execve(candidate.as_ptr(), argv.as_ptr(), envp.as_ptr());
            let e = errno();
            last = (e, i);
            if e != libc::ENOENT && e != libc::ENOTDIR && saved.is_none() {
                saved = Some((e, i));
            }
        }
        let (e, idx) = saved.unwrap_or(last);
        self.report(e, idx);
        libc::_exit(127)
    }

    /// `f"{cmd}: {exc}\r\n"` on fd 2 (the PTY slave), from stack buffers only.
    unsafe fn report(&self, e: i32, idx: usize) {
        let write_c = |s: *const libc::c_char| {
            libc::write(2, s.cast(), libc::strlen(s));
        };
        let write_b = |b: &[u8]| {
            libc::write(2, b.as_ptr().cast(), b.len());
        };
        write_c(self.prefix.as_ptr());
        write_b(b"[Errno ");
        let mut num = [0u8; 24];
        write_b(itoa(e, &mut num));
        write_b(b"] ");
        let mut msg = [0 as libc::c_char; 256];
        if libc::strerror_r(e, msg.as_mut_ptr(), msg.len()) == 0 {
            write_c(msg.as_ptr());
        }
        if let Some((_, shown)) = self.candidates.get(idx) {
            write_b(b": ");
            write_c(shown.as_ptr());
        }
        write_b(b"\r\n");
    }
}

/// Signed decimal into a stack buffer (the child cannot allocate).
fn itoa(mut v: i32, buf: &mut [u8; 24]) -> &[u8] {
    let negative = v < 0;
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v != 0 {
        i -= 1;
        buf[i] = b'0' + u8::try_from((v % 10).abs()).unwrap_or(0);
        v /= 10;
    }
    if negative {
        i -= 1;
        buf[i] = b'-';
    }
    &buf[i..]
}

/// python `_detect_shell` — `$SHELL` if it is a file, else the first of
/// zsh/bash/sh, else `/bin/sh`. No `-l`: python passes the shell alone.
fn detect_shell() -> String {
    if let Ok(sh) = std::env::var("SHELL") {
        if !sh.is_empty() && std::path::Path::new(&sh).is_file() {
            return sh;
        }
    }
    for cand in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        if std::path::Path::new(cand).is_file() {
            return cand.to_string();
        }
    }
    "/bin/sh".to_string()
}

/// python `tempfile.mkdtemp(prefix=…)` — a 0700 dir in the system temp dir.
fn mkdtemp(prefix: &str) -> Result<PathBuf, i32> {
    let template = std::env::temp_dir().join(format!("{prefix}XXXXXXXX"));
    let mut bytes = template.to_string_lossy().into_owned().into_bytes();
    bytes.push(0);
    let ptr = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if ptr.is_null() {
        return Err(errno());
    }
    let len = bytes.len() - 1;
    Ok(PathBuf::from(
        String::from_utf8_lossy(&bytes[..len]).to_string(),
    ))
}

/// python `secrets.token_hex(n)` — 2n hex chars.
fn token_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    // SAFETY: `buf` is a valid writable buffer of `n` bytes.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr().cast(), n) };
    if rc != 0 {
        // Never a security primitive here — the file name only has to be unique.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
            ^ std::process::id();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = u8::try_from((seed >> (i % 4 * 8)) & 0xff).unwrap_or(0);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// python `kernel.get(agent_id) or {}`.
fn record(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
        return Map::new();
    };
    let meta = agent
        .meta
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    meta.clone()
}

#[cfg(test)]
mod tests;
