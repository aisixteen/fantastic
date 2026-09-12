//! nvidia_nim_backend — NVIDIA NIM (OpenAI-compatible) LLM bundle.
//! Thin shell over `fantastic-ai-core`: this crate supplies only the NIM
//! `Provider` (HTTPS + Bearer + SSE + BOTH text channels + 429 retry) + a
//! `Bundle` that dispatches every verb through ai-core. The FIFO lock, menu
//! cache, prompt assembly, agentic loop (incl. RAW tool-call parsing + reply
//! routing), status phase machine, and history persistence live in ai-core. NO
//! native tools — Fantastic NEVER sends OpenAI `tools`/`tool_choice` nor reads
//! `delta.tool_calls`; the model emits a `<call name=…>` envelope in the text.
//!
//! A reasoning model streams its deliberation as `delta.reasoning_content` and
//! only its conclusion as `delta.content`; both are read and the deliberation is
//! wrapped in `<think>…</think>` once per turn (see `fantastic_ai_core::think`).
//!
//! See `fantastic_ai_core`'s module header for the canonical LLM backend
//! contract. NIM-local extras:
//!
//! - the key is read from the ENVIRONMENT (`NVIDIA_API_KEY`) at call time.
//!   There is no key-storage verb and no sidecar: a connector is inert until
//!   the operator wires the variable, and an absent key fails loud NAMING it.
//! - Bearer HTTP client cache (dropped on key change), 429 rate-limit retry.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::events::{emit_status, to_caller, CallerRoute};
use fantastic_ai_core::provider::{Provider, ProviderEvent, ProviderStream};
use fantastic_ai_core::retrieval;
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "nvidia_nim_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default NIM endpoint — OpenAI-compatible.
pub const DEFAULT_ENDPOINT: &str = "https://integrate.api.nvidia.com/v1";

/// NO guessy default (by default, no defaults): never name a network-reaching
/// model. The sentinel exists only so reflect can say what's true; the request
/// path fails loud when the model is unset/sentinel.
pub const DEFAULT_MODEL: &str = "(no default — set the `model` meta)";

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

/// Per-backend config: per-client-inbox routing, serial tool dispatch.
const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::PerClientInbox,
    name: "nvidia_nim_backend",
};

// ── NIM-local: Bearer HTTP client cache ─────────────────────────────

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

/// An error WITH ITS CAUSES — reqwest's `Display` prints only the outer layer
/// ("error sending request for url"), which hid whether a dead request was DNS,
/// TLS, a reset or a timeout across 29 e2e logs. Python's exception text names
/// the cause; this walks `source()` so rust's does too.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(" ← ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

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
        // NO IDLE CONNECTION REUSE — python opens a fresh `httpx.AsyncClient`
        // per send, so every request rides a new connection. This client is
        // cached per agent, and a pooled keep-alive connection the proxy has
        // closed idle fails the next POST with "error sending request" (hyper
        // does not retry a non-idempotent request): 29 of 620 rust e2e logs
        // died that way at a test's first request, against 10 of 1131 python
        // logs (measured 2026-09-12).
        .pool_max_idle_per_host(0)
        // HTTP/1.1, as python's httpx speaks it. reqwest negotiates HTTP/2 over
        // ALPN by default, and through the CloudFront proxy the h2 streams
        // died mid-request with "error sending request" / "error decoding
        // response body" even with idle reuse off (2 more in run
        // 20260912-195846). One protocol on both runtimes, no guessing.
        .http1_only()
        .build()
        .map_err(|e| format!("build client: {e}"))
}

