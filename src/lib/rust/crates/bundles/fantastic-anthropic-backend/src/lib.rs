//! anthropic_backend — Anthropic (Claude) Messages-API LLM bundle.
//! Thin shell over `fantastic-ai-core`: this crate supplies only the
//! Anthropic `Provider` (HTTPS + `x-api-key` + `anthropic-version`
//! header + event-typed SSE + `tool_use`-block aggregation + the
//! OpenAI↔Anthropic message/tool translation + 429 retry) plus a
//! `Bundle` that dispatches every verb through ai-core. The FIFO lock,
//! menu cache, prompt assembly, agentic loop, status phase machine, and
//! history persistence live in ai-core.
//!
//! See `fantastic_ai_core`'s module header for the canonical LLM backend
//! contract. Anthropic-local extras:
//!
//! - `set_api_key` args `{api_key:str}` → persists to sidecar via file agent
//! - `clear_api_key` → deletes the sidecar
//! - `reflect` includes `has_api_key: bool` (never the value)
//! - `x-api-key` HTTP client cache (dropped on key change), 429 retry.
//!
//! Wire translation (ai-core speaks OpenAI; Anthropic differs):
//! - `system` role → top-level `system` param (concatenated)
//! - `assistant.tool_calls` → `tool_use` content blocks
//! - `role:tool` result → a `user` msg with a `tool_result` block
//! - `{type:function, function:{name,description,parameters}}` tool →
//!   `{name, description, input_schema}`
//! - response `content_block_delta`(`text_delta`/`input_json_delta`) +
//!   `tool_use` blocks → ai-core `ProviderEvent::Token` / `ToolCall`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::events::{emit_status, to_caller, CallerRoute};
use fantastic_ai_core::provider::{Provider, ProviderEvent, ProviderStream};
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "anthropic_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default Anthropic API base — the Messages API lives at `/messages`.
pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1";

