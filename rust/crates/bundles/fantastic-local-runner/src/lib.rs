//! Local `fantastic` lifecycle as an agent.
//!
//! Each agent represents one project on this machine. Verbs spawn /
//! signal a `fantastic` subprocess directly (no SSH, no tunnels) and
//! observe its live status via two sibling files in the project's
//! `.fantastic/` dir:
//!
//! - `lock.json` — `{pid:int}`, PID-only (substrate's lock).
//! - `agents/web_*/agent.json` — the web bundle's persisted record,
//!   which carries the port.
//!
//! ## Record fields
//!
//! | key            | purpose                                                                  |
//! |----------------|--------------------------------------------------------------------------|
//! | `remote_path`  | project root (absolute filesystem path)                                  |
//! | `remote_cmd`   | `fantastic` CLI to invoke (default: lookup via `FANTASTIC_BIN` / PATH)   |
//! | `entry_path`   | URL suffix appended to the live serve URL for `get_webapp`               |
//!
//! ## Verbs
//!
//! `reflect`, `boot`, `start`, `stop`, `restart`, `status`,
//! `get_webapp`. See the readme for the exact reply shapes.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle as _;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "local_runner.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// How long to wait for `.fantastic/lock.json` to appear after `start`.
pub const LOCK_POLL_TIMEOUT_SECS: f64 = 30.0;
/// Polling cadence while waiting for `lock.json`.
pub const LOCK_POLL_INTERVAL_MS: u64 = 500;
/// How long to wait after SIGTERM before escalating to SIGKILL.
pub const STOP_POLL_TIMEOUT_SECS: f64 = 6.0;
/// Polling cadence while waiting for the child to die.
pub const STOP_POLL_INTERVAL_MS: u64 = 100;

/// Per-agent live-state cache. Tracks the most-recently-spawned child
/// so tests + `on_delete` can observe it without filesystem races.
///
/// In production the source of truth is `lock.json` on disk; this
/// cache is a warm shortcut that mirrors what `start` just spawned
/// and gets cleared on `stop` / `on_delete`.
static RUNNERS: OnceLockRunnerMap = OnceLockRunnerMap::new();

// ── once-lock map helper ────────────────────────────────────────────

/// Per-agent runner state. Each agent's child handle lives behind a
/// `tokio::sync::Mutex` so `start` / `stop` serialize their lifecycle
/// transitions.
pub struct RunnerState {
    /// The most recent `tokio::process::Child` (None if we never
    /// spawned, or last spawn already reaped).
    pub child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

impl RunnerState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            child: tokio::sync::Mutex::new(None),
        })
    }
}