/// The key comes from the ENVIRONMENT, so no kernel handle is needed here any
/// more — this used to read a sidecar through the file agent.
fn get_or_build_client(self_id: &AgentId) -> Result<Arc<reqwest::Client>, String> {
    if let Some(c) = HTTP_CLIENTS.lock().get(self_id).cloned() {
        return Ok(c);
    }
    let Some(key) = read_api_key() else {
        return Err(no_key_error());
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

// ── the api key: ENVIRONMENT ONLY ───────────────────────────────────

/// The variable that carries the key. Named in the failure text so an unwired
/// connector says exactly what to set.
const KEY_VAR: &str = "NVIDIA_API_KEY";

/// Read the key from the environment. No sidecar, no agent record — the key is
/// never written to disk by this bundle and never appears in `agent.json`.
fn read_api_key() -> Option<String> {
    std::env::var(KEY_VAR)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The refusal for an unwired connector — names the variable to set.
fn no_key_error() -> String {
    format!("nvidia_nim_backend: no api key — set {KEY_VAR}")
}

// ── NIM provider (Bearer + SSE + 429 retry) ─────────────────────────

/// Built per `send`; captures the kernel + client id + route so it can
/// emit the rate-limit notices during a 429 backoff before yielding the
/// retried stream.
struct NimProvider {
    self_id: AgentId,
    endpoint: String,
    model: String,
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

#[async_trait]
impl Provider for NimProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String> {
        // NO guessy default: fail loud before the wire if no model is named.
        if self.model.is_empty() || self.model == DEFAULT_MODEL {
            return Err("nvidia_nim_backend: no `model` set — this connector does \
                not guess one. Name a model on the agent record (model=...)."
                .to_string());
        }
        // RAW: no native `tools`/`tool_choice` — ai-core parses the `<tool_call>`
        // envelope out of the streamed text.
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
        });
        // Retry-once-on-429 (before any chunk yielded). Emits a
        // rate-limit say + status during the backoff.
        let mut attempt: u32 = 0;
        let resp = loop {
            let client = get_or_build_client(&self.self_id)?;
            let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("http: {}", error_chain(&e)))?;
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

        // Drain the SSE body fully, parse, finalize tool-calls.
        let events = consume_sse(resp).await?;
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

/// Drain the SSE body, split into `data:` lines, parse JSON, emit content
/// tokens in order. PURE RAW TEXT — no native `tool_calls` are read; ai-core
/// parses the `<call name=…>` envelope out of the token text.
async fn consume_sse(
    resp: reqwest::Response,
) -> Result<Vec<Result<ProviderEvent, String>>, String> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<Result<ProviderEvent, String>> = Vec::new();
    let mut done_marker_seen = false;
    // ONE wrapper for the whole pass — the reasoning span is per TURN.
    let mut wrap = ThinkWrap::new();

    while let Some(chunk_res) = stream.next().await {
        let bytes = match chunk_res {
            Ok(b) => b,
            Err(e) => return Err(format!("stream: {}", error_chain(&e))),
        };
        buf.extend_from_slice(&bytes);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
            let line_owned = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            let line = line_owned.as_str();
            if line.is_empty() || !line.starts_with("data:") {
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
            decode_delta_into(&delta, &mut wrap, &mut out);
        }
        if done_marker_seen {
            break;
        }
    }

    // The turn ended while still reasoning (content never started) — close the
    // span exactly once so the deliberation is strippable.
    if let Some(m) = wrap.close_marker() {
        out.push(Ok(ProviderEvent::Token(m.to_string())));
    }

    Ok(out)
}

/// Decode ONE OpenAI-style streaming `delta` into content tokens, reading BOTH
/// text channels. `wrap` carries the per-TURN reasoning span across deltas.
///
/// REASONING MODELS SPLIT THEIR OUTPUT. nemotron/deepseek-style NIM models stream
/// most tokens as `reasoning_content` and only the conclusion as `content` —
/// measured 740 chars of reasoning to 129 of content on one turn. Reading
/// `content` alone silently drops whole turns: observed live (2026-08-02), the
/// model spent an entire pass reasoning, we saw an EMPTY completion, and the
/// chain died with no error anywhere. A raw text protocol reads the text the
/// model produced; it does not let the provider decide which field counts as
/// output. WRAPPED once (same as the ollama backend, see
/// `fantastic_ai_core::think`): parsed for tool calls, stripped before the turn
/// is persisted.
fn decode_delta_into(
    delta: &Value,
    wrap: &mut ThinkWrap,
    out: &mut Vec<Result<ProviderEvent, String>>,
) {
    // DELEGATES to the shared decode: NIM and `llama-server` speak the same
    // wire, and two copies would drift on the one thing this gets right —
    // reading BOTH channels, not just `content`.
    for piece in fantastic_ai_core::think::decode_openai_delta(delta, wrap) {
        out.push(Ok(ProviderEvent::Token(piece)));
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
            "shutdown" => Value::Null,
            // BOOT WITH A STORE RUNS `context_recall` — python `_make_boot`.
            "boot" => {
                if fantastic_ai_core::history::load_history(
                    agent_id,
                    kernel,
                    helpers::DEFAULT_CLIENT_ID,
                )
                .await
                .is_empty()
                {
                    Value::Null
                } else {
                    let p = build_provider(agent_id, kernel, helpers::DEFAULT_CLIENT_ID);
                    retrieval::context_recall(agent_id, &json!({}), kernel, p.as_ref()).await
                }
            }
            "send" => send_reply(agent_id, payload, kernel).await,
            // ONE ASK VERB, WHOEVER IS ASKED: a person answers `inquiry` from a
            // record, this agent answers it by THINKING, and the asker writes
            // the same call either way.
            "inquiry" => match verbs::inquiry_open(payload) {
                Err(refusal) => refusal,
                Ok(sp) => verbs::inquiry_close(send_reply(agent_id, &sp, kernel).await, payload),
            },
            "history" => verbs::history(agent_id, payload, kernel).await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            // FULL ASYNC: page a stored reply body by pid, or FIND the line first.
            "pull" => retrieval::pull(agent_id, payload, kernel).await,
            "grep" => retrieval::grep(agent_id, payload, kernel).await,
            // Rebuild the view from the store: on boot, after folds, on request.
            "context_recall" => {
                let p = build_provider(agent_id, kernel, helpers::DEFAULT_CLIENT_ID);
                retrieval::context_recall(agent_id, payload, kernel, p.as_ref()).await
            }
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            other => json!({"error": format!("nvidia_nim_backend: unknown type {other:?}")}),
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

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate drops
    /// the future (which frees the SSE stream + the FIFO guard by itself).
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel, CFG.route).await;
        Ok(())
    }
}

/// The provider this agent is configured for. Built per call (it is cheap and the
/// record can change between turns), and shared by `send` and the verbs that need
/// a completion of their own — `context_recall` asks the model which rows to keep
/// in full. `None` when the backend is not configured to reach a model at all;
/// the caller says so in its own words rather than guessing a provider.
fn build_provider(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Option<Arc<dyn Provider>> {
    read_api_key()?;
    Some(Arc::new(NimProvider {
        self_id: agent_id.clone(),
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
        kernel: Arc::clone(kernel),
        client_id: client_id.to_string(),
    }))
}

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let client_id = helpers::safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let Some(provider) = build_provider(agent_id, kernel, &client_id) else {
        return json!({ "error": no_key_error() });
    };
    verbs::send(provider, agent_id, payload, kernel, CFG).await
}

async fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let endpoint = helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT);
    let model = helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL);
    let file_bridge_id_v = helpers::meta_string(agent_id, kernel, "file_bridge_id");
    let has_key = read_api_key().is_some();
    let generating = state::is_generating(agent_id);
    let meta = helpers::agent_meta(agent_id, kernel);
    let context_window = fantastic_ai_core::context::resolve_context_window(&meta);
    let context_strategy = meta
        .get("context_strategy")
        .and_then(Value::as_str)
        .unwrap_or("compact")
        .to_string();
    json!({
        "id": agent_id.as_str(),
        "sentence": "NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "file_bridge_id": file_bridge_id_v,
        "has_api_key": has_key,
        "generating": generating,
        "context_window": context_window,
        "context_strategy": context_strategy,
        "verbs": {
            "reflect": "Identity + model + endpoint + has_api_key + generating + file_bridge_id binding. No args. `has_api_key` reports whether the ENV key is set; the value is NEVER returned.",
            "boot": "No-op. Returns null.",
            "send": "args: text:str (req), client_id:str? (default 'cli'), system_prompt:str? (REPLACES the default system block), send_to:str? (hands this pass's TEXT to that agent id once, no propagation). Streams tokens to ONLY the caller. ONE provider pass; the model routes its own work with `reply_to` inside its <tool_call>s (default: back to itself). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Failfast if file_bridge_id unset OR no api key in the environment.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id}.",
            "recall": "args: client_id:str?, query:str?, limit:int?, before:int?. Pages turns back from the durable store (lossless on demand after compaction). Returns {messages, total, truncated, client_id}.",
            "pull": "args: pid:int (req — the pid on a settled reply's tag), offset:int? (0), length:int? (chars; default and max = your inline cap). Reads ONE PAGE of a reply body that was too large to inline. Returns {pid, offset, text, next_offset, eof} — page with offset: next_offset until eof.",
            "grep": "args: query:str (req — case-insensitive substring), ids:list[int]? (row ids / pids / fold ids to search; default: every row of this conversation and every stored reply body it received), limit:int?, width:int? (chars of each matched line, default 200). Finds the LINE that mentions something, wherever it is. Returns {matches:[{id, line, text}], total, truncated}.",
            "context_recall": "args: dry:bool? (mechanical fit only — no model call). Rebuilds this agent's VIEW from its store: fits the head to the budget at digest resolution, asks the model which rows it wants in full, persists the id list, logs the decision as a row. Returns {kept, expanded, dropped, budget, why, degraded}.",
            "context_status": "No args. Context-budget posture + last compaction + derived reaction. Returns {context_window, output_reserve, budget, strategy, last_projection, last_reaction}.",
            "status": "args: client_id:str?. Privacy-filtered queue/in-flight snapshot.",
        },
        "cancel": fantastic_ai_core::CANCEL_DOC,
        "emits": {
            "queued": "{type:'queued', source, client_id, send_id} — back-compat queue notice.",
            "token": "{type:'token', text, source, client_id} — streaming chunk.",
            "say": "{type:'say', text, source, client_id} — per call summary plus rate-limit notices.",
            "status": "{type:'status', source, client_id, ts, phase, detail} — phase machine: queued|thinking|streaming|tool_calling|done. detail.waiting_on='rate_limit' + wait_s during 429 backoff.",
            "done": "{type:'done', source, client_id} — back-compat end marker.",
            "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the Context Protocol push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized}. too_small: detail={context_window, system_tokens, hint} (model NOT called). Pull counterpart: the context_status verb.",
        },
        "concurrency": "Per-backend FIFO lock around `send`. reflect/history skip the lock.",
    })
}

#[cfg(test)]
mod tests;
