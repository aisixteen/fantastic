//! lua_runtime — a general-purpose IN-PROCESS Lua executor bundle. NOT
//! agentic: this bundle knows nothing about AI, loops, or control flow. It
//! gives caller-authored Lua code a persistent, addressable, bootable home:
//!
//! - an agent record whose `code` meta IS the program (persists like any
//!   record → survives reboot; `boot` re-starts it),
//! - a **kernel connector** as the script's ONLY capability —
//!   `kernel.send(target, payload)` (async, awaits the reply),
//!   `kernel.emit(payload)`, `kernel.on_message()` (awaits the next inbox
//!   item), `kernel.self_id`. No `io`, no `os` — sandbox by omission.
//! - an **addressable inbox**: any verb not in this bundle's own table is
//!   queued for the script (`{queued:true}` ack) and delivered via
//!   `on_message()` — so an authored agent receives turns like any agent.
//!
//! What the code does — a control harness, a tool, a state machine, event
//! wiring — is entirely the author's. The engine is vendored (lua5.4,
//! ~300KB); scripts run on the kernel's own tokio runtime; `send` suspends
//! the script until the reply arrives.
//!
//! A `_call_stack` guard rides every connector `send` (own id appended;
//! refuse if already present or depth > 8) so authored call chains can't
//! recurse into themselves.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::call_guard::guard_call_stack;
use fantastic_kernel::{AgentId, Kernel};
use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "lua_runtime.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

// ── per-agent job state ─────────────────────────────────────────────

struct JobState {
    task: JoinHandle<()>,
    inbox_tx: mpsc::UnboundedSender<Value>,
}

struct OnceLockJobs(OnceLock<Mutex<HashMap<AgentId, JobState>>>);
impl OnceLockJobs {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, JobState>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("JOBS poisoned")
    }
}

static JOBS: OnceLockJobs = OnceLockJobs::new();

fn running(agent_id: &AgentId) -> bool {
    JOBS.lock()
        .get(agent_id)
        .map(|j| !j.task.is_finished())
        .unwrap_or(false)
}

fn stop_job(agent_id: &AgentId) -> bool {
    if let Some(job) = JOBS.lock().remove(agent_id) {
        let was_live = !job.task.is_finished();
        job.task.abort();
        was_live
    } else {
        false
    }
}

// ── the script task ─────────────────────────────────────────────────

/// Spawn `code` as this agent's living script: build a Lua VM, inject the
/// kernel connector, run the chunk to completion (or abort). Errors and
/// completion are emitted on the kernel as `job_done`.
fn spawn_script(agent_id: &AgentId, code: String, kernel: &Arc<Kernel>) {
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel::<Value>();
    let inbox_rx = Arc::new(tokio::sync::Mutex::new(inbox_rx));
    let self_id = agent_id.clone();
    let kernel = Arc::clone(kernel);

    // Lock the registry FIRST, then abort → spawn → insert in ONE critical section.
    // Spawning before locking left a window in which the map still held the OUTGOING
    // script's `inbox_tx` while the delivery path (which also locks JOBS) handed an
    // inbound `send` to a script about to be aborted — the message vanished. Mirrors
    // the js_runtime fix.
    let mut jobs = JOBS.lock();
    if let Some(prev) = jobs.remove(agent_id) {
        prev.task.abort(); // one living script per agent — restart semantics
    }
    let task = tokio::spawn(async move {
        let outcome = run_script(&self_id, &code, &kernel, inbox_rx).await;
        let ev = match outcome {
            Ok(()) => json!({"type": "job_done", "source": self_id.as_str(), "ok": true}),
            Err(e) => {
                json!({"type": "job_done", "source": self_id.as_str(), "ok": false, "error": e})
            }
        };
        kernel.emit(&self_id, ev).await;
    });
    jobs.insert(agent_id.clone(), JobState { task, inbox_tx });
}

