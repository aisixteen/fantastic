//! nvidia_nim_backend — NVIDIA NIM (OpenAI-compatible) LLM bundle.
//! Same surface as `ollama_backend` per the LLM backend contract;
//! transport differs (HTTPS + Bearer auth + SSE + per-index tool-call
//! argument aggregation + 429 rate-limit retry).
//!
//! See `fantastic_ollama_backend`'s module header for the full
//! contract spec. Extras specific to this backend:
//!
//! - `set_api_key` args `{api_key:str}` → persists to sidecar via file agent
//! - `clear_api_key` → deletes the sidecar
//! - `reflect` includes `has_api_key: bool` (never the value)
//!
//! ## AI rehaul backlog (TODO — not in scope for the current port)
//!
//! These items will need a coordinated redesign across all LLM
//! backends + ai_chat_webapp before the next major bump:
//!
//! 1. Cross-backend conversation portability — today history lives
//!    in `<backend>/chat_<client>.json`. Switching `upstream_id`
//!    starts a fresh conversation. Future: history travels with the
//!    chat tile, backends become stateless-modulo-streaming.
//! 2. Tool-call streaming protocol — current contract is one
//!    tool_call per chunk (ollama) vs argument fragments aggregated
//!    across chunks (OpenAI/NIM). Pick one and version it.
//! 3. Multi-modal binary frames — image/audio payloads currently
//!    have no defined wire shape. Needs the WS binary frame channel
//!    (also blocks terminal_backend's image-paste).
//! 4. Cost / token tracking — no per-turn cost report today.
//! 5. Context-window management — backends silently truncate; no
//!    surface to inspect or override.
//! 6. Auth — api_key sidecar is plaintext; no per-tenant scoping.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::{AbortHandle, JoinHandle};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "nvidia_nim_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default NIM endpoint — OpenAI-compatible.
pub const DEFAULT_ENDPOINT: &str = "https://integrate.api.nvidia.com/v1";

/// Default model (matches the Python provider).
pub const DEFAULT_MODEL: &str = "nvidia/llama-3_1-nemotron-ultra-253b-v1";

/// Headless / REPL caller default.
pub const DEFAULT_CLIENT_ID: &str = "cli";

/// Hard per-generation ceiling (seconds). Releases the FIFO lock.
pub const SEND_TIMEOUT_SECS: u64 = 180;

/// Max wait honored from a `Retry-After` header on 429.
pub const RATE_LIMIT_MAX_WAIT_SECS: u64 = 60;

/// Default wait when `Retry-After` is absent / unparseable.
pub const RATE_LIMIT_DEFAULT_WAIT_SECS: u64 = 5;

/// Per-agent retry budget on 429 before any chunk has been yielded.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 1;

// ── process-global maps ─────────────────────────────────────────────

static HTTP_CLIENTS: OnceLockHttpMap = OnceLockHttpMap::new();
static IN_FLIGHT_TASKS: OnceLockTaskMap = OnceLockTaskMap::new();
static SEND_LOCKS: OnceLockLockMap = OnceLockLockMap::new();
static MENU_CACHE: OnceLockMenuMap = OnceLockMenuMap::new();
static QUEUE_MAP: OnceLockQueueMap = OnceLockQueueMap::new();
static CURRENT_MAP: OnceLockCurrentMap = OnceLockCurrentMap::new();

struct OnceLockHttpMap(OnceLock<Mutex<HashMap<AgentId, Arc<reqwest::Client>>>>);
impl OnceLockHttpMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Arc<reqwest::Client>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("HTTP_CLIENTS poisoned")
    }
}

struct OnceLockTaskMap(OnceLock<Mutex<HashMap<AgentId, AbortHandle>>>);
impl OnceLockTaskMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, AbortHandle>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("IN_FLIGHT_TASKS poisoned")
    }
}

struct OnceLockLockMap(OnceLock<Mutex<HashMap<AgentId, Arc<AsyncMutex<()>>>>>);
impl OnceLockLockMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn get_or_create(&self, id: &AgentId) -> Arc<AsyncMutex<()>> {
        let mut guard = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("SEND_LOCKS poisoned");
        guard
            .entry(id.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

struct OnceLockMenuMap(OnceLock<Mutex<HashMap<AgentId, Vec<Value>>>>);
impl OnceLockMenuMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Vec<Value>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("MENU_CACHE poisoned")
    }
}

struct OnceLockQueueMap(OnceLock<Mutex<HashMap<AgentId, Vec<Value>>>>);
impl OnceLockQueueMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Vec<Value>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("QUEUE_MAP poisoned")
    }
}

struct OnceLockCurrentMap(OnceLock<Mutex<HashMap<AgentId, Value>>>);
impl OnceLockCurrentMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Value>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("CURRENT_MAP poisoned")
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The NVIDIA NIM backend bundle.
pub struct NvidiaNimBundle;

