//! Subprocess Python exec — `python -c <code>` per call.
//!
//! Each `exec` spawns a fresh `tokio::process::Command` with stdout +
//! stderr piped, waits up to `timeout` seconds, returns the captured
//! buffers. Per-agent in-flight tracking (`OnceLockMap<AgentId,
//! Mutex<HashSet<u32>>>` keyed by PID) enables `interrupt` (SIGINT) and
//! `stop` (SIGKILL).
//!
//! ## Interpreter resolution ladder
//!
//! The Rust kernel has no `sys.executable` equivalent, so this bundle
//! extends the Python bundle's ladder with two POSIX fallbacks:
//!
//! 1. `payload.python` — explicit interpreter path
//! 2. `payload.venv`   — `<venv>/bin/python` (or `bin/python3` / `Scripts/python.exe`)
//! 3. `record.python`
//! 4. `record.venv`    — same venv-dir lookup as #2
//! 5. `FANTASTIC_PYTHON` env var
//! 6. `which python3`
//! 7. `which python`
//! 8. Error
//!
//! The Python bundle's `_boot` persists `sys.executable` into
//! `record.python` automatically; opening the same workdir under the
//! Rust kernel then hits step 3 without falling through to PATH.
//!
//! ## Verbs
//!
//! | verb       | args                                                          | reply                                          |
//! |------------|---------------------------------------------------------------|------------------------------------------------|
//! | `reflect`  | _none_                                                        | `{id, sentence, cwd, python, venv, in_flight, verbs}` |
//! | `exec`     | `code:str`, `timeout:float?` (60), `cwd?`, `python?`, `venv?` | `{stdout, stderr, exit_code, timed_out}`       |
//! | `interrupt`| _none_                                                        | `{interrupted: int}` (SIGINT to in-flight)     |
//! | `stop`     | _none_                                                        | `{killed: int}` (SIGKILL to in-flight)         |
//! | `boot`     | _none_                                                        | `null` (no-op)                                 |

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "python_runtime.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default exec timeout in seconds when the caller omits `timeout`.
pub const DEFAULT_TIMEOUT_SECS: f64 = 60.0;

/// Live subprocess PIDs keyed by agent id. `exec` inserts; the
/// terminating branch (whether normal exit, timeout-kill, or
/// interrupt/stop) removes. Mirrors Python's `_procs` dict.
static IN_FLIGHT: OnceLockPidSet = OnceLockPidSet::new();

// ── once-lock map helper ────────────────────────────────────────────

type PidSetInner = HashMap<AgentId, Arc<Mutex<HashSet<u32>>>>;