/// Default model (a current, cost-reasonable Claude).
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` is REQUIRED by the Messages API — default if unset on the meta.
pub const DEFAULT_MAX_TOKENS: u64 = 4096;

/// Headless / REPL caller default.
pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;

/// Hard per-generation ceiling (seconds). Releases the FIFO lock.
pub const SEND_TIMEOUT_SECS: u64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

/// Max wait honored from a `Retry-After` header on 429.
pub const RATE_LIMIT_MAX_WAIT_SECS: u64 = 60;

/// Default wait when `Retry-After` is absent / unparseable.
pub const RATE_LIMIT_DEFAULT_WAIT_SECS: u64 = 5;

/// Per-agent retry budget on 429 before any chunk has been yielded.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 1;

/// Per-backend config: per-client-inbox routing; Anthropic `tool_use.input`
/// is a JSON OBJECT (not a string), so the replayed assistant `tool_calls`
/// carry the object directly (`tool_args_as_json: false`); serial dispatch.
const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::PerClientInbox,
    tool_args_as_json: false,
    parallel_tools: false,
    name: "anthropic_backend",
};

// ── Anthropic-local: x-api-key HTTP client cache ────────────────────

static HTTP_CLIENTS: OnceLockHttpMap = OnceLockHttpMap::new();

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

fn build_client(api_key: &str) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    let key_hv = reqwest::header::HeaderValue::from_str(api_key)
        .map_err(|e| format!("api_key has illegal header chars: {e}"))?;
    headers.insert("x-api-key", key_hv);
    headers.insert(
        "anthropic-version",
        reqwest::header::HeaderValue::from_static(ANTHROPIC_VERSION),
    );
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

// ── Anthropic-local: api_key sidecar ────────────────────────────────

fn key_path(self_id: &AgentId) -> String {
    // Store-relative (`agents/<id>/…`) — wire `file_bridge_id` to the `.fantastic`
    // store; lands next to the agent's agent.json. Matches Python + NIM.
    format!("agents/{}/api_key", self_id)
}

async fn read_api_key(self_id: &AgentId, kernel: &Arc<Kernel>) -> Option<String> {
    let raw = helpers::file_read(self_id, kernel, &key_path(self_id)).await?;
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

async fn set_api_key_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if helpers::file_bridge_id(agent_id, kernel).is_none() {
        return json!({"error": "anthropic_backend: file_bridge_id required"});
    }
    let key = payload.get("api_key").and_then(Value::as_str).unwrap_or("");
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return json!({"error": "set_api_key: api_key must be a non-empty string"});
    }
    if let Err(e) = helpers::file_write(agent_id, kernel, &key_path(agent_id), trimmed).await {
        return json!({"error": format!("set_api_key: file write failed: {e}")});
    }
    drop_cached_client(agent_id);
    json!({"ok": true})
}

async fn clear_api_key_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    if helpers::file_bridge_id(agent_id, kernel).is_none() {
        return json!({"error": "anthropic_backend: file_bridge_id required"});
    }
    let deleted = helpers::file_delete(agent_id, kernel, &key_path(agent_id)).await;
    drop_cached_client(agent_id);
    json!({"ok": true, "deleted": deleted})
}

// ── OpenAI → Anthropic translation ──────────────────────────────────

/// Coerce a `function.arguments` field (string OR object) into a JSON object.
fn args_to_object(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

/// Split ai-core's OpenAI-shaped messages into Anthropic's top-level
/// `system` string + the `messages` array (with `tool_use` / `tool_result`
/// content blocks). System blocks are concatenated; empty assistant turns
/// (no text, no tool_calls) are dropped (Anthropic rejects empty content).
fn translate_messages(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system: Option<String> = None;
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" => {
                let c = m.get("content").and_then(Value::as_str).unwrap_or("");
                system = Some(match system.take() {
                    Some(prev) if !prev.is_empty() => format!("{prev}\n\n{c}"),
                    _ => c.to_string(),
                });
            }
            "user" => {
                let c = m.get("content").and_then(Value::as_str).unwrap_or("");
                out.push(json!({"role": "user", "content": c}));
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = m.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                        let func = tc.get("function").cloned().unwrap_or(Value::Null);
                        let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                        let input = args_to_object(func.get("arguments"));
                        blocks.push(json!({
                            "type": "tool_use", "id": id, "name": name, "input": input,
                        }));
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => {
                let tool_use_id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                let content = m.get("content").and_then(Value::as_str).unwrap_or("");
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                    }],
                }));
            }
            _ => {}
        }
    }
    (system, out)
}

/// `{type:function, function:{name, description, parameters}}` →
/// `{name, description, input_schema}`.
fn translate_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name").and_then(Value::as_str)?;
            let description = func
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": schema,
            }))
        })
        .collect()
}

// ── Anthropic provider (x-api-key + event-typed SSE + 429 retry) ─────

struct AnthropicProvider {
    self_id: AgentId,
    endpoint: String,
    model: String,
    max_tokens: u64,
    kernel: Arc<Kernel>,
    client_id: String,
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match raw.trim().parse::<u64>() {
        Ok(v) => v.clamp(1, RATE_LIMIT_MAX_WAIT_SECS),
        Err(_) => RATE_LIMIT_DEFAULT_WAIT_SECS,
    }
}

/// One pending (streamed) `tool_use` block, keyed by content-block index.
/// `input` arrives as `partial_json` STRING fragments that JSON-parse only
/// once concatenated.
#[derive(Default)]
struct PendingToolUse {
    id: String,
    name: String,
    partial_json: String,
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: &[Value], tools: &[Value]) -> Result<ProviderStream, String> {
        let (system, anthropic_messages) = translate_messages(messages);
        let anthropic_tools = translate_tools(tools);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(self.max_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("messages".into(), json!(anthropic_messages));
        if let Some(sys) = system {
            body.insert("system".into(), json!(sys));
        }
        if !anthropic_tools.is_empty() {
            body.insert("tools".into(), json!(anthropic_tools));
            body.insert("tool_choice".into(), json!({"type": "auto"}));
        }
        let body = Value::Object(body);

        // Retry-once-on-429 (before any chunk yielded). Emits a rate-limit
        // say + status during the backoff.
        let mut attempt: u32 = 0;
        let resp = loop {
            let client = get_or_build_client(&self.self_id, &self.kernel).await?;
            let url = format!("{}/messages", self.endpoint.trim_end_matches('/'));
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("http: {e}"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt < RATE_LIMIT_MAX_RETRIES {
                    let wait = parse_retry_after(resp.headers());
                    attempt += 1;
                    drop(resp);
                    to_caller(
                        &self.kernel,
                        &self.self_id,
                        &self.client_id,
                        CFG.route,
                        json!({
                            "type": "say",
                            "text": format!("[provider rate limited (429); waiting {wait}s]"),
                            "source": self.self_id.as_str(),
                        }),
                    )
                    .await;
                    let mut extras = Map::new();
                    extras.insert("waiting_on".into(), json!("rate_limit"));
                    extras.insert("wait_s".into(), json!(wait));
                    emit_status(
                        &self.kernel,
                        &state::state_for(&self.self_id),
                        &self.self_id,
                        &self.client_id,
                        CFG.route,
                        "thinking",
                        extras,
                    )
                    .await;
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
            break resp;
        };

        let events = consume_sse(resp).await?;
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

/// Drain the event-typed SSE body. Anthropic frames each event as an
/// `event:` line + a `data:` JSON line; the JSON carries its own `type`,
/// so we dispatch on that and ignore the `event:` lines. Tokens
/// (`text_delta`) and finalized tool-calls (one per `tool_use` block,
/// closed at its `content_block_stop`) are pushed in arrival order.
async fn consume_sse(
    resp: reqwest::Response,
) -> Result<Vec<Result<ProviderEvent, String>>, String> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<Result<ProviderEvent, String>> = Vec::new();
    // content-block index → pending tool_use (text blocks aren't tracked).
    let mut pending: HashMap<u64, PendingToolUse> = HashMap::new();
    let mut stop = false;

    while let Some(chunk_res) = stream.next().await {
        let bytes = match chunk_res {
            Ok(b) => b,
            Err(e) => return Err(format!("stream: {e}")),
        };
        buf.extend_from_slice(&bytes);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            if !line.starts_with("data:") {
                continue;
            }
            let data = line[5..].trim();
            if data.is_empty() {
                continue;
            }
            let obj: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                "content_block_start" => {
                    let idx = obj.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let block = obj.get("content_block").cloned().unwrap_or(Value::Null);
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        pending.insert(
                            idx,
                            PendingToolUse {
                                id: id.to_string(),
                                name: name.to_string(),
                                partial_json: String::new(),
                            },
                        );
                    }
                }
                "content_block_delta" => {
                    let idx = obj.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let delta = obj.get("delta").cloned().unwrap_or(Value::Null);
                    match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text_delta" => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    out.push(Ok(ProviderEvent::Token(t.to_string())));
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(slot) = pending.get_mut(&idx) {
                                if let Some(pj) = delta.get("partial_json").and_then(Value::as_str)
                                {
                                    slot.partial_json.push_str(pj);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let idx = obj.get("index").and_then(Value::as_u64).unwrap_or(0);
                    if let Some(slot) = pending.remove(&idx) {
                        if !slot.name.is_empty() {
                            let args: Value = if slot.partial_json.is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&slot.partial_json).unwrap_or(json!({}))
                            };
                            let id = if slot.id.is_empty() {
                                format!("toolu_{:08x}", idx)
                            } else {
                                slot.id
                            };
                            out.push(Ok(ProviderEvent::ToolCall {
                                id,
                                name: slot.name,
                                args,
                            }));
                        }
                    }
                }
                "error" => {
                    let msg = obj
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("anthropic stream error");
                    return Err(format!("stream: {msg}"));
                }
                "message_stop" => {
                    stop = true;
                    break;
                }
                _ => {} // message_start, message_delta, ping — ignored
            }
        }
        if stop {
            break;
        }
    }
    Ok(out)
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The Anthropic (Claude) backend bundle.
pub struct AnthropicBundle;

#[async_trait]
impl Bundle for AnthropicBundle {
    fn name(&self) -> &str {
        "anthropic_backend"
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
            "history" => verbs::history(agent_id, payload, kernel, "anthropic_backend").await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "interrupt" => verbs::interrupt(agent_id),
            "refresh_menu" => verbs::refresh_menu(agent_id),
            "set_api_key" => set_api_key_reply(agent_id, payload, kernel).await,
            "clear_api_key" => clear_api_key_reply(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            other => json!({"error": format!("anthropic_backend: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        drop_cached_client(agent_id);
        state::drop_state(agent_id);
        Ok(())
    }
}

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if helpers::file_bridge_id(agent_id, kernel).is_none() {
        return json!({"error": "anthropic_backend: file_bridge_id required"});
    }
    if !has_api_key(agent_id, kernel).await {
        return json!({"error": "anthropic_backend: api_key not set; call set_api_key first"});
    }
    let client_id = helpers::safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let max_tokens = helpers::agent_meta(agent_id, kernel)
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let provider: Arc<dyn Provider> = Arc::new(AnthropicProvider {
        self_id: agent_id.clone(),
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
        max_tokens,
        kernel: Arc::clone(kernel),
        client_id,
    });
    verbs::send(provider, agent_id, payload, kernel, CFG).await
}

async fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let endpoint = helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT);
    let model = helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL);
    let file_bridge_id_v = helpers::meta_string(agent_id, kernel, "file_bridge_id");
    let has_key = has_api_key(agent_id, kernel).await;
    let generating = state::is_generating(agent_id);
    let meta = helpers::agent_meta(agent_id, kernel);
    let max_tokens = meta
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let context_window = fantastic_ai_core::context::resolve_context_window(&meta);
    let context_strategy = meta
        .get("context_strategy")
        .and_then(Value::as_str)
        .unwrap_or("compact")
        .to_string();
    json!({
        "id": agent_id.as_str(),
        "sentence": "Anthropic (Claude) Messages-API LLM agent (native tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "max_tokens": max_tokens,
        "file_bridge_id": file_bridge_id_v,
        "has_api_key": has_key,
        "generating": generating,
        "context_window": context_window,
        "context_strategy": context_strategy,
        "verbs": {
            "reflect": "Identity + model + endpoint + max_tokens + has_api_key + generating + file_bridge_id binding. No args. The api_key value itself is NEVER returned — only the boolean.",
            "boot": "No-op. Returns null.",
            "send": "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller. Failfast if file_bridge_id unset OR api_key not set.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id}.",
            "recall": "args: client_id:str?, query:str?, limit:int?, before:int?. Pages turns back from the durable store (lossless on demand after compaction). Returns {messages, total, truncated, client_id}.",
            "context_status": "No args. Context-budget posture + last compaction + derived reaction. Returns {context_window, output_reserve, budget, strategy, last_projection, last_reaction}.",
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
            "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the Context Protocol push half. Pull counterpart: the context_status verb.",
        },
        "concurrency": "Per-backend FIFO lock around `send`. reflect/history/interrupt/set_api_key/clear_api_key skip the lock.",
    })
}

#[cfg(test)]
mod tests;