#[async_trait]
impl Bundle for NvidiaNimBundle {
    fn name(&self) -> &str {
        "nvidia_nim_backend"
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
            "reflect" => reflect_reply(agent_id, kernel).await,
            "boot" | "shutdown" => Value::Null,
            "send" => send_reply(agent_id, payload, kernel).await,
            "history" => history_reply(agent_id, payload, kernel).await,
            "interrupt" => interrupt_reply(agent_id),
            "refresh_menu" => {
                invalidate_menu(agent_id);
                json!({"refreshed": true})
            }
            "set_api_key" => set_api_key_reply(agent_id, payload, kernel).await,
            "clear_api_key" => clear_api_key_reply(agent_id, kernel).await,
            "status" => status_reply(agent_id, payload),
            other => json!({"error": format!("nvidia_nim_backend: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // Best-effort: drop cached client, abort any in-flight task.
        HTTP_CLIENTS.lock().remove(agent_id);
        if let Some(h) = IN_FLIGHT_TASKS.lock().remove(agent_id) {
            h.abort();
        }
        SEND_LOCKS
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("SEND_LOCKS poisoned")
            .remove(agent_id);
        MENU_CACHE.lock().remove(agent_id);
        QUEUE_MAP.lock().remove(agent_id);
        CURRENT_MAP.lock().remove(agent_id);
        Ok(())
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

// ── path helpers ────────────────────────────────────────────────────

fn safe_client(client_id: &str) -> String {
    let trimmed = client_id.trim();
    let raw = if trimmed.is_empty() {
        DEFAULT_CLIENT_ID
    } else {
        trimmed
    };
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.len() > 64 {
        out.truncate(64);
    }
    if out.is_empty() {
        DEFAULT_CLIENT_ID.to_string()
    } else {
        out
    }
}

fn chat_path(self_id: &AgentId, client_id: &str) -> String {
    format!(
        ".fantastic/agents/{}/chat_{}.json",
        self_id,
        safe_client(client_id),
    )
}

fn key_path(self_id: &AgentId) -> String {
    format!(".fantastic/agents/{}/api_key", self_id)
}

// ── file-agent-routed I/O ───────────────────────────────────────────

fn file_agent_id(self_id: &AgentId, kernel: &Kernel) -> Option<String> {
    meta_string(self_id, kernel, "file_agent_id")
}

async fn file_read(self_id: &AgentId, kernel: &Arc<Kernel>, path: &str) -> Option<String> {
    let fid = file_agent_id(self_id, kernel)?;
    let reply = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "read", "path": path}),
        )
        .await;
    reply
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn file_write(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    path: &str,
    content: &str,
) -> Result<(), String> {
    let fid = file_agent_id(self_id, kernel).ok_or_else(|| "file_agent_id unset".to_string())?;
    let reply = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "write", "path": path, "content": content}),
        )
        .await;
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    Ok(())
}

async fn file_delete(self_id: &AgentId, kernel: &Arc<Kernel>, path: &str) -> bool {
    let Some(fid) = file_agent_id(self_id, kernel) else {
        return false;
    };
    let reply = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "delete", "path": path}),
        )
        .await;
    reply
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

async fn read_api_key(self_id: &AgentId, kernel: &Arc<Kernel>) -> Option<String> {
    let raw = file_read(self_id, kernel, &key_path(self_id)).await?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

async fn has_api_key(self_id: &AgentId, kernel: &Arc<Kernel>) -> bool {
    read_api_key(self_id, kernel).await.is_some()
}

async fn load_history(self_id: &AgentId, kernel: &Arc<Kernel>, client_id: &str) -> Vec<Value> {
    let Some(raw) = file_read(self_id, kernel, &chat_path(self_id, client_id)).await else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Value>>(&raw).unwrap_or_default()
}

async fn save_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    messages: &[Value],
) -> Result<(), String> {
    let body = serde_json::to_string_pretty(&messages).map_err(|e| format!("serialize: {e}"))?;
    file_write(self_id, kernel, &chat_path(self_id, client_id), &body).await
}

// ── menu cache ──────────────────────────────────────────────────────

fn invalidate_menu(self_id: &AgentId) {
    MENU_CACHE.lock().remove(self_id);
}

// ── timestamps + ids ────────────────────────────────────────────────

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn new_send_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    format!("snd_{:08x}", (mix as u32))
}

// ── caller-route helpers ────────────────────────────────────────────
//
// Per the LLM contract, stream events go to the originating caller
// ONLY. In Python, `client_id == "cli"` round-trips through the
// `cli` agent (stdout renderer); in this Rust port the `cli` bundle
// is a state-subscriber, not an agent. We route every event uniformly
// via `kernel.emit(<client_id>, ev)` so tests can drain the inbox and
// real callers (browser WS clients) get the same shape.

async fn to_caller(kernel: &Arc<Kernel>, self_id: &AgentId, client_id: &str, mut ev: Value) {
    if let Value::Object(ref mut obj) = ev {
        obj.entry("client_id")
            .or_insert_with(|| Value::String(client_id.to_string()));
        obj.entry("source")
            .or_insert_with(|| Value::String(self_id.0.clone()));
    }
    kernel.emit(&AgentId::from(client_id), ev).await;
}