async fn run_script(
    self_id: &AgentId,
    code: &str,
    kernel: &Arc<Kernel>,
    inbox_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Value>>>,
) -> Result<(), String> {
    // "Sandbox by omission" is only true if we actually omit. `Lua::new()` loads
    // ALL_SAFE, which still includes `os`/`io`/`package` — so `os.execute`,
    // `io.open`, `io.popen`, `require`, `os.remove` were all live, contradicting
    // the "only capability is the kernel connector" contract. Load only the pure
    // computational libraries; the script reaches the outside world solely through
    // the injected `kernel` connector.
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE,
        LuaOptions::default(),
    )
    .map_err(|e| e.to_string())?;
    let globals = lua.globals();
    let kernel_tbl = lua.create_table().map_err(|e| e.to_string())?;

    // kernel.self_id
    kernel_tbl
        .set("self_id", self_id.as_str())
        .map_err(|e| e.to_string())?;

    // kernel.send(target, payload) -> reply (suspends the script)
    {
        let kernel = Arc::clone(kernel);
        let self_id = self_id.clone();
        let send_fn = lua
            .create_async_function(move |lua, (target, payload): (String, mlua::Value)| {
                let kernel = Arc::clone(&kernel);
                let self_id = self_id.clone();
                async move {
                    let mut payload: Value = lua.from_value(payload)?;
                    if let Err(e) = guard_call_stack(self_id.as_str(), &mut payload) {
                        return lua.to_value(&json!({"error": e}));
                    }
                    let reply = kernel.send(&AgentId::from(target.as_str()), payload).await;
                    lua.to_value(&reply)
                }
            })
            .map_err(|e| e.to_string())?;
        kernel_tbl.set("send", send_fn).map_err(|e| e.to_string())?;
    }

    // kernel.emit(payload)
    {
        let kernel = Arc::clone(kernel);
        let self_id = self_id.clone();
        let emit_fn = lua
            .create_async_function(move |lua, payload: mlua::Value| {
                let kernel = Arc::clone(&kernel);
                let self_id = self_id.clone();
                async move {
                    let payload: Value = lua.from_value(payload)?;
                    kernel.emit(&self_id, payload).await;
                    Ok(())
                }
            })
            .map_err(|e| e.to_string())?;
        kernel_tbl.set("emit", emit_fn).map_err(|e| e.to_string())?;
    }

    // kernel.on_message() -> payload (suspends until the next inbox item)
    {
        let on_msg_fn = lua
            .create_async_function(move |lua, ()| {
                let inbox_rx = Arc::clone(&inbox_rx);
                async move {
                    let msg = inbox_rx.lock().await.recv().await;
                    match msg {
                        Some(v) => lua.to_value(&v),
                        // Channel closed (agent deleted) — the script sees nil
                        // and should return.
                        None => Ok(mlua::Value::Nil),
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        kernel_tbl
            .set("on_message", on_msg_fn)
            .map_err(|e| e.to_string())?;
    }

    // kernel.json_decode(str) -> table | nil, kernel.json_encode(any) -> str
    // (Lua ships no JSON; completions and payload-strings need one.)
    {
        let decode_fn = lua
            .create_function(|lua, s: String| match serde_json::from_str::<Value>(&s) {
                Ok(v) => lua.to_value(&v),
                Err(_) => Ok(mlua::Value::Nil),
            })
            .map_err(|e| e.to_string())?;
        kernel_tbl
            .set("json_decode", decode_fn)
            .map_err(|e| e.to_string())?;
        let encode_fn = lua
            .create_function(|lua, v: mlua::Value| {
                let v: Value = lua.from_value(v)?;
                Ok(v.to_string())
            })
            .map_err(|e| e.to_string())?;
        kernel_tbl
            .set("json_encode", encode_fn)
            .map_err(|e| e.to_string())?;
    }

    globals
        .set("kernel", kernel_tbl)
        .map_err(|e| e.to_string())?;

    lua.load(code)
        .set_name(format!("agent:{self_id}"))
        .exec_async()
        .await
        .map_err(|e| e.to_string())
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The Lua runtime bundle.
pub struct LuaRuntimeBundle;

#[async_trait]
impl Bundle for LuaRuntimeBundle {
    fn name(&self) -> &str {
        "lua_runtime"
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
            "boot" => {
                // Rehydration: a persisted `code` meta re-becomes the living
                // script on every boot (idempotent — skip if already running).
                let code = meta_code(agent_id, kernel);
                match code {
                    Some(code) if !running(agent_id) => {
                        spawn_script(agent_id, code, kernel);
                        json!({"started": true})
                    }
                    _ => Value::Null,
                }
            }
            "start" => {
                // Explicit (re)start; `code` arg wins over the record meta.
                // Passing `code` here runs it EPHEMERALLY — put `code` on the
                // record (create_agent/update_agent) to make it durable.
                let code = payload
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| meta_code(agent_id, kernel));
                match code {
                    Some(code) => {
                        spawn_script(agent_id, code, kernel);
                        json!({"started": true, "running": true})
                    }
                    None => {
                        json!({"error": "start: no `code` — pass it or set it on the agent record"})
                    }
                }
            }
            "stop" => json!({"stopped": stop_job(agent_id)}),
            "status" => json!({
                "running": running(agent_id),
                "has_code": meta_code(agent_id, kernel).is_some(),
            }),
            "shutdown" => {
                stop_job(agent_id);
                Value::Null
            }
            // EVERYTHING else is the script's mail: queued for on_message().
            _ => {
                let delivered = JOBS
                    .lock()
                    .get(agent_id)
                    .map(|j| j.inbox_tx.send(payload.clone()).is_ok())
                    .unwrap_or(false);
                if delivered {
                    json!({"queued": true})
                } else {
                    json!({"error": format!(
                        "{}: no running script to receive {verb:?} — start it first (boot/start)",
                        agent_id.as_str()
                    )})
                }
            }
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        stop_job(agent_id);
        Ok(())
    }
}

fn meta_code(agent_id: &AgentId, kernel: &Kernel) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get("code").and_then(Value::as_str).map(str::to_string)
}

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    json!({
        "id": agent_id.as_str(),
        "sentence": "General-purpose Lua executor — runs an authored script as a living agent (kernel connector is its only capability).",
        "running": running(agent_id),
        "has_code": meta_code(agent_id, kernel).is_some(),
        "verbs": {
            "reflect": "Identity + running + has_code. No args.",
            "boot": "If the record has `code` meta and no script is running, start it (rehydration). Returns {started:true} or null.",
            "start": "args: code:str?. (Re)start the script — `code` arg runs EPHEMERALLY; set `code` on the record (create_agent/update_agent) to make it durable across reboots. Returns {started, running}.",
            "stop": "No args. Abort the running script. Returns {stopped:bool}.",
            "status": "No args. Returns {running, has_code}.",
            "shutdown": "Abort + drop process state. Returns null.",
            "<anything else>": "Queued to the SCRIPT's inbox, consumed by kernel.on_message() — this agent is addressable like any other; reply is {queued:true}. Real output rides emits.",
        },
        "script_api": {
            "kernel.send(target, payload)": "Dispatch to any agent by id; suspends the script until the reply table returns. A _call_stack guard refuses cycles/depth>8.",
            "kernel.emit(payload)": "Emit an event on this agent (watchers + tagged client_id inboxes receive it).",
            "kernel.on_message()": "A PULL you LOOP, not a callback — takes no args, suspends until the next queued inbox payload arrives (nil when the agent is deleted). Call it in `while true do local m = kernel.on_message(); if m == nil then break end ... end`; never pass a function.",
            "kernel.self_id": "This agent's id (string).",
            "kernel.json_decode(str) / kernel.json_encode(v)": "JSON <-> Lua table (decode returns nil on invalid JSON).",
        },
        "emits": {
            "job_done": "{type:'job_done', source, ok:bool, error?} — the script ran to completion (or crashed). A harness-style script normally never completes.",
            "<script-defined>": "Anything the script kernel.emit()s — its own contract.",
        },
        "meta": "One meta-possibility to note: a script can create_agent OTHER agents — including more script agents — via kernel.send('kernel', {type:'create_agent', ...}). Tools and harnesses compose.",
    })
}

#[cfg(test)]
mod tests;
