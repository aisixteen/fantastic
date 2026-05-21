//! ollama_backend — local LLM (ollama) bundle. Streams tokens +
//! tool-calls. Per-client chat threads, FIFO lock, menu cache.
//! All persistence routed through `file_agent_id`.
//!
//! ## LLM backend contract (canonical reference)
//!
//! Every LLM backend bundle (this one, `nvidia_nim_backend`, future
//! `apple_fm_backend`, etc.) MUST implement the same verb + event
//! shape so any chat UI (`ai_chat_webapp` today, others later) can
//! retarget via a single `update_agent upstream_id=<other>` with no
//! frontend changes.
//!
//! ### Verbs (caller → backend, via `kernel.send`)
//!
//! - `send` args `{text:str, client_id:str?}` →
//!   `{response:str, final:str, client_id:str}` on success,
//!   `{error:str, client_id:str}` on failure.
//! - `history` args `{client_id:str?}` →
//!   `{messages:[{role,content,…}], client_id:str}`.
//! - `interrupt` → `{interrupted:bool}`. Cancels in-flight `send`.
//! - `reflect`, `boot`, `shutdown`, `status`, `refresh_menu` — see below.
//!
//! ### Events (backend → `client_id`'s inbox, via `kernel.emit`)
//!
//! - `{type:"status", source, client_id, ts, phase, detail}` where
//!   `phase` ∈ `queued|thinking|streaming|tool_calling|done`.
//! - `{type:"token", text, source, client_id}` — one per chunk.
//! - `{type:"say", text:"[tool target → reply]", source, client_id}` —
//!   one per tool-call summary.
//! - `{type:"done", source, client_id}` — emitted as the final event.
//!
//! The chat UI watches `client_id`'s inbox via `kernel.watch(...)`;
//! the WS proxy in `fantastic-web` delivers each event as a JSON frame.
//!
//! ### Concurrency model
//!
//! `BACKENDS` (process-global, keyed by agent id) holds per-backend
//! state: an `Arc<tokio::sync::Mutex<()>>` FIFO serializer, the
//! in-flight `JoinHandle`, the current entry (for `status`
//! snapshots), and the pending queue. A `send` enqueues → emits
//! `queued` if the lock is contested → acquires lock → spawns the
//! streaming task → waits with `SEND_TIMEOUT` → cleans up.
//! `interrupt` aborts the JoinHandle; the spawned task's drop path
//! emits the final `status(done, reason="interrupted")` + `{type:"done"}`.
//!
//! ### Persistence
//!
//! `<file_agent.root>/.fantastic/agents/{self_id}/chat_{client_id}.json`
//! — array of `{role, content, tool_calls?, tool_call_id?}` entries.
//! System prompt is rebuilt each turn (menu + reflect lookups);
//! only user/assistant/tool turns hit disk. All I/O via
//! `kernel.send(file_agent_id, {type:"read"|"write", path, content?})`.
//!
//! ### HTTP
//!
//! `POST {endpoint}/api/chat` with `{model, messages, tools?,
//! stream:true}` → line-delimited JSON; each line carries
//! `{message:{content?, tool_calls?}}`. Ollama's `arguments` field is
//! a parsed JSON object — do not re-parse.
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
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "ollama_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Hard ceiling per `send` (mirrors Python's `SEND_TIMEOUT`).
pub const SEND_TIMEOUT_SECS: u64 = 180;

/// Default `client_id` for callers that don't supply one. Matches
/// Python's `DEFAULT_CLIENT_ID`.
pub const DEFAULT_CLIENT_ID: &str = "cli";