async fn emit_status(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    client_id: &str,
    phase: &str,
    extras: Map<String, Value>,
) {
    let mut detail = extras;
    // Auto-fill correlation fields from CURRENT_MAP if present.
    // Snapshot + release the lock before any further locking to avoid
    // re-entrant deadlocks (we re-acquire below to write back).
    let cur_opt = {
        let guard = CURRENT_MAP.lock();
        guard.get(self_id).cloned()
    };
    if let Some(cur) = cur_opt {
        if let Some(obj) = cur.as_object() {
            if !detail.contains_key("send_id") {
                if let Some(v) = obj.get("send_id") {
                    detail.insert("send_id".to_string(), v.clone());
                }
            }
            if !detail.contains_key("started_at") {
                if let Some(v) = obj.get("started_at") {
                    detail.insert("started_at".to_string(), v.clone());
                }
            }
        }
        // Phase mutation on the current snapshot.
        let mut updated = cur.clone();
        if let Some(o) = updated.as_object_mut() {
            o.insert("phase".to_string(), json!(phase));
        }
        CURRENT_MAP.lock().insert(self_id.clone(), updated);
    }
    if !detail.contains_key("queue_depth") {
        let depth = {
            let guard = QUEUE_MAP.lock();
            guard.get(self_id).map(|q| q.len()).unwrap_or(0)
        };
        detail.insert("queue_depth".to_string(), json!(depth));
    }
    let ev = json!({
        "type": "status",
        "source": self_id.0,
        "phase": phase,
        "detail": Value::Object(detail),
        "ts": now_secs(),
    });
    to_caller(kernel, self_id, client_id, ev).await;
}

// ── HTTP client cache ───────────────────────────────────────────────

fn build_client(api_key: &str) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    let val = format!("Bearer {api_key}");
    let hv = reqwest::header::HeaderValue::from_str(&val)
        .map_err(|e| format!("api_key has illegal header chars: {e}"))?;
    headers.insert(reqwest::header::AUTHORIZATION, hv);
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("text/event-stream"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build client: {e}"))
}

async fn get_or_build_client(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<reqwest::Client>, String> {
    if let Some(c) = HTTP_CLIENTS.lock().get(self_id).cloned() {
        return Ok(c);
    }
    let Some(key) = read_api_key(self_id, kernel).await else {
        return Err("api_key not set".to_string());
    };
    let client = Arc::new(build_client(&key)?);
    HTTP_CLIENTS
        .lock()
        .insert(self_id.clone(), Arc::clone(&client));
    Ok(client)
}

fn drop_cached_client(self_id: &AgentId) {
    HTTP_CLIENTS.lock().remove(self_id);
}

// ── reflect ─────────────────────────────────────────────────────────

async fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let endpoint =
        meta_string(agent_id, kernel, "endpoint").unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let model = meta_string(agent_id, kernel, "model").unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let file_agent_id_v = meta_string(agent_id, kernel, "file_agent_id");
    // `has_api_key` requires kernel.send to the file agent. We need an
    // Arc<Kernel> for that — caller already passes one in.
    let arc_kernel = Arc::clone(kernel);
    let has_key = has_api_key(agent_id, &arc_kernel).await;
    let generating = IN_FLIGHT_TASKS
        .lock()
        .get(agent_id)
        .map(|h| !h.is_finished())
        .unwrap_or(false);
    json!({
        "id": agent_id.as_str(),
        "sentence": "NVIDIA NIM-backed LLM agent (OpenAI-compatible, native tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "file_agent_id": file_agent_id_v,
        "has_api_key": has_key,
        "generating": generating,
        "verbs": {
            "reflect": "Identity + model + endpoint + has_api_key + generating + file_agent_id binding. No args. The api_key value itself is NEVER returned — only the boolean.",
            "boot": "No-op. Returns null.",
            "send": "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller. Failfast if file_agent_id unset OR api_key not set.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id}.",
            "interrupt": "No args. Cancels any in-flight `send`. Returns {interrupted:bool}.",
            "refresh_menu": "No args. Drops the cached agent menu.",
            "set_api_key": "args: api_key:str (req). Persists to .fantastic/agents/<id>/api_key via file agent. Drops cached client.",
            "clear_api_key": "No args. Deletes the api_key sidecar. Returns {ok:true, deleted:bool}.",
            "status": "args: client_id:str?. Privacy-filtered queue/in-flight snapshot.",
        },
        "emits": {
            "queued": "{type:'queued', source, client_id, send_id} — back-compat queue notice.",
            "token": "{type:'token', text, source, client_id} — streaming chunk.",
            "say": "{type:'say', text, source, client_id} — per tool_call summary plus rate-limit notices.",
            "status": "{type:'status', source, client_id, ts, phase, detail} — phase machine: queued|thinking|streaming|tool_calling|done. detail.waiting_on='rate_limit' + wait_s during 429 backoff.",
            "done": "{type:'done', source, client_id} — back-compat end marker.",
        },
        "concurrency": "Per-backend FIFO lock around `send`. reflect/history/interrupt/set_api_key/clear_api_key skip the lock.",
    })
}

// ── api_key verbs ───────────────────────────────────────────────────