struct OnceLockRunnerMap(OnceLock<Mutex<HashMap<AgentId, Arc<RunnerState>>>>);
impl OnceLockRunnerMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn get_or_init_for(&self, id: &AgentId) -> Arc<RunnerState> {
        let mut map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("RUNNERS outer mutex poisoned");
        if let Some(existing) = map.get(id) {
            return Arc::clone(existing);
        }
        let arc = RunnerState::new();
        map.insert(id.clone(), Arc::clone(&arc));
        arc
    }
    fn remove(&self, id: &AgentId) {
        let mut map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("RUNNERS outer mutex poisoned");
        map.remove(id);
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The local_runner bundle.
pub struct LocalRunnerBundle;

#[async_trait]
impl Bundle for LocalRunnerBundle {
    fn name(&self) -> &str {
        "local_runner"
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
            "boot" => Value::Null,
            "start" => start_reply(agent_id, kernel).await,
            "stop" | "shutdown" => stop_reply(agent_id, kernel).await,
            "restart" => {
                let _ = stop_reply(agent_id, kernel).await;
                start_reply(agent_id, kernel).await
            }
            "status" => status_reply(agent_id, kernel),
            "get_webapp" => get_webapp_reply(agent_id, kernel),
            other => json!({"error": format!("local_runner: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = stop_reply(agent_id, kernel).await;
        RUNNERS.remove(agent_id);
        Ok(())
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn snapshot_meta(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a.meta.read().expect("meta poisoned").clone(),
        None => Map::new(),
    }
}

fn meta_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

// ── binary resolution ───────────────────────────────────────────────

/// Resolve the path to the `fantastic` CLI binary.
///
/// Ladder:
/// 1. `record.remote_cmd` (the Python bundle's preferred field)
/// 2. `record.fantastic_path` (legacy alias)
/// 3. `FANTASTIC_BIN` env var
/// 4. `which fantastic`
/// 5. Error
pub fn resolve_fantastic_bin(meta: &Map<String, Value>) -> Result<PathBuf, String> {
    if let Some(c) = meta_str(meta, "remote_cmd") {
        if !c.is_empty() {
            // If absolute or contains a slash, use literally; otherwise
            // try `which` so PATH-only names like "fantastic" resolve.
            if c.contains('/') || c.contains('\\') {
                return Ok(PathBuf::from(c));
            }
            if let Ok(p) = which::which(c) {
                return Ok(p);
            }
            return Ok(PathBuf::from(c)); // fall through; spawn will error if missing
        }
    }
    if let Some(c) = meta_str(meta, "fantastic_path") {
        if !c.is_empty() {
            return Ok(PathBuf::from(c));
        }
    }
    if let Ok(p) = std::env::var("FANTASTIC_BIN") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Ok(p) = which::which("fantastic") {
        return Ok(p);
    }
    Err(
        "local_runner: no `fantastic` binary resolved; set record.remote_cmd or FANTASTIC_BIN"
            .to_string(),
    )
}

// ── lock + port discovery ───────────────────────────────────────────

fn read_lock(remote_path: &Path) -> Option<Value> {
    let p = remote_path.join(".fantastic").join("lock.json");
    if !p.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid), None).is_ok()
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    false
}

fn discover_web_port(remote_path: &Path) -> Option<u16> {
    let agents_dir = remote_path.join(".fantastic").join("agents");
    if !agents_dir.is_dir() {
        return None;
    }
    let mut entries: Vec<_> = std::fs::read_dir(&agents_dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let af = entry.path().join("agent.json");
        if !af.exists() {
            continue;
        }
        let raw = match std::fs::read_to_string(&af) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rec: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if rec.get("handler_module").and_then(Value::as_str) == Some("web.tools") {
            if let Some(p) = rec.get("port").and_then(Value::as_u64) {
                if p > 0 && p <= u16::MAX as u64 {
                    return Some(p as u16);
                }
            }
        }
    }
    None
}

fn live_pid_port(remote_path: &Path) -> (Option<i32>, Option<u16>) {
    let Some(lock) = read_lock(remote_path) else {
        return (None, None);
    };
    let Some(pid) = lock.get("pid").and_then(Value::as_i64) else {
        return (None, None);
    };
    let pid = pid as i32;
    if !pid_alive(pid) {
        return (None, None);
    }
    (Some(pid), discover_web_port(remote_path))
}

fn has_web_record(proj: &Path) -> bool {
    let agents_dir = proj.join(".fantastic").join("agents");
    if !agents_dir.is_dir() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let af = entry.path().join("agent.json");
        if !af.exists() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&af) else {
            continue;
        };
        let Ok(rec): Result<Value, _> = serde_json::from_str(&raw) else {
            continue;
        };
        if rec.get("handler_module").and_then(Value::as_str) == Some("web.tools") {
            return true;
        }
    }
    false
}

/// Pick a free TCP port by binding to `127.0.0.1:0`. Loopback-only to
/// match the Python bundle and avoid CodeQL's
/// `py/bind-socket-all-network-interfaces`.
fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let rp = meta_str(&meta, "remote_path").map(PathBuf::from);
    let (pid, port) = match rp.as_ref() {
        Some(p) => live_pid_port(p),
        None => (None, None),
    };
    json!({
        "id": agent_id.as_str(),
        "sentence": "Local `fantastic --port N` lifecycle (subprocess + lock.json).",
        "remote_path": meta.get("remote_path").cloned().unwrap_or(Value::Null),
        "remote_cmd": meta_str(&meta, "remote_cmd").unwrap_or("fantastic"),
        "entry_path": meta_str(&meta, "entry_path").unwrap_or(""),
        "running": pid.is_some(),
        "pid": pid,
        "port": port,
        "verbs": {
            "reflect": "Identity + every record field + live status. No args.",
            "boot": "No-op. local_runner does NOT auto-start the project — `start` is explicit.",
            "start": "No args. Picks a free port, ensures a `web` agent record, spawns `<remote_cmd>` in `<remote_path>`. Polls lock.json (~30s).",
            "stop": "No args. SIGTERM the pid recorded in lock.json (SIGKILL after 6s), remove stale lock.",
            "restart": "No args. stop + start.",
            "status": "No args. {running, pid, port}.",
            "get_webapp": "No args. Canvas-facing UI descriptor {url, default_width, default_height, title} when alive.",
        },
    })
}