/// Default ollama HTTP endpoint.
pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Default model id (mirrors the Python default — overridable via
/// the agent record's `model` field).
pub const DEFAULT_MODEL: &str = "gemma4:e2b";

// ── per-backend state ───────────────────────────────────────────────

/// One queued entry awaiting the FIFO lock.
#[derive(Clone, Debug)]
struct QueuedEntry {
    client_id: String,
    text: String,
    send_id: String,
    queued_at: f64,
}

/// The entry currently holding the FIFO lock (used by `status`).
#[derive(Clone, Debug)]
struct CurrentEntry {
    client_id: String,
    text: String,
    send_id: String,
    started_at: f64,
    phase: String,
    text_so_far: String,
    last_tool: Option<Value>,
}

/// Per-backend coordination state. Cloning the `Arc` is cheap; the
/// owned `Mutex`es are accessed under short critical sections.
struct BackendState {
    /// FIFO serializer — only one `send` runs at a time per backend.
    lock: Arc<AsyncMutex<()>>,
    /// In-flight task handle. Set on `send` lock acquisition; cleared
    /// when the task completes. `interrupt` aborts via this.
    current_task: Mutex<Option<JoinHandle<()>>>,
    /// Snapshot data for the `status` verb. `Some` iff a `send` holds
    /// the lock; `None` otherwise.
    current_meta: Mutex<Option<CurrentEntry>>,
    /// Entries waiting on `lock`. Front = next to acquire.
    queue: Mutex<VecDeque<QueuedEntry>>,
    /// Lazy menu cache — `None` means "rebuild on next assemble".
    /// Invalidated after every tool batch and by `refresh_menu`.
    menu: Mutex<Option<Vec<Value>>>,
}

impl BackendState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lock: Arc::new(AsyncMutex::new(())),
            current_task: Mutex::new(None),
            current_meta: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            menu: Mutex::new(None),
        })
    }
}

/// Process-global map: agent id → backend state. Same `OnceLockMap`
/// shape as `fantastic-scheduler`.
struct OnceLockBackends(OnceLock<Mutex<HashMap<AgentId, Arc<BackendState>>>>);
impl OnceLockBackends {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Arc<BackendState>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("BACKENDS poisoned")
    }
}

static BACKENDS: OnceLockBackends = OnceLockBackends::new();