async fn set_api_key_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if file_agent_id(agent_id, kernel).is_none() {
        return json!({"error": "nvidia_nim_backend: file_agent_id required"});
    }
    let key = payload.get("api_key").and_then(Value::as_str).unwrap_or("");
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return json!({"error": "set_api_key: api_key must be a non-empty string"});
    }
    if let Err(e) = file_write(agent_id, kernel, &key_path(agent_id), trimmed).await {
        return json!({"error": format!("set_api_key: file write failed: {e}")});
    }
    drop_cached_client(agent_id);
    json!({"ok": true})
}

async fn clear_api_key_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    if file_agent_id(agent_id, kernel).is_none() {
        return json!({"error": "nvidia_nim_backend: file_agent_id required"});
    }
    let deleted = file_delete(agent_id, kernel, &key_path(agent_id)).await;
    drop_cached_client(agent_id);
    json!({"ok": true, "deleted": deleted})
}

// ── history / interrupt / status ────────────────────────────────────

async fn history_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if file_agent_id(agent_id, kernel).is_none() {
        return json!({"error": "nvidia_nim_backend: file_agent_id required"});
    }
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let messages = load_history(agent_id, kernel, &client_id).await;
    json!({"messages": messages, "client_id": client_id})
}

fn interrupt_reply(agent_id: &AgentId) -> Value {
    let removed = IN_FLIGHT_TASKS.lock().remove(agent_id);
    if let Some(h) = removed {
        if !h.is_finished() {
            h.abort();
            return json!({"interrupted": true});
        }
    }
    json!({"interrupted": false})
}