async fn start_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let Some(rp_str) = meta_str(&meta, "remote_path") else {
        return json!({"error": "local_runner.start: remote_path required"});
    };
    let proj = PathBuf::from(rp_str);
    if !proj.is_dir() {
        return json!({"error": format!("local_runner.start: not a directory: {}", proj.display())});
    }

    // Already running?
    let (pid, port) = live_pid_port(&proj);
    if pid.is_some() {
        return json!({
            "started": true,
            "pid": pid,
            "port": port,
            "already_running": true,
        });
    }

    let bin = match resolve_fantastic_bin(&meta) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };

    let port = match free_port() {
        Ok(p) => p,
        Err(e) => return json!({"error": format!("local_runner.start: free_port: {e}")}),
    };
    let fant_dir = proj.join(".fantastic");
    if let Err(e) = std::fs::create_dir_all(&fant_dir) {
        return json!({"error": format!("local_runner.start: mkdir: {e}")});
    }
    let log_path = fant_dir.join("serve.log");

    // Step 1: pre-create the web agent record. Subprocess (not via the
    // Rust kernel) because the child must persist the record in its
    // own workdir; we then spawn the daemon which rehydrates.
    if !has_web_record(&proj) {
        let _ = std::process::Command::new(&bin)
            .args([
                "core",
                "create_agent",
                "handler_module=web.tools",
                &format!("port={port}"),
            ])
            .current_dir(&proj)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // Step 2: spawn the daemon. We use tokio::process::Command so
    // RUNNERS can hold the Child for tests + on_delete cleanup.
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => return json!({"error": format!("local_runner.start: open log: {e}")}),
    };
    let log_err = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return json!({"error": format!("local_runner.start: clone log: {e}")}),
    };

    let runner = RUNNERS.get_or_init_for(agent_id);
    let mut child_slot = runner.child.lock().await;

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.current_dir(&proj)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(false);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("local_runner.start: spawn: {e}")}),
    };
    *child_slot = Some(child);
    drop(child_slot);

    // Poll lock.json (matches Python deadline math).
    let deadline = Instant::now() + Duration::from_secs_f64(LOCK_POLL_TIMEOUT_SECS);
    while Instant::now() < deadline {
        if let Some(lock) = read_lock(&proj) {
            if let Some(pid) = lock.get("pid").and_then(Value::as_i64) {
                if let Some(p) = discover_web_port(&proj) {
                    return json!({"started": true, "pid": pid, "port": p});
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(LOCK_POLL_INTERVAL_MS)).await;
    }
    json!({
        "error": "local_runner.start: lock.json never appeared",
        "requested_port": port,
    })
}

async fn stop_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let Some(rp_str) = meta_str(&meta, "remote_path") else {
        return json!({"error": "local_runner.stop: remote_path required"});
    };
    let proj = PathBuf::from(rp_str);
    let lock_path = proj.join(".fantastic").join("lock.json");

    // Resolve target pid from lock.json (truth) or from our cached
    // Child handle (best-effort for ephemeral test spawns).
    let lock_pid: Option<i32> = read_lock(&proj)
        .and_then(|v| v.get("pid").and_then(Value::as_i64))
        .map(|n| n as i32);
    let cache_pid: Option<i32> = {
        let runner = RUNNERS.get_or_init_for(agent_id);
        let child_slot = runner.child.lock().await;
        child_slot.as_ref().and_then(|c| c.id().map(|p| p as i32))
    };
    let pid_to_kill = lock_pid.or(cache_pid);

    let Some(pid) = pid_to_kill else {
        // Nothing to stop; sweep stale lock.
        let _ = std::fs::remove_file(&lock_path);
        return json!({"stopped": true, "pid": Value::Null});
    };

    #[cfg(unix)]
    let term_sent = {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid), Signal::SIGTERM).is_ok()
    };
    #[cfg(not(unix))]
    let term_sent = false;

    if !term_sent {
        let _ = std::fs::remove_file(&lock_path);
        return json!({
            "stopped": true,
            "pid": pid,
            "already_gone": true,
        });
    }

    let deadline = Instant::now() + Duration::from_secs_f64(STOP_POLL_TIMEOUT_SECS);
    let mut died = false;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            died = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(STOP_POLL_INTERVAL_MS)).await;
    }
    if !died {
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let _ = std::fs::remove_file(&lock_path);
    // Drop the cached Child handle so the next start spawns fresh.
    {
        let runner = RUNNERS.get_or_init_for(agent_id);
        let mut slot = runner.child.lock().await;
        if let Some(mut c) = slot.take() {
            // Best-effort reap so tokio doesn't keep a zombie.
            let _ = c.wait().await;
        }
    }

    json!({
        "stopped": true,
        "pid": pid,
        "died_cleanly": died,
    })
}

fn status_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let (pid, port) = match meta_str(&meta, "remote_path") {
        Some(rp) => live_pid_port(Path::new(rp)),
        None => (None, None),
    };
    json!({
        "running": pid.is_some(),
        "pid": pid,
        "port": port,
    })
}

fn get_webapp_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let Some(rp_str) = meta_str(&meta, "remote_path") else {
        return json!({"error": "local_runner.get_webapp: remote_path required"});
    };
    let proj = PathBuf::from(rp_str);
    let (pid, port) = live_pid_port(&proj);
    let Some(_pid) = pid else {
        return json!({"error": "local_runner.get_webapp: not running"});
    };
    let Some(port) = port else {
        return json!({"error": "local_runner.get_webapp: port unknown"});
    };
    let entry = meta_str(&meta, "entry_path").unwrap_or("");
    let title = meta_str(&meta, "display_name")
        .map(str::to_string)
        .or_else(|| proj.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| agent_id.as_str().to_string());
    json!({
        "url": format!("http://localhost:{port}/{entry}"),
        "default_width": 800,
        "default_height": 600,
        "title": title,
    })
}

#[cfg(test)]
mod tests;