fn state_for(agent_id: &AgentId) -> Arc<BackendState> {
    let mut map = BACKENDS.lock();
    Arc::clone(
        map.entry(agent_id.clone())
            .or_insert_with(BackendState::new),
    )
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The ollama backend bundle.
pub struct OllamaBackendBundle;

#[async_trait]
impl Bundle for OllamaBackendBundle {
    fn name(&self) -> &str {
        "ollama_backend"
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
            "shutdown" => shutdown_reply(agent_id),
            "send" => send_reply(agent_id, payload, kernel).await,
            "history" => history_reply(agent_id, payload, kernel).await,
            "interrupt" => interrupt_reply(agent_id),
            "refresh_menu" => refresh_menu_reply(agent_id),
            "status" => status_reply(agent_id, payload),
            other => json!({"error": format!("ollama: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // Abort any in-flight task and drop our state slot.
        let state = BACKENDS.lock().remove(agent_id);
        if let Some(s) = state {
            if let Some(task) = s.current_task.lock().expect("task poisoned").take() {
                task.abort();
            }
        }
        Ok(())
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

fn meta_string_or(agent_id: &AgentId, kernel: &Kernel, key: &str, default: &str) -> String {
    meta_string(agent_id, kernel, key).unwrap_or_else(|| default.to_string())
}

// ── persistence ─────────────────────────────────────────────────────

fn safe_client(client_id: &str) -> String {
    let trimmed = client_id.trim();
    let base = if trimmed.is_empty() {
        DEFAULT_CLIENT_ID
    } else {
        trimmed
    };
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.chars().take(64).collect()
}

fn chat_path(self_id: &AgentId, client_id: &str) -> String {
    format!(
        ".fantastic/agents/{}/chat_{}.json",
        self_id,
        safe_client(client_id)
    )
}

async fn file_read(agent_id: &AgentId, kernel: &Arc<Kernel>, path: &str) -> Option<String> {
    let fid = meta_string(agent_id, kernel, "file_agent_id")?;
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
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    path: &str,
    content: &str,
) -> Result<(), String> {
    let fid = match meta_string(agent_id, kernel, "file_agent_id") {
        Some(s) => s,
        None => return Err("file_agent_id unset".to_string()),
    };
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

async fn load_history_messages(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Vec<Value> {
    let path = chat_path(self_id, client_id);
    let Some(raw) = file_read(self_id, kernel, &path).await else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Value>>(&raw).unwrap_or_default()
}

async fn save_history_messages(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    messages: &[Value],
) -> Result<(), String> {
    let path = chat_path(self_id, client_id);
    let body = serde_json::to_string_pretty(messages).map_err(|e| format!("serialize: {e}"))?;
    file_write(self_id, kernel, &path, &body).await
}

// ── time + id helpers ───────────────────────────────────────────────

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn mint_send_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    format!("snd_{:08x}", mix as u32)
}

fn mint_tool_call_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    format!("tc_{:06x}", (mix as u32) & 0xff_ffff)
}

// ── event routing ───────────────────────────────────────────────────

/// Route a streaming event to the originating caller's inbox. cli
/// caller is dispatched via `kernel.send("cli", ...)` so the cli
/// renderer's handler runs; everyone else gets `kernel.emit(self_id, ...)`
/// with the event tagged by `client_id` (the browser WS proxy filters).
async fn to_caller(kernel: &Arc<Kernel>, self_id: &AgentId, client_id: &str, mut ev: Value) {
    if let Some(obj) = ev.as_object_mut() {
        obj.insert("client_id".to_string(), json!(client_id));
    }
    if client_id == DEFAULT_CLIENT_ID {
        // Best-effort — if no cli agent is registered, the send returns
        // an error reply which we discard.
        let _ = kernel.send(&AgentId::from("cli"), ev).await;
    } else {
        kernel.emit(self_id, ev).await;
    }
}

async fn emit_status(
    kernel: &Arc<Kernel>,
    state: &Arc<BackendState>,
    self_id: &AgentId,
    client_id: &str,
    phase: &str,
    extra_detail: Map<String, Value>,
) {
    let (send_id, started_at) = {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        if let Some(c) = cur.as_mut() {
            c.phase = phase.to_string();
            (Some(c.send_id.clone()), Some(c.started_at))
        } else {
            (None, None)
        }
    };
    let queue_depth = state.queue.lock().expect("queue poisoned").len();
    let mut detail = extra_detail;
    if let Some(sid) = send_id {
        detail.entry("send_id").or_insert(json!(sid));
    }
    if let Some(t) = started_at {
        detail.entry("started_at").or_insert(json!(t));
    }
    detail
        .entry("queue_depth")
        .or_insert(json!(queue_depth as u64));
    let ev = json!({
        "type": "status",
        "source": self_id.as_str(),
        "phase": phase,
        "detail": Value::Object(detail),
        "ts": now_secs(),
    });
    to_caller(kernel, self_id, client_id, ev).await;
}

async fn emit_done(kernel: &Arc<Kernel>, self_id: &AgentId, client_id: &str) {
    let ev = json!({"type": "done", "source": self_id.as_str()});
    to_caller(kernel, self_id, client_id, ev).await;
}

// ── prompt assembly ─────────────────────────────────────────────────

fn render_reflect(v: &Value) -> String {
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return String::new(),
    };
    let sentence = obj
        .remove("sentence")
        .and_then(|s| s.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    for (k, val) in obj.iter() {
        let rendered = match val.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(val).unwrap_or_default(),
        };
        parts.push(format!("{k}={rendered}"));
    }
    let fields = parts.join("  ");
    format!("{sentence}  {fields}").trim().to_string()
}

async fn build_menu(self_id: &AgentId, kernel: &Arc<Kernel>) -> Vec<Value> {
    let online = kernel
        .send(&AgentId::from("core"), json!({"type": "list_agents"}))
        .await;
    let Some(agents) = online.get("agents").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for a in agents {
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
        let verb_names: Vec<String> = match r.get("verbs") {
            Some(Value::Object(m)) => m.keys().cloned().collect(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        items.push(json!({
            "id": id,
            "sentence": sentence,
            "verbs": verb_names,
        }));
    }
    items
}

fn render_menu(menu: &[Value]) -> String {
    if menu.is_empty() {
        return "## Available agents\n(none — only `core` and `self`)".to_string();
    }
    let mut lines = vec![
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
        let head = if verbs.len() > 10 {
            format!("{} …", verbs[..10].join(", "))
        } else {
            verbs.join(", ")
        };
        let head_display = if head.is_empty() {
            "(none)".to_string()
        } else {
            head
        };
        lines.push(format!("- `{id}` — {sentence} — verbs: {head_display}"));
    }
    lines.join("\n")
}

const SEND_HOWTO: &str = r#"## How to use the `send` tool
You have ONE tool: `send(target_id, payload)`. EVERY action goes through it.
- To do something concrete (read a file, run python, list agents, etc.), pick
  an agent from the menu above whose verbs cover what you need, then build
  `{type:'<verb>', ...args}` and pass it as `payload`.
- To learn an agent's full verb signatures (arg names, types):
  `send('<id>', {type:'reflect'})` returns `{verbs: {name: 'doc'}, ...}`.
- To rebuild your menu of agents (useful right after you create one):
  `send('<your_own_id>', {type:'refresh_menu'})` — next turn shows the fresh menu.
- NEVER claim "I don't have access" without trying the menu first. The
  send tool reaches every agent in the system.
"#;

fn send_tool_def() -> Value {
    json!({
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
    })
}

async fn assemble_messages(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Vec<Value> {
    let primer = kernel
        .send(&AgentId::from("kernel"), json!({"type": "reflect"}))
        .await;
    let me = kernel.send(self_id, json!({"type": "reflect"})).await;

    // Lazy menu rebuild.
    let needs_rebuild = state.menu.lock().expect("menu poisoned").is_none();
    if needs_rebuild {
        let menu = build_menu(self_id, kernel).await;
        *state.menu.lock().expect("menu poisoned") = Some(menu);
    }
    let menu = state
        .menu
        .lock()
        .expect("menu poisoned")
        .clone()
        .unwrap_or_default();

    let sys_blocks = [
        render_reflect(&primer),
        format!("You are `{}`. {}", self_id, render_reflect(&me)),
        render_menu(&menu),
        SEND_HOWTO.to_string(),
    ];
    let system_content = sys_blocks.join("\n\n");
    let mut messages = vec![json!({"role": "system", "content": system_content})];
    messages.extend(load_history_messages(self_id, kernel, client_id).await);
    messages.push(json!({"role": "user", "content": user_text}));
    messages
}

// ── ollama streaming ────────────────────────────────────────────────

/// One chunk decoded from ollama's NDJSON stream.
enum OllamaChunk {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// Stream ollama's `/api/chat`. Returns a `Vec` of chunks rather than
/// a true async stream — keeps the call-site simple and matches the
/// way the Python loop consumes it. Errors collapse to an empty list
/// (the caller observes the truncated stream + the next iteration
/// will terminate).
async fn ollama_chat(
    endpoint: &str,
    model: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<Vec<OllamaChunk>, String> {
    let body = json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "stream": true,
    });
    let client = reqwest::Client::new();
    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("ollama: request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ollama: HTTP {}", resp.status()));
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<OllamaChunk> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => return Err(format!("ollama: stream error: {e}")),
        };
        buf.extend_from_slice(&bytes);
        // Split on newlines; each complete line is one JSON object.
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            // strip trailing newline
            let trimmed = &line[..line.len().saturating_sub(1)];
            if trimmed.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_slice::<Value>(trimmed) else {
                continue;
            };
            decode_chunk_into(&parsed, &mut out);
        }
    }
    // flush a trailing line without newline (most ollama servers
    // newline-terminate every chunk; this is defensive)
    if !buf.is_empty() {
        if let Ok(parsed) = serde_json::from_slice::<Value>(&buf) {
            decode_chunk_into(&parsed, &mut out);
        }
    }
    Ok(out)
}

fn decode_chunk_into(parsed: &Value, out: &mut Vec<OllamaChunk>) {
    let Some(msg) = parsed.get("message") else {
        return;
    };
    if let Some(content) = msg.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            out.push(OllamaChunk::Text(content.to_string()));
        }
    }
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let fnobj = call.get("function").cloned().unwrap_or(Value::Null);
            let name = fnobj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // ollama's `arguments` is a parsed dict — DO NOT re-parse.
            let arguments = fnobj.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(mint_tool_call_id);
            out.push(OllamaChunk::ToolCall {
                id,
                name,
                arguments,
            });
        }
    }
}