fn status_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let cid_in = payload.get("client_id").and_then(Value::as_str);
    let cid = cid_in.map(safe_client);
    let cur = CURRENT_MAP.lock().get(agent_id).cloned();
    let queue: Vec<Value> = QUEUE_MAP.lock().get(agent_id).cloned().unwrap_or_default();
    let (mine_pending, others_pending, current_out) = match cid.as_deref() {
        Some(my_cid) => {
            let is_mine = cur
                .as_ref()
                .and_then(|c| c.get("client_id"))
                .and_then(Value::as_str)
                .map(|s| s == my_cid)
                .unwrap_or(false);
            let cur_out = cur.as_ref().map(|c| {
                let mut m = Map::new();
                if let Some(o) = c.as_object() {
                    if let Some(v) = o.get("phase") {
                        m.insert("phase".into(), v.clone());
                    }
                    if let Some(v) = o.get("send_id") {
                        m.insert("send_id".into(), v.clone());
                    }
                    if let Some(v) = o.get("started_at") {
                        m.insert("started_at".into(), v.clone());
                    }
                    let started = o
                        .get("started_at")
                        .and_then(Value::as_f64)
                        .unwrap_or_else(now_secs);
                    m.insert("elapsed".into(), json!(now_secs() - started));
                    m.insert("is_mine".into(), json!(is_mine));
                    if is_mine {
                        if let Some(v) = o.get("text") {
                            m.insert("text".into(), v.clone());
                        }
                        if let Some(v) = o.get("text_so_far") {
                            m.insert("text_so_far".into(), v.clone());
                        }
                        if let Some(v) = o.get("last_tool") {
                            m.insert("last_tool".into(), v.clone());
                        }
                    }
                }
                Value::Object(m)
            });
            let mut mine: Vec<Value> = Vec::new();
            let mut others = 0usize;
            for e in &queue {
                let cid_match = e
                    .get("client_id")
                    .and_then(Value::as_str)
                    .map(|s| s == my_cid)
                    .unwrap_or(false);
                if cid_match {
                    let mut m = Map::new();
                    if let Some(v) = e.get("send_id") {
                        m.insert("send_id".into(), v.clone());
                    }
                    if let Some(v) = e.get("text") {
                        m.insert("text".into(), v.clone());
                    }
                    if let Some(v) = e.get("queued_at") {
                        m.insert("queued_at".into(), v.clone());
                    }
                    mine.push(Value::Object(m));
                } else {
                    others += 1;
                }
            }
            (mine, others, cur_out)
        }
        None => {
            let cur_out = cur.as_ref().map(|c| {
                let mut m = Map::new();
                if let Some(o) = c.as_object() {
                    for (k, v) in o {
                        if k != "text" && k != "text_so_far" && k != "last_tool" {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                    let started = o
                        .get("started_at")
                        .and_then(Value::as_f64)
                        .unwrap_or_else(now_secs);
                    m.insert("elapsed".into(), json!(now_secs() - started));
                    m.insert("is_mine".into(), json!(false));
                }
                Value::Object(m)
            });
            (Vec::new(), queue.len(), cur_out)
        }
    };
    let generating = cur.is_some();
    json!({
        "source": agent_id.as_str(),
        "client_id": cid,
        "generating": generating,
        "current": current_out,
        "mine_pending": mine_pending,
        "others_pending": others_pending,
    })
}

// ── send orchestration ──────────────────────────────────────────────

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if file_agent_id(agent_id, kernel).is_none() {
        return json!({"error": "nvidia_nim_backend: file_agent_id required"});
    }
    if !has_api_key(agent_id, kernel).await {
        return json!({"error": "nvidia_nim_backend: api_key not set; call set_api_key first"});
    }
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let send_id = new_send_id();
    let entry = json!({
        "client_id": client_id,
        "text": text,
        "send_id": send_id,
        "queued_at": now_secs(),
    });
    QUEUE_MAP
        .lock()
        .entry(agent_id.clone())
        .or_default()
        .push(entry.clone());

    let lock = SEND_LOCKS.get_or_create(agent_id);
    let was_contended = lock.try_lock().is_err();
    if was_contended {
        let ahead = QUEUE_MAP
            .lock()
            .get(agent_id)
            .map(|q| q.len().saturating_sub(1))
            .unwrap_or(0);
        to_caller(
            kernel,
            agent_id,
            &client_id,
            json!({
                "type": "queued",
                "source": agent_id.0,
                "send_id": send_id,
            }),
        )
        .await;
        let mut extras = Map::new();
        extras.insert("send_id".into(), json!(send_id));
        extras.insert("ahead".into(), json!(ahead));
        emit_status(kernel, agent_id, &client_id, "queued", extras).await;
    }

    let _guard = lock.lock().await;

    // Dequeue + promote to current.
    {
        let mut q = QUEUE_MAP.lock();
        if let Some(list) = q.get_mut(agent_id) {
            list.retain(|e| {
                e.get("send_id")
                    .and_then(Value::as_str)
                    .map(|s| s != send_id)
                    .unwrap_or(true)
            });
        }
    }
    {
        let mut cur = entry.clone();
        if let Some(o) = cur.as_object_mut() {
            o.insert("started_at".into(), json!(now_secs()));
            o.insert("phase".into(), json!("thinking"));
            o.insert("text_so_far".into(), json!(""));
        }
        CURRENT_MAP.lock().insert(agent_id.clone(), cur);
    }
    emit_status(kernel, agent_id, &client_id, "thinking", Map::new()).await;

    // Spawn the actual work so `interrupt` can abort it via AbortHandle.
    let aid = agent_id.clone();
    let cid = client_id.clone();
    let txt = text.clone();
    let kclone = Arc::clone(kernel);
    let join: JoinHandle<Value> =
        tokio::spawn(async move { run_generation(&aid, &txt, &kclone, &cid).await });
    IN_FLIGHT_TASKS
        .lock()
        .insert(agent_id.clone(), join.abort_handle());

    let outcome = match tokio::time::timeout(Duration::from_secs(SEND_TIMEOUT_SECS), join).await {
        Ok(Ok(v)) => v,
        Ok(Err(join_err)) => {
            if join_err.is_cancelled() {
                let mut extras = Map::new();
                extras.insert("reason".into(), json!("interrupted"));
                emit_status(kernel, agent_id, &client_id, "done", extras).await;
                to_caller(
                    kernel,
                    agent_id,
                    &client_id,
                    json!({"type":"done","source": agent_id.0}),
                )
                .await;
                json!({"response": "", "interrupted": true, "client_id": client_id})
            } else {
                let msg = format!("send: task panicked: {join_err}");
                let mut extras = Map::new();
                extras.insert("reason".into(), json!("error"));
                extras.insert("error".into(), json!(msg.clone()));
                emit_status(kernel, agent_id, &client_id, "done", extras).await;
                to_caller(
                    kernel,
                    agent_id,
                    &client_id,
                    json!({"type":"done","source": agent_id.0}),
                )
                .await;
                json!({"error": msg, "client_id": client_id})
            }
        }
        Err(_) => {
            // Timeout: abort the in-flight task.
            if let Some(h) = IN_FLIGHT_TASKS.lock().remove(agent_id) {
                h.abort();
            }
            let mut extras = Map::new();
            extras.insert("reason".into(), json!("timeout"));
            emit_status(kernel, agent_id, &client_id, "done", extras).await;
            to_caller(
                kernel,
                agent_id,
                &client_id,
                json!({"type":"done","source": agent_id.0}),
            )
            .await;
            json!({
                "error": format!("send: timeout after {SEND_TIMEOUT_SECS}s"),
                "client_id": client_id,
            })
        }
    };

    IN_FLIGHT_TASKS.lock().remove(agent_id);
    CURRENT_MAP.lock().remove(agent_id);
    outcome
}

// ── agentic loop ────────────────────────────────────────────────────

/// Outcome of one provider streaming pass.
struct StreamPass {
    content: String,
    tool_calls: Vec<ToolCall>,
}

#[derive(Clone)]
struct ToolCall {
    id: String,
    name: String,
    /// Already parsed (best-effort JSON parse of the accumulated
    /// fragments). Defaults to `{}` on parse failure.
    arguments: Value,
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    /// Accumulator for the streamed `function.arguments` fragments —
    /// OpenAI ships these as STRING pieces that JSON-parse only once
    /// concatenated. We hold the raw text here and parse on stream
    /// end.
    arguments: String,
}