struct OnceLockPidSet(OnceLock<Mutex<PidSetInner>>);
impl OnceLockPidSet {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn get_or_init_for(&self, id: &AgentId) -> Arc<Mutex<HashSet<u32>>> {
        let map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("IN_FLIGHT outer mutex poisoned");
        let mut map = map;
        if let Some(existing) = map.get(id) {
            return Arc::clone(existing);
        }
        let arc = Arc::new(Mutex::new(HashSet::new()));
        map.insert(id.clone(), Arc::clone(&arc));
        arc
    }
    fn get(&self, id: &AgentId) -> Option<Arc<Mutex<HashSet<u32>>>> {
        let map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("IN_FLIGHT outer mutex poisoned");
        map.get(id).cloned()
    }
    fn remove(&self, id: &AgentId) {
        let mut map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("IN_FLIGHT outer mutex poisoned");
        map.remove(id);
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The python_runtime bundle.
pub struct PythonRuntimeBundle;

#[async_trait]
impl Bundle for PythonRuntimeBundle {
    fn name(&self) -> &str {
        "python_runtime"
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
            "boot" | "shutdown" => Value::Null,
            "exec" => exec_reply(agent_id, payload, kernel).await,
            "interrupt" => interrupt_reply(agent_id),
            "stop" => stop_reply(agent_id),
            other => json!({"error": format!("python_runtime: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // Best-effort SIGKILL any survivors so the agent dir can be
        // rmtreed without leaking children.
        let _ = stop_reply(agent_id);
        IN_FLIGHT.remove(agent_id);
        Ok(())
    }
}

// ── snapshot helpers ────────────────────────────────────────────────

/// Snapshot the agent record's meta map. Returns an empty map if the
/// agent has been deleted concurrently.
fn snapshot_meta(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a.meta.read().expect("meta poisoned").clone(),
        None => Map::new(),
    }
}

/// Read a string field, treating empty strings as absent. Callers
/// (`update_agent id=py python=""`) pass `""` to unset — without this
/// filter the resolver picks up the empty path and spawn fails with
/// ENOENT. Discovered by `python_runtime_resolution.md` Tests 4/5/6/8.
fn meta_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

// ── interpreter resolution ──────────────────────────────────────────

/// Resolve the Python interpreter for one exec call.
///
/// Walks the 8-step ladder documented at the crate root. The first
/// branch that produces an existing path wins. Returns the error
/// payload string when every branch falls through.
pub fn resolve_python(meta: &Map<String, Value>, payload: &Value) -> Result<PathBuf, String> {
    // 1. payload.python
    if let Some(p) = payload_str(payload, "python") {
        let path = expanduser(p);
        return Ok(path);
    }
    // 2. payload.venv
    if let Some(v) = payload_str(payload, "venv") {
        if let Some(p) = venv_python(v) {
            return Ok(p);
        }
    }
    // 3. record.python
    if let Some(p) = meta_str(meta, "python") {
        let path = expanduser(p);
        return Ok(path);
    }
    // 4. record.venv
    if let Some(v) = meta_str(meta, "venv") {
        if let Some(p) = venv_python(v) {
            return Ok(p);
        }
    }
    // 5. FANTASTIC_PYTHON env var
    if let Ok(p) = std::env::var("FANTASTIC_PYTHON") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    // 6. which python3
    if let Ok(p) = which::which("python3") {
        return Ok(p);
    }
    // 7. which python
    if let Ok(p) = which::which("python") {
        return Ok(p);
    }
    // 8. Error
    Err(
        "python_runtime: no Python interpreter resolved; set record.python or FANTASTIC_PYTHON"
            .to_string(),
    )
}

/// Expand a leading `~` to `$HOME`. Pure on platforms without HOME.
fn expanduser(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~") {
        if let Ok(home) = std::env::var("HOME") {
            let trimmed = rest.strip_prefix('/').unwrap_or(rest);
            return PathBuf::from(home).join(trimmed);
        }
    }
    PathBuf::from(s)
}

/// Locate a Python interpreter inside a venv-style directory. Returns
/// `None` when nothing usable is found (caller falls through to the
/// next ladder branch).
fn venv_python(venv_path: &str) -> Option<PathBuf> {
    let base = expanduser(venv_path);
    for rel in [
        "bin/python",
        "bin/python3",
        "Scripts/python.exe",
        "Scripts/python",
    ] {
        let cand = base.join(rel);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let cwd_val = meta_str(&meta, "cwd")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<process default>".to_string());
    let python_val = match resolve_python(&meta, &json!({})) {
        Ok(p) => json!(p.to_string_lossy().to_string()),
        Err(e) => json!({"error": e}),
    };
    let venv_val = meta.get("venv").cloned().unwrap_or(Value::Null);
    let in_flight = IN_FLIGHT
        .get(agent_id)
        .map(|arc| arc.lock().expect("pid set poisoned").len())
        .unwrap_or(0);
    json!({
        "id": agent_id.as_str(),
        "sentence": "Python subprocess runner.",
        "cwd": cwd_val,
        "python": python_val,
        "venv": venv_val,
        "in_flight": in_flight,
        "verbs": {
            "reflect": "Identity + cwd + interpreter + count of in-flight subprocesses. No args.",
            "exec": "args: code:str (req), timeout:float? (default 60), cwd:str? (overrides agent cwd), python:str? (interpreter path override), venv:str? (venv-dir override; uses <venv>/bin/python).",
            "interrupt": "No args. Sends SIGINT to all in-flight subprocesses for this agent. Returns {interrupted:int}.",
            "stop": "No args. SIGKILLs all in-flight subprocesses for this agent. Returns {killed:int}.",
            "boot": "No-op. python_runtime is stateless per-call.",
        },
    })
}

async fn exec_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let code = match payload.get("code") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::String(_)) => {
            return json!({"error": "python_runtime: code (str) required"});
        }
        Some(_) => {
            return json!({"error": "python_runtime: code (str) required"});
        }
        None => {
            return json!({"error": "python_runtime: code (str) required"});
        }
    };
    let timeout_secs = payload
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let meta = snapshot_meta(agent_id, kernel);
    let cwd = payload_str(payload, "cwd")
        .or_else(|| meta_str(&meta, "cwd"))
        .map(PathBuf::from);
    let interp = match resolve_python(&meta, payload) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };

    let mut cmd = tokio::process::Command::new(&interp);
    cmd.arg("-c")
        .arg(&code)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("python_runtime: spawn failed: {e}")}),
    };
    let pid = match child.id() {
        Some(p) => p,
        None => {
            // Process already exited — wait + report.
            let output = match child.wait_with_output().await {
                Ok(o) => o,
                Err(e) => return json!({"error": format!("python_runtime: wait failed: {e}")}),
            };
            return json!({
                "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
                "exit_code": output.status.code().unwrap_or(-1),
                "timed_out": false,
            });
        }
    };

    let pid_set = IN_FLIGHT.get_or_init_for(agent_id);
    pid_set.lock().expect("pid set poisoned").insert(pid);

    let (stdout, stderr, exit_code, timed_out) = match tokio::time::timeout(
        Duration::from_secs_f64(timeout_secs.max(0.0)),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => (
            output.stdout,
            output.stderr,
            output.status.code().unwrap_or(-1),
            false,
        ),
        Ok(Err(e)) => {
            pid_set.lock().expect("pid set poisoned").remove(&pid);
            return json!({"error": format!("python_runtime: wait failed: {e}")});
        }
        Err(_) => {
            // Timeout: SIGKILL via PID (we consumed `child`, can't call
            // its `.kill()` here — wait_with_output owns it).
            #[cfg(unix)]
            {
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid;
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            // Best-effort: give the kernel a beat to reap.
            tokio::time::sleep(Duration::from_millis(50)).await;
            (Vec::new(), Vec::new(), -1, true)
        }
    };

    pid_set.lock().expect("pid set poisoned").remove(&pid);

    json!({
        "stdout": String::from_utf8_lossy(&stdout).to_string(),
        "stderr": String::from_utf8_lossy(&stderr).to_string(),
        "exit_code": exit_code,
        "timed_out": timed_out,
    })
}

fn interrupt_reply(agent_id: &AgentId) -> Value {
    let Some(arc) = IN_FLIGHT.get(agent_id) else {
        return json!({"interrupted": 0});
    };
    let pids: Vec<u32> = arc
        .lock()
        .expect("pid set poisoned")
        .iter()
        .copied()
        .collect();
    let mut n: u64 = 0;
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        for pid in pids {
            if kill(Pid::from_raw(pid as i32), Signal::SIGINT).is_ok() {
                n += 1;
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: no SIGINT — best-effort no-op (matches behaviour
        // where the Python bundle would also degrade).
        let _ = pids;
    }
    json!({"interrupted": n})
}

fn stop_reply(agent_id: &AgentId) -> Value {
    let Some(arc) = IN_FLIGHT.get(agent_id) else {
        return json!({"killed": 0});
    };
    let pids: Vec<u32> = arc
        .lock()
        .expect("pid set poisoned")
        .iter()
        .copied()
        .collect();
    let mut n: u64 = 0;
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        for pid in pids {
            if kill(Pid::from_raw(pid as i32), Signal::SIGKILL).is_ok() {
                n += 1;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pids;
    }
    json!({"killed": n})
}

#[cfg(test)]
mod tests;