// ── verbs ───────────────────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let model = meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL);
    let endpoint = meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT);
    let file_agent_id = meta_string(agent_id, kernel, "file_agent_id");
    let generating = BACKENDS
        .lock()
        .get(agent_id)
        .map(|s| {
            s.current_task
                .lock()
                .expect("task poisoned")
                .as_ref()
                .map(|t| !t.is_finished())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    json!({
        "id": agent_id.as_str(),
        "sentence": "Ollama-backed LLM agent (native tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "file_agent_id": file_agent_id,
        "generating": generating,
        "verbs": {
            "reflect": "Identity + model + endpoint + generating flag + file_agent_id binding. No args.",
            "boot": "No-op. Returns null.",
            "shutdown": "Aborts any in-flight send and drops process-memory state. Returns {stopped:bool}.",
            "send": "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller. Per-backend FIFO lock. Returns {response, final, client_id}.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id} — that client's persisted chat.",
            "interrupt": "No args. Cancels any in-flight send. Returns {interrupted:bool}.",
            "refresh_menu": "No args. Drops the cached agent menu. Returns {refreshed:true}.",
            "status": "args: client_id:str?. Returns the in-flight/queue snapshot (text redacted for other clients).",
        },
        "emits": {
            "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...}} — phase transitions.",
            "token": "{type:'token', text:str, source, client_id} — one per streaming chunk.",
            "say": "{type:'say', text:'[tool target → reply]', source, client_id} — per tool_call summary.",
            "done": "{type:'done', source, client_id} — final event after streaming completes (or interrupt).",
        },
        "concurrency": "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait + receive a queued status event. reflect/history/interrupt/status skip the lock.",
    })
}

fn shutdown_reply(agent_id: &AgentId) -> Value {
    let state = BACKENDS.lock().remove(agent_id);
    if let Some(s) = state {
        if let Some(task) = s.current_task.lock().expect("task poisoned").take() {
            task.abort();
        }
    }
    Value::Null
}

fn interrupt_reply(agent_id: &AgentId) -> Value {
    let task_opt = {
        let map = BACKENDS.lock();
        map.get(agent_id)
            .and_then(|s| s.current_task.lock().expect("task poisoned").take())
    };
    if let Some(task) = task_opt {
        if !task.is_finished() {
            task.abort();
            return json!({"interrupted": true});
        }
    }
    json!({"interrupted": false})
}

fn refresh_menu_reply(agent_id: &AgentId) -> Value {
    let state = state_for(agent_id);
    *state.menu.lock().expect("menu poisoned") = None;
    json!({"refreshed": true})
}

fn redact_entry(c: &CurrentEntry, requesting: Option<&str>) -> Value {
    let is_mine = requesting
        .map(|r| r == c.client_id.as_str())
        .unwrap_or(false);
    let elapsed = (now_secs() - c.started_at).max(0.0);
    let mut out = json!({
        "client_id": c.client_id,
        "send_id": c.send_id,
        "started_at": c.started_at,
        "phase": c.phase,
        "elapsed": elapsed,
        "is_mine": is_mine,
    });
    if is_mine {
        let obj = out.as_object_mut().unwrap();
        obj.insert("text".to_string(), json!(c.text));
        obj.insert("text_so_far".to_string(), json!(c.text_so_far));
        if let Some(t) = &c.last_tool {
            obj.insert("last_tool".to_string(), t.clone());
        }
    }
    out
}

fn status_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let requesting = payload
        .get("client_id")
        .and_then(Value::as_str)
        .map(safe_client);
    let state = state_for(agent_id);
    let cur = state.current_meta.lock().expect("current poisoned").clone();
    let queue = state
        .queue
        .lock()
        .expect("queue poisoned")
        .iter()
        .cloned()
        .collect::<Vec<_>>();

    let current_out = cur
        .as_ref()
        .map(|c| redact_entry(c, requesting.as_deref()))
        .unwrap_or(Value::Null);

    let (mine_pending, others_pending) = match &requesting {
        Some(req) => {
            let mut mine = Vec::new();
            let mut others = 0u64;
            for q in queue.iter() {
                if q.client_id == *req {
                    mine.push(json!({
                        "send_id": q.send_id,
                        "text": q.text,
                        "queued_at": q.queued_at,
                    }));
                } else {
                    others += 1;
                }
            }
            (Value::Array(mine), others)
        }
        None => (Value::Array(Vec::new()), queue.len() as u64),
    };

    json!({
        "source": agent_id.as_str(),
        "client_id": requesting,
        "generating": cur.is_some(),
        "current": current_out,
        "mine_pending": mine_pending,
        "others_pending": others_pending,
    })
}