async fn run_generation(
    self_id: &AgentId,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Value {
    let endpoint =
        meta_string(self_id, kernel, "endpoint").unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let model = meta_string(self_id, kernel, "model").unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let mut messages = match assemble_messages(self_id, user_text, kernel, client_id).await {
        Ok(m) => m,
        Err(e) => return json!({"error": format!("assemble: {e}"), "client_id": client_id}),
    };
    let mut last_text;

    let send_tool = json!({
        "type": "function",
        "function": {
            "name": "send",
            "description": "Send a message to any agent in the Fantastic substrate. Universal verb on every agent: reflect (returns identity + state). Discover agents by sending list_agents to the core agent.",
            "parameters": {
                "type": "object",
                "properties": {
                    "target_id": {
                        "type": "string",
                        "description": "Agent id to send the payload to (e.g. 'core', 'cli', 'terminal_xxx').",
                    },
                    "payload": {
                        "type": "object",
                        "description": "{\"type\": \"<verb>\", ...fields}. Universal verb: reflect.",
                    },
                },
                "required": ["target_id", "payload"],
            },
        },
    });

    let mut iteration = 0;
    loop {
        iteration += 1;
        if iteration > 1 {
            emit_status(kernel, self_id, client_id, "thinking", Map::new()).await;
        }
        let body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "tools": [send_tool],
            "tool_choice": "auto",
        });
        let pass = match stream_with_rate_limit_retry(self_id, kernel, client_id, &endpoint, &body)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let mut extras = Map::new();
                extras.insert("reason".into(), json!("error"));
                extras.insert("error".into(), json!(e.clone()));
                emit_status(kernel, self_id, client_id, "done", extras).await;
                to_caller(
                    kernel,
                    self_id,
                    client_id,
                    json!({"type":"done","source": self_id.0}),
                )
                .await;
                return json!({"error": e, "client_id": client_id});
            }
        };
        last_text = pass.content.clone();

        if pass.tool_calls.is_empty() {
            break;
        }

        // Append assistant turn carrying tool_calls (OpenAI wants
        // arguments as a JSON string).
        let tcs: Vec<Value> = pass
            .tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".to_string()),
                    },
                })
            })
            .collect();
        messages.push(json!({
            "role": "assistant",
            "content": last_text,
            "tool_calls": tcs,
        }));

        // Execute tool_calls sequentially (rust + serial preserves
        // model-emitted ordering deterministically; agentic correctness
        // is unchanged from Python's parallel gather).
        for c in &pass.tool_calls {
            let args = &c.arguments;
            let target = args
                .get("target_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let payload = args.get("payload").cloned().unwrap_or(Value::Null);
            let verb = payload
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_entry = json!({
                "call_id": c.id,
                "target": target,
                "verb": verb,
                "args": args,
            });
            {
                let mut cur = CURRENT_MAP.lock();
                if let Some(c) = cur.get_mut(self_id) {
                    if let Some(o) = c.as_object_mut() {
                        o.insert("last_tool".into(), tool_entry.clone());
                    }
                }
            }
            let mut extras = Map::new();
            extras.insert("tool".into(), tool_entry.clone());
            emit_status(kernel, self_id, client_id, "tool_calling", extras).await;

            let reply = if target.is_empty() {
                json!({"error": "tool_call: empty target_id"})
            } else {
                kernel.send(&AgentId::from(target.as_str()), payload).await
            };
            let reply_str = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".to_string());
            let preview: String = reply_str.chars().take(120).collect();
            let mut tool_done = tool_entry.clone();
            if let Some(o) = tool_done.as_object_mut() {
                o.insert("reply_preview".into(), json!(preview.clone()));
            }
            {
                let mut cur = CURRENT_MAP.lock();
                if let Some(c) = cur.get_mut(self_id) {
                    if let Some(o) = c.as_object_mut() {
                        o.insert("last_tool".into(), tool_done.clone());
                    }
                }
            }
            let mut extras = Map::new();
            extras.insert("tool".into(), tool_done);
            emit_status(kernel, self_id, client_id, "tool_calling", extras).await;
            to_caller(
                kernel,
                self_id,
                client_id,
                json!({
                    "type": "say",
                    "text": format!("[tool {} -> {}]", target, preview),
                    "source": self_id.0,
                }),
            )
            .await;

            messages.push(json!({
                "role": "tool",
                "tool_call_id": c.id,
                "name": c.name,
                "content": reply_str,
            }));
        }
        invalidate_menu(self_id);
    }

    // Done.
    let mut extras = Map::new();
    extras.insert("reason".into(), json!("ok"));
    emit_status(kernel, self_id, client_id, "done", extras).await;
    to_caller(
        kernel,
        self_id,
        client_id,
        json!({"type":"done", "source": self_id.0}),
    )
    .await;

    // Persist history (user + assistant final).
    let mut history = load_history(self_id, kernel, client_id).await;
    history.push(json!({"role": "user", "content": user_text}));
    history.push(json!({"role": "assistant", "content": last_text}));
    let _ = save_history(self_id, kernel, client_id, &history).await;
    json!({
        "response": last_text,
        "final": last_text,
        "client_id": client_id,
    })
}

// ── prompt assembly ─────────────────────────────────────────────────

