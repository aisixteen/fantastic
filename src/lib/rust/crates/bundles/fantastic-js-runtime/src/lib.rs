//! js_runtime — a general-purpose IN-PROCESS JavaScript executor bundle
//! (QuickJS via rquickjs, bundled — no system deps). NOT agentic: this
//! bundle knows nothing about AI, loops, or control flow. It gives
//! caller-authored JS a persistent, addressable, bootable home — the same
//! contract as `lua_runtime`, on a JS engine:
//!
//! - the agent record's `code` meta IS the program (persists → survives
//!   reboot; `boot` re-starts it),
//! - the script's ONLY capability is the **kernel connector**:
//!   `await kernel.send(target, payload)` → reply,
//!   `await kernel.emit(payload)`, `await kernel.on_message()` → next inbox
//!   payload (or `null` when the agent dies), `kernel.self_id`. No fs, no
//!   net, no process — sandbox by omission.
//! - any verb this bundle doesn't own is queued for the script
//!   (`{queued:true}` ack) and delivered via `on_message()`.
//!
//! Authored code runs inside an async IIFE (so top-level `await` works).
//! The Rust↔JS bridge crosses as JSON strings; a tiny prelude re-exposes it
//! as plain objects. A `_call_stack` guard rides every connector `send`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::call_guard::guard_call_stack;
use fantastic_kernel::{AgentId, Kernel};
use rquickjs::function::Async;
use rquickjs::{async_with, AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "js_runtime.tools";

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

/// The JS-side prelude: re-expose the string bridge as a plain-object API.
const PRELUDE: &str = r#"
globalThis.kernel = {
    self_id: __self_id,
    send: async (target, payload) =>
        JSON.parse(await __send(target, JSON.stringify(payload))),
    emit: async (payload) => { await __emit(JSON.stringify(payload)); },
    on_message: async () => {
        const s = await __on_message();
        return s === null || s === undefined ? null : JSON.parse(s);
    },
};
"#;

fn spawn_script(agent_id: &AgentId, code: String, kernel: &Arc<Kernel>) {
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel::<Value>();
    let inbox_rx = Arc::new(tokio::sync::Mutex::new(inbox_rx));
    let self_id = agent_id.clone();
    let kernel = Arc::clone(kernel);

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

    let mut jobs = JOBS.lock();
    if let Some(prev) = jobs.remove(agent_id) {
        prev.task.abort(); // one living script per agent — restart semantics
    }
    jobs.insert(agent_id.clone(), JobState { task, inbox_tx });
}

async fn run_script(
    self_id: &AgentId,
    code: &str,
    kernel: &Arc<Kernel>,
    inbox_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Value>>>,
) -> Result<(), String> {
    let rt = AsyncRuntime::new().map_err(|e| e.to_string())?;
    let ctx = AsyncContext::full(&rt).await.map_err(|e| e.to_string())?;

    // Authored code runs in an async IIFE so top-level `await` works.
    let wrapped = format!("(async () => {{\n{PRELUDE}\n{code}\n}})()");
    let self_str = self_id.as_str().to_string();
    let kernel_send = Arc::clone(kernel);
    let self_for_send = self_id.clone();
    let kernel_emit = Arc::clone(kernel);
    let self_for_emit = self_id.clone();

    async_with!(ctx => |ctx| {
        let globals = ctx.globals();
        globals
            .set("__self_id", self_str.as_str())
            .map_err(|e| e.to_string())?;

        // __send(target, payload_json) -> Promise<reply_json>
        let send_fn = Function::new(
            ctx.clone(),
            Async(move |target: String, payload_json: String| {
                let kernel = Arc::clone(&kernel_send);
                let self_id = self_for_send.clone();
                async move {
                    let mut payload: Value =
                        serde_json::from_str(&payload_json).unwrap_or(Value::Null);
                    if let Err(e) = guard_call_stack(self_id.as_str(), &mut payload) {
                        return json!({"error": e}).to_string();
                    }
                    let reply = kernel.send(&AgentId::from(target.as_str()), payload).await;
                    reply.to_string()
                }
            }),
        )
        .map_err(|e| e.to_string())?;
        globals.set("__send", send_fn).map_err(|e| e.to_string())?;

        // __emit(payload_json) -> Promise<void>
        let emit_fn = Function::new(
            ctx.clone(),
            Async(move |payload_json: String| {
                let kernel = Arc::clone(&kernel_emit);
                let self_id = self_for_emit.clone();
                async move {
                    let payload: Value =
                        serde_json::from_str(&payload_json).unwrap_or(Value::Null);
                    kernel.emit(&self_id, payload).await;
                }
            }),
        )
        .map_err(|e| e.to_string())?;
        globals.set("__emit", emit_fn).map_err(|e| e.to_string())?;

        // __on_message() -> Promise<payload_json | null>
        let on_msg_fn = Function::new(
            ctx.clone(),
            Async(move || {
                let inbox_rx = Arc::clone(&inbox_rx);
                async move {
                    let msg = inbox_rx.lock().await.recv().await;
                    msg.map(|v| v.to_string())
                }
            }),
        )
        .map_err(|e| e.to_string())?;
        globals
            .set("__on_message", on_msg_fn)
            .map_err(|e| e.to_string())?;

        // Evaluate: the IIFE yields a promise; awaiting it drives the script.
        let promise: Promise = ctx
            .eval(wrapped.as_bytes())
            .catch(&ctx)
            .map_err(|e| e.to_string())?;
        promise
            .into_future::<()>()
            .await
            .catch(&ctx)
            .map_err(|e| e.to_string())
    })
    .await
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The JS runtime bundle.
pub struct JsRuntimeBundle;

#[async_trait]
impl Bundle for JsRuntimeBundle {
    fn name(&self) -> &str {
        "js_runtime"
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
        "sentence": "General-purpose JavaScript executor — runs an authored script as a living agent (kernel connector is its only capability).",
        "running": running(agent_id),
        "has_code": meta_code(agent_id, kernel).is_some(),
        "verbs": {
            "reflect": "Identity + running + has_code. No args.",
            "boot": "If the record has `code` meta and no script is running, start it (rehydration). Returns {started:true} or null.",
            "start": "args: code:str?. (Re)start the script — `code` arg runs EPHEMERALLY; set `code` on the record (create_agent/update_agent) to make it durable across reboots. Returns {started, running}.",
            "stop": "No args. Abort the running script. Returns {stopped:bool}.",
            "status": "No args. Returns {running, has_code}.",
            "shutdown": "Abort + drop process state. Returns null.",
            "<anything else>": "Queued to the SCRIPT's inbox, consumed by await kernel.on_message() — this agent is addressable like any other; reply is {queued:true}. Real output rides emits.",
        },
        "script_api": {
            "await kernel.send(target, payload)": "Dispatch to any agent by id; resolves with the reply object. A _call_stack guard refuses cycles/depth>8.",
            "await kernel.emit(payload)": "Emit an event on this agent (watchers + tagged client_id inboxes receive it).",
            "await kernel.on_message()": "Resolves with the next queued inbox payload (null when the agent is deleted).",
            "kernel.self_id": "This agent's id (string).",
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