async fn history_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if meta_string(agent_id, kernel, "file_agent_id").is_none() {
        return json!({"error": "ollama_backend: file_agent_id required"});
    }
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    let messages = load_history_messages(agent_id, kernel, &client_id).await;
    json!({"messages": messages, "client_id": client_id})
}

// ── the send flow ───────────────────────────────────────────────────

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if meta_string(agent_id, kernel, "file_agent_id").is_none() {
        return json!({"error": "ollama_backend: file_agent_id required"});
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
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    let send_id = mint_send_id();
    let state = state_for(agent_id);

    // Enqueue.
    let entry = QueuedEntry {
        client_id: client_id.clone(),
        text: text.clone(),
        send_id: send_id.clone(),
        queued_at: now_secs(),
    };
    {
        state
            .queue
            .lock()
            .expect("queue poisoned")
            .push_back(entry.clone());
    }

    // Best-effort contention detection.
    let contested = {
        // If anyone holds the lock right now `try_lock` fails. Drop the
        // guard immediately so we don't poison the FIFO.
        state.lock.try_lock().is_err()
    };
    if contested {
        let ahead = state
            .queue
            .lock()
            .expect("queue poisoned")
            .len()
            .saturating_sub(1);
        // Back-compat `queued` envelope.
        to_caller(
            kernel,
            agent_id,
            &client_id,
            json!({"type": "queued", "source": agent_id.as_str(), "send_id": send_id}),
        )
        .await;
        let mut detail = Map::new();
        detail.insert("send_id".to_string(), json!(send_id));
        detail.insert("ahead".to_string(), json!(ahead));
        emit_status(kernel, &state, agent_id, &client_id, "queued", detail).await;
    }

    // Acquire FIFO lock.
    let lock_arc = Arc::clone(&state.lock);
    let _guard = lock_arc.lock_owned().await;

    // Pop ourselves from the queue, become the in-flight entry.
    {
        let mut q = state.queue.lock().expect("queue poisoned");
        if let Some(pos) = q.iter().position(|e| e.send_id == send_id) {
            q.remove(pos);
        }
    }
    {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        *cur = Some(CurrentEntry {
            client_id: client_id.clone(),
            text: text.clone(),
            send_id: send_id.clone(),
            started_at: now_secs(),
            phase: "thinking".to_string(),
            text_so_far: String::new(),
            last_tool: None,
        });
    }
    emit_status(kernel, &state, agent_id, &client_id, "thinking", Map::new()).await;

    // Spawn the streaming task so `interrupt` can abort it via JoinHandle.
    let agent_id_owned = agent_id.clone();
    let client_id_owned = client_id.clone();
    let text_owned = text.clone();
    let kernel_owned = Arc::clone(kernel);
    let state_owned = Arc::clone(&state);
    let task_result: Arc<AsyncMutex<Option<Result<String, String>>>> =
        Arc::new(AsyncMutex::new(None));
    let task_result_inner = Arc::clone(&task_result);

    let join: JoinHandle<()> = tokio::spawn(async move {
        let outcome = run_send(
            &agent_id_owned,
            &state_owned,
            &text_owned,
            &kernel_owned,
            &client_id_owned,
        )
        .await;
        *task_result_inner.lock().await = Some(outcome);
    });

    // Stash the JoinHandle so interrupt can abort it.
    {
        let mut t = state.current_task.lock().expect("task poisoned");
        // Drop any previous (should be None, defensive).
        if let Some(prev) = t.take() {
            prev.abort();
        }
        // We need a clone of the JoinHandle. tokio doesn't give us
        // that — but `interrupt` only needs `.abort()` which is via an
        // `AbortHandle`. Stash a no-op JoinHandle as a "running"
        // marker by re-spawning a watcher? Simpler: we re-architect to
        // keep the AbortHandle separately.
        // Workaround: we store the JoinHandle here directly, and use
        // tokio::join! to await its completion below (via take()).
        *t = Some(join);
    }
    let abort_handle = {
        let t = state.current_task.lock().expect("task poisoned");
        t.as_ref().map(|j| j.abort_handle())
    };

    // Wait for completion or timeout.
    let timeout = Duration::from_secs(SEND_TIMEOUT_SECS);
    let wait_outcome: Result<Option<Result<String, String>>, &'static str> = {
        // Sleep loop watching for completion OR timeout.
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Quick check.
            let finished = state
                .current_task
                .lock()
                .expect("task poisoned")
                .as_ref()
                .map(|j| j.is_finished())
                .unwrap_or(true);
            if finished {
                break Ok(task_result.lock().await.take());
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                if let Some(ah) = &abort_handle {
                    ah.abort();
                }
                // give the abort a moment to register
                tokio::time::sleep(Duration::from_millis(20)).await;
                break Err("timeout");
            }
            let step = std::cmp::min(
                Duration::from_millis(50),
                deadline.saturating_duration_since(now),
            );
            tokio::time::sleep(step).await;
        }
    };

    // Clear the task slot (drop the JoinHandle).
    let taken = state.current_task.lock().expect("task poisoned").take();
    drop(taken);

    let reply = match wait_outcome {
        Ok(Some(Ok(final_text))) => {
            json!({
                "response": final_text,
                "final": final_text,
                "client_id": client_id,
            })
        }
        Ok(Some(Err(e))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.clone()));
                m
            })
            .await;
            emit_done(kernel, agent_id, &client_id).await;
            json!({"error": e, "client_id": client_id})
        }
        Ok(None) => {
            // Task aborted (interrupt) — task_result never wrote.
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("interrupted"));
                m
            })
            .await;
            emit_done(kernel, agent_id, &client_id).await;
            json!({"response": "", "interrupted": true, "client_id": client_id})
        }
        Err(_) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("timeout"));
                m
            })
            .await;
            emit_done(kernel, agent_id, &client_id).await;
            json!({
                "error": format!("send: timeout after {}s", SEND_TIMEOUT_SECS),
                "client_id": client_id,
            })
        }
    };

    // Clear current_meta.
    *state.current_meta.lock().expect("current poisoned") = None;
    drop(_guard);
    reply
}