async fn assemble_messages(
    self_id: &AgentId,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Result<Vec<Value>, String> {
    // Pull root reflect (tree) + self reflect.
    let primer = kernel
        .send(&AgentId::from("kernel"), json!({"type": "reflect"}))
        .await;
    let me = kernel.send(self_id, json!({"type": "reflect"})).await;

    // Build menu lazily.
    let menu = {
        let cached = MENU_CACHE.lock().get(self_id).cloned();
        match cached {
            Some(m) => m,
            None => {
                let built = build_menu(self_id, kernel).await;
                MENU_CACHE.lock().insert(self_id.clone(), built.clone());
                built
            }
        }
    };

    let sys = format!(
        "{}\n\nYou are `{}`. {}\n\n{}\n\n{}",
        render_reflect(&primer),
        self_id,
        render_reflect(&me),
        render_menu(&menu),
        SEND_HOWTO,
    );
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": sys})];
    messages.extend(load_history(self_id, kernel, client_id).await);
    messages.push(json!({"role": "user", "content": user_text}));
    Ok(messages)
}

fn render_reflect(v: &Value) -> String {
    let Some(obj) = v.as_object() else {
        return String::new();
    };
    let sentence = obj
        .get("sentence")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut parts: Vec<String> = Vec::new();
    for (k, val) in obj {
        if k == "sentence" {
            continue;
        }
        let rendered = match val {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        parts.push(format!("{k}={rendered}"));
    }
    format!("{sentence}  {}", parts.join("  "))
        .trim()
        .to_string()
}

async fn build_menu(self_id: &AgentId, kernel: &Arc<Kernel>) -> Vec<Value> {
    let online = kernel
        .send(&AgentId::from("core"), json!({"type": "list_agents"}))
        .await;
    let mut items: Vec<Value> = Vec::new();
    let Some(arr) = online.get("agents").and_then(Value::as_array) else {
        return items;
    };
    for a in arr {
        let Some(id) = a.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id == self_id.as_str() {
            continue;
        }
        let r = kernel
            .send(&AgentId::from(id), json!({"type": "reflect"}))
            .await;
        let sentence = r
            .get("sentence")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let verbs: Vec<Value> = r
            .get("verbs")
            .and_then(Value::as_object)
            .map(|o| o.keys().map(|k| Value::String(k.clone())).collect())
            .unwrap_or_default();
        items.push(json!({"id": id, "sentence": sentence, "verbs": verbs}));
    }
    items
}

fn render_menu(menu: &[Value]) -> String {
    if menu.is_empty() {
        return "## Available agents\n(none — only `core` and `self`)".to_string();
    }
    let mut lines: Vec<String> = vec![
        "## Available agents (reflect on any for full verb signatures + arg shapes)".to_string(),
    ];
    for m in menu {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("");
        let sentence = m.get("sentence").and_then(Value::as_str).unwrap_or("");
        let verbs: Vec<String> = m
            .get("verbs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let head_iter = verbs
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let head = if verbs.len() > 10 {
            format!("{head_iter} …")
        } else {
            head_iter
        };
        let head = if head.is_empty() {
            "(none)".to_string()
        } else {
            head
        };
        lines.push(format!("- `{id}` — {sentence} — verbs: {head}"));
    }
    lines.join("\n")
}

const SEND_HOWTO: &str = "## How to use the `send` tool\nYou have ONE tool: `send(target_id, payload)`. EVERY action goes through it.\n- To do something concrete (read a file, run python, list agents, etc.), pick\n  an agent from the menu above whose verbs cover what you need, then build\n  `{type:'<verb>', ...args}` and pass it as `payload`.\n- To learn an agent's full verb signatures (arg names, types):\n  `send('<id>', {type:'reflect'})` returns `{verbs: {name: 'doc'}, ...}`.\n- To rebuild your menu of agents (useful right after you create one):\n  `send('<your_own_id>', {type:'refresh_menu'})` — next turn shows the fresh menu.\n- NEVER claim \"I don't have access\" without trying the menu first. The\n  send tool reaches every agent in the system.\n";

// ── streaming + SSE + 429 retry ─────────────────────────────────────

/// Wrap one streaming POST with retry-once-on-429 (before any chunk
/// yielded). Returns the accumulated `StreamPass` (content + tool_calls)
/// or an `Err` describing the failure.
///
/// Control flow:
///   loop attempt in 0..=RATE_LIMIT_MAX_RETRIES {
///       send POST; on 429 with no chunks yielded: sleep + retry;
///       otherwise stream the body, accumulate, return.
///   }
///
/// `yielded_anything` tracks whether at least one decoded SSE event
/// has been parsed. Once `true`, a mid-stream 429 propagates as a
/// hard error rather than restarting (we'd otherwise lose half a
/// completion's tokens).
async fn stream_with_rate_limit_retry(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    endpoint: &str,
    body: &Value,
) -> Result<StreamPass, String> {
    let mut attempt: u32 = 0;
    loop {
        let client = get_or_build_client(self_id, kernel).await?;
        let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
        let resp = match client.post(&url).json(body).send().await {
            Ok(r) => r,
            Err(e) => return Err(format!("http: {e}")),
        };
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // No chunks yielded yet (we haven't even started reading
            // the body). Check retry budget.
            if attempt < RATE_LIMIT_MAX_RETRIES {
                let wait = parse_retry_after(resp.headers());
                attempt += 1;
                // Drain the response body to free the connection.
                drop(resp);
                to_caller(
                    kernel,
                    self_id,
                    client_id,
                    json!({
                        "type": "say",
                        "text": format!("[provider rate limited (429); waiting {wait}s]"),
                        "source": self_id.0,
                    }),
                )
                .await;
                let mut extras = Map::new();
                extras.insert("waiting_on".into(), json!("rate_limit"));
                extras.insert("wait_s".into(), json!(wait));
                emit_status(kernel, self_id, client_id, "thinking", extras).await;
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            } else {
                let wait = parse_retry_after(resp.headers());
                return Err(format!("send: rate limited (429); retry in {wait}s"));
            }
        }
        if !status.is_success() {
            return Err(format!("send: provider HTTP {}", status.as_u16()));
        }
        // Stream the body.
        return consume_sse(self_id, kernel, client_id, resp).await;
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let n: u64 = match raw.trim().parse::<u64>() {
        Ok(v) => v,
        Err(_) => return RATE_LIMIT_DEFAULT_WAIT_SECS,
    };
    n.clamp(1, RATE_LIMIT_MAX_WAIT_SECS)
}

/// Drain the body bytes_stream, split into SSE lines, parse JSON
/// payloads, emit `token` events for assistant content and accumulate
/// per-index tool-call argument fragments.
///
/// SSE wire format on this endpoint:
///   `data: {json}\n`
///   `data: {json}\n`
///   `data: [DONE]\n`
///
/// Lines may arrive split across multiple body chunks; we buffer
/// bytes and only consume a line once a `\n` boundary is seen.
async fn consume_sse(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    resp: reqwest::Response,
) -> Result<StreamPass, String> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();
    let mut pending: HashMap<u32, PendingToolCall> = HashMap::new();
    let mut first_chunk = true;
    let mut done_marker_seen = false;

    while let Some(chunk_res) = stream.next().await {
        let bytes = match chunk_res {
            Ok(b) => b,
            Err(e) => return Err(format!("stream: {e}")),
        };
        buf.extend_from_slice(&bytes);
        // Scan for newline boundaries.
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            // Slice off one line (without the trailing newline).
            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
            let line_owned = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            let line = line_owned.as_str();
            if line.is_empty() {
                continue;
            }
            if !line.starts_with("data:") {
                // Other SSE field (event:, id:, retry:) — ignore.
                continue;
            }
            let data = line[5..].trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                done_marker_seen = true;
                break;
            }
            let obj: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(choices) = obj.get("choices").and_then(Value::as_array) else {
                continue;
            };
            let Some(first) = choices.first() else {
                continue;
            };
            let delta = first.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    if first_chunk {
                        emit_status(kernel, self_id, client_id, "streaming", Map::new()).await;
                        first_chunk = false;
                    }
                    content_parts.push(content.to_string());
                    // text_so_far accumulator (for status snapshots).
                    {
                        let mut cur = CURRENT_MAP.lock();
                        if let Some(c) = cur.get_mut(self_id) {
                            if let Some(o) = c.as_object_mut() {
                                let prev = o
                                    .get("text_so_far")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                o.insert("text_so_far".into(), json!(prev + content));
                            }
                        }
                    }
                    to_caller(
                        kernel,
                        self_id,
                        client_id,
                        json!({
                            "type": "token",
                            "text": content,
                            "source": self_id.0,
                        }),
                    )
                    .await;
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    let idx = tc
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32)
                        .unwrap_or(0);
                    let slot = pending.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            slot.id = id.to_string();
                        }
                    }
                    if let Some(fn_obj) = tc.get("function") {
                        if let Some(name) = fn_obj.get("name").and_then(Value::as_str) {
                            if !name.is_empty() {
                                slot.name = name.to_string();
                            }
                        }
                        if let Some(args) = fn_obj.get("arguments").and_then(Value::as_str) {
                            // Per-index accumulation — OpenAI streams
                            // arguments as STRING fragments that only
                            // JSON-parse once concatenated.
                            slot.arguments.push_str(args);
                        }
                    }
                }
            }
        }
        if done_marker_seen {
            break;
        }
    }

    // Finalize tool-calls: parse the accumulated arguments string.
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut idxs: Vec<u32> = pending.keys().copied().collect();
    idxs.sort_unstable();
    for i in idxs {
        let slot = pending.remove(&i).unwrap();
        if slot.name.is_empty() {
            continue;
        }
        let parsed: Value = if slot.arguments.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&slot.arguments).unwrap_or(json!({}))
        };
        let id = if slot.id.is_empty() {
            format!("call_{:08x}", i)
        } else {
            slot.id
        };
        tool_calls.push(ToolCall {
            id,
            name: slot.name,
            arguments: parsed,
        });
    }

    Ok(StreamPass {
        content: content_parts.join(""),
        tool_calls,
    })
}

#[cfg(test)]
mod tests;