/// Core streaming loop. Runs inside the spawned task so `interrupt`
/// can abort cleanly. Returns the final assistant text on success.
async fn run_send(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Result<String, String> {
    let endpoint = meta_string_or(self_id, kernel, "endpoint", DEFAULT_ENDPOINT);
    let model = meta_string_or(self_id, kernel, "model", DEFAULT_MODEL);
    let mut messages = assemble_messages(self_id, state, user_text, kernel, client_id).await;
    let tools = vec![send_tool_def()];
    let mut last_text;
    let mut iteration = 0usize;
    loop {
        iteration += 1;
        if iteration > 1 {
            emit_status(kernel, state, self_id, client_id, "thinking", Map::new()).await;
        }
        let chunks = ollama_chat(&endpoint, &model, &messages, &tools).await?;
        let mut content_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<(String, String, Value)> = Vec::new();
        let mut first_text_chunk = true;
        for ch in chunks {
            match ch {
                OllamaChunk::Text(t) => {
                    if first_text_chunk {
                        first_text_chunk = false;
                        emit_status(kernel, state, self_id, client_id, "streaming", Map::new())
                            .await;
                    }
                    // Update text_so_far for status snapshots.
                    {
                        let mut cur = state.current_meta.lock().expect("current poisoned");
                        if let Some(c) = cur.as_mut() {
                            c.text_so_far.push_str(&t);
                        }
                    }
                    to_caller(
                        kernel,
                        self_id,
                        client_id,
                        json!({"type": "token", "text": t, "source": self_id.as_str()}),
                    )
                    .await;
                    content_parts.push(t);
                }
                OllamaChunk::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls.push((id, name, arguments));
                }
            }
        }
        last_text = content_parts.join("");

        if tool_calls.is_empty() {
            break;
        }

        // Record assistant turn with its tool_calls.
        let assistant_calls: Vec<Value> = tool_calls
            .iter()
            .map(|(id, name, args)| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args},
                })
            })
            .collect();
        messages.push(json!({
            "role": "assistant",
            "content": last_text,
            "tool_calls": assistant_calls,
        }));

        // Parallel-dispatch tool calls.
        let mut futures = Vec::new();
        for (id, name, args) in tool_calls.iter() {
            let target = args
                .get("target_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let payload = args.get("payload").cloned().unwrap_or(Value::Null);
            let id_owned = id.clone();
            let name_owned = name.clone();
            let args_owned = args.clone();
            let verb = payload
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_entry = json!({
                "call_id": id_owned,
                "target": target,
                "verb": verb,
                "args": args_owned,
            });
            {
                let mut cur = state.current_meta.lock().expect("current poisoned");
                if let Some(c) = cur.as_mut() {
                    c.last_tool = Some(tool_entry.clone());
                }
            }
            let mut entry_detail = Map::new();
            entry_detail.insert("tool".to_string(), tool_entry.clone());
            emit_status(
                kernel,
                state,
                self_id,
                client_id,
                "tool_calling",
                entry_detail,
            )
            .await;
            let kernel_clone = Arc::clone(kernel);
            let target_clone = target.clone();
            let self_id_clone = self_id.clone();
            let client_id_clone = client_id.to_string();
            let state_clone = Arc::clone(state);
            let payload_clone = payload.clone();
            futures.push(async move {
                let reply = if target_clone.is_empty() {
                    json!({"error": "empty target_id"})
                } else {
                    kernel_clone
                        .send(&AgentId::from(target_clone.as_str()), payload_clone)
                        .await
                };
                let reply_str = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".to_string());
                let preview: String = reply_str.chars().take(120).collect();
                let tool_entry_done = json!({
                    "call_id": id_owned,
                    "target": target_clone,
                    "verb": verb,
                    "args": args_owned,
                    "reply_preview": preview,
                });
                {
                    let mut cur = state_clone.current_meta.lock().expect("current poisoned");
                    if let Some(c) = cur.as_mut() {
                        c.last_tool = Some(tool_entry_done.clone());
                    }
                }
                let mut exit_detail = Map::new();
                exit_detail.insert("tool".to_string(), tool_entry_done);
                emit_status(
                    &kernel_clone,
                    &state_clone,
                    &self_id_clone,
                    &client_id_clone,
                    "tool_calling",
                    exit_detail,
                )
                .await;
                to_caller(
                    &kernel_clone,
                    &self_id_clone,
                    &client_id_clone,
                    json!({
                        "type": "say",
                        "text": format!("[tool {} → {}]", target_clone, preview),
                        "source": self_id_clone.as_str(),
                    }),
                )
                .await;
                json!({
                    "role": "tool",
                    "tool_call_id": id_owned,
                    "name": name_owned,
                    "content": reply_str,
                })
            });
        }
        let results = futures_util::future::join_all(futures).await;
        // Menu invalidates AFTER each tool batch.
        *state.menu.lock().expect("menu poisoned") = None;
        messages.extend(results);
    }

    // Done.
    emit_status(kernel, state, self_id, client_id, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        m
    })
    .await;
    emit_done(kernel, self_id, client_id).await;

    // Append final assistant turn + persist.
    messages.push(json!({"role": "assistant", "content": last_text}));
    // Persist everything except the system block.
    let to_persist: Vec<Value> = messages.iter().skip(1).cloned().collect();
    let _ = save_history_messages(self_id, kernel, client_id, &to_persist).await;
    Ok(last_text)
}

#[cfg(test)]
mod tests;
