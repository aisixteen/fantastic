//! anthropic_backend — Anthropic (Claude) Messages-API LLM bundle.
//! Thin shell over `fantastic-ai-core`: this crate supplies only the
//! Anthropic `Provider` (HTTPS + `x-api-key` + `anthropic-version`
//! header + event-typed SSE, RAW TEXT) plus a `Bundle` that
//! dispatches every verb through ai-core. The FIFO lock, menu cache, prompt
//! assembly, agentic loop (incl. RAW tool-call parsing), status phase machine,
//! and history persistence live in ai-core.
//!
//! See `fantastic_ai_core`'s module header for the canonical LLM backend
//! contract. Anthropic-local extras:
//!
//! - the key is read from the ENVIRONMENT (`ANTHROPIC_KEY`, else
//!   `ANTHROPIC_API_KEY`) at call time — python-canonical. There is no
//!   key-storage verb and no sidecar: a connector is inert until the operator
//!   wires the variable, and an absent key fails loud NAMING it.
//! - `x-api-key` HTTP client cache (dropped on key change). NO 429 retry and no
//!   event for one: python's provider has no rate-limit policy at all — a 429 is
//!   a status the endpoint ANSWERED with, and it is returned as `anthropic 429:
//!   <body>` like every other client error.
//!
//! Wire translation: ai-core's plain messages → Anthropic's top-level `system`
//! param + alternating user/assistant TEXT turns. NO native tools — Fantastic
//! NEVER uses Anthropic `tool_use`; tool-calling is RAW prompt-and-parse owned
//! by ai-core (the model emits a `<call name=…>` envelope in the text). Response
//! `content_block_delta`(`text_delta` + `thinking_delta`) → `ProviderEvent::Token`;
//! the reasoning channel is wrapped once per turn in `<think>…</think>`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use fantastic_ai_core::retrieval;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle::{py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "anthropic_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default Anthropic API base — the Messages API lives at `/messages`.
/// The Anthropic messages URL, used VERBATIM — python parity.
///
/// This used to be a BASE that the provider appended `/messages` to, while
/// python's `DEFAULT_ENDPOINT` was the full URL its provider POSTs as-is. One
/// meta key, `endpoint`, meant two different things depending on the runtime,
/// so a value that worked against a proxy on one kernel 404'd on another.
/// Python is canonical: the value is the URL.
pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// NO guessy default (by default, no defaults): this connector never names a
/// model that reaches the network. The sentinel exists only so reflect can say
/// what's true; the request path fails loud when the model is unset/sentinel.
pub const DEFAULT_MODEL: &str = "(no default — set the `model` meta)";

/// What `reflect` says this agent IS — python `anthropic_backend/tools.py`'s
/// `sentence`, verbatim. Native tool-calling is NOT used: ai_core parses the
/// `<call>` envelope out of the raw stream.
pub const SENTENCE: &str = "Anthropic-backed LLM agent (raw prompt-and-parse tool-calling).";

/// Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` is REQUIRED by the Messages API — default if unset on the meta.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// EDGE TRANSPORT FAILURES — not answers, and not statements about the request.
/// A 4xx, or any 5xx carrying a JSON body, is the endpoint DELIBERATELY
/// answering: that is the caller's bug and it stays loud. These codes with a
/// NON-JSON body are a CDN/proxy error page, meaning the request never reached
/// anything that could judge it. Python `provider.py`'s `TRANSPORT_CODES`.
const TRANSPORT_CODES: [u16; 8] = [502, 503, 504, 520, 521, 522, 523, 524];

/// Attempts spent on a transport code before giving up — python `RETRIES`.
const RETRIES: u32 = 2;

/// Seconds slept between transport attempts, multiplied by the attempt number
/// (1.5s, then 3.0s) — python `BACKOFF`.
const BACKOFF: f64 = 1.5;

/// Headless / REPL caller default.
pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;

/// Hard per-generation ceiling (seconds). Releases the FIFO lock.
pub const SEND_TIMEOUT_SECS: f64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

/// Per-backend config: per-client-inbox routing; serial dispatch.
const CFG: BackendConfig = BackendConfig {
    name: "anthropic_backend",
    provider_missing_error: None,
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
/// more — this used to read a sidecar through the file agent. The key is passed
/// in because `chat()` has already read and REFUSED on it (python checks it
/// there, first thing); this only builds the transport.
fn get_or_build_client(self_id: &AgentId, key: &str) -> Result<Arc<reqwest::Client>, String> {
    if let Some(c) = HTTP_CLIENTS.lock().get(self_id).cloned() {
        return Ok(c);
    }
    let client = Arc::new(build_client(key)?);
    HTTP_CLIENTS
        .lock()
        .insert(self_id.clone(), Arc::clone(&client));
    Ok(client)
}

fn drop_cached_client(self_id: &AgentId) {
    HTTP_CLIENTS.lock().remove(self_id);
}

// ── the api key: ENVIRONMENT ONLY ───────────────────────────────────

/// The variables that carry the key, in python's order (`provider.py`).
/// Named in the failure text so an unset connector says which one to set.
const KEY_VARS: [&str; 2] = ["ANTHROPIC_KEY", "ANTHROPIC_API_KEY"];

/// Read the key from the environment. No sidecar, no agent record — the key is
/// never written to disk by this bundle and never appears in `agent.json`.
fn read_api_key() -> Option<String> {
    KEY_VARS.iter().find_map(|v| {
        std::env::var(v)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

// ── plain messages → Anthropic translation ──────────────────────────

/// Python `anthropic_backend.provider._to_anthropic`: ai-core's messages →
/// (top-level `system`, the `messages` array of plain user/assistant turns).
///
/// Tool calls and their replies are already inline text (`<call name=…>` / its
/// reply tag), so there is NO tool_use/tool_result translation — pure text turns,
/// and this transport learns nothing about the tool protocol.
///
/// THE LAST SYSTEM MESSAGE WINS (it is an assignment in python, not a
/// concatenation), an empty one clears the key entirely, a falsy `content` on a
/// user/assistant turn becomes `(no content)` rather than being dropped, and
/// EVERY OTHER ROLE IS DROPPED — including `role:tool`, which rust used to remap
/// to `user` and so put a turn on the wire python never sends.
fn to_anthropic(messages: &[Value]) -> (Option<Value>, Vec<Value>) {
    let mut system: Option<Value> = None;
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        // python `m.get("content", "") or ""`: absent OR falsy is the empty string.
        let content = match m.get("content") {
            Some(v) if py_truthy(v) => v.clone(),
            _ => json!(""),
        };
        match role {
            "system" => system = Some(content),
            "user" | "assistant" => {
                let body = if py_truthy(&content) {
                    content
                } else {
                    json!("(no content)")
                };
                out.push(json!({"role": role, "content": body}));
            }
            _ => {}
        }
    }
    // python `if system:` — a falsy system block means no `system` key at all.
    (system.filter(py_truthy), out)
}

// ── Anthropic provider (x-api-key + event-typed SSE) ─────────────────

struct AnthropicProvider {
    self_id: AgentId,
    endpoint: String,
    model: String,
    max_tokens: u64,
    /// RUNNER (meta) PARAMS — the record's `sampling` object, merged VERBATIM
    /// into the request body after the fixed fields, exactly as python's
    /// provider does (`body.update(self._sampling)`). This is what bounds a
    /// reasoning model's deliberation (`thinking_token_budget`) and kills the
    /// CoT-repetition loop (`no_repeat_ngram_size`, `repetition_penalty`):
    /// without it every long rust turn ran to ~41k chars and the proxy's 180s
    /// cap (measured 2026-09-12: build, implicit_constraint, graph_shape).
    sampling: Map<String, Value>,
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, ProviderError> {
        // THE KEY IS CHECKED HERE, AND BEFORE THE MODEL — python
        // `provider.py:112-119`. It used to gate the PROVIDER BUILD, so an
        // unwired connector answered `{error: "anthropic_backend: no api key
        // — set ANTHROPIC_KEY or ANTHROPIC_API_KEY"}` as a COMPLETED turn,
        // before `status(thinking)`, with a sentence python does not have.
        // Inside `chat` the turn has already opened: the caller sees
        // `status(thinking)`, then `status(done, reason='error')`, and the
        // failure RAISES like every other provider failure.
        let Some(key) = read_api_key() else {
            return Err(ProviderError::raised(
                "ANTHROPIC_KEY not set in environment (.env)",
            ));
        };
        // NO guessy default: fail loud before the wire if no model is named.
        if self.model.is_empty() || self.model == DEFAULT_MODEL {
            return Err(ProviderError::raised(
                "anthropic_backend: no `model` set — this connector does \
                 not guess one. Name a model on the agent record (model=...).",
            ));
        }
        // RAW, always: no native `tools`/`tool_choice` — ai-core parses the
        // `<call name=…>` envelope out of the streamed text. The optional
        // `tool_use` route that declared one `send` tool and translated the
        // blocks back is DELETED: a model writing JSON tool calls is writing a
        // grammar this kernel does not parse, and the honest answer is to say so
        // (see `decode_sse_line`), not to translate a second dialect at the edge.
        // PYTHON'S KEY ORDER — `model, max_tokens, messages, stream, system`,
        // then the record's sampling knobs. The body is what a wire capture
        // diffs, so the order is part of the surface.
        let (system, anthropic_messages) = to_anthropic(messages);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(self.max_tokens));
        body.insert("messages".into(), json!(anthropic_messages));
        body.insert("stream".into(), json!(true));
        if let Some(sys) = system {
            body.insert("system".into(), sys);
        }
        for (k, v) in &self.sampling {
            body.insert(k.clone(), v.clone());
        }
        let body = Value::Object(body);

        // NO RATE-LIMIT POLICY. A 429 is the endpoint answering, and it is
        // returned as `anthropic 429: <body>` exactly like every other status
        // — python's provider has no retry, no backoff and no event for one.
        //
        // A GATEWAY HICCUP is the one retried case: 502/503/504/520-524 from a
        // CDN or reverse proxy in front of the API, and ONLY when the body is
        // not JSON. A JSON body is the API speaking deliberately — that stays
        // loud, or a real refusal would be re-sent until the budget ran out.
        let mut attempt: u32 = 0;
        let resp = loop {
            let client = get_or_build_client(&self.self_id, &key).map_err(ProviderError::raised)?;
            // VERBATIM — `endpoint` IS the messages URL (see DEFAULT_ENDPOINT).
            let url = self.endpoint.clone();
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::raised(format!("http: {}", error_chain(&e))))?;
            let code = resp.status().as_u16();
            if code < 400 {
                break resp;
            }
            let ctype = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // python `(await resp.aread()).decode("utf-8", "replace")`.
            let detail = match resp.bytes().await {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => return Err(ProviderError::raised(format!("http: {}", error_chain(&e)))),
            };
            if !(TRANSPORT_CODES.contains(&code)
                && !ctype.to_lowercase().contains("json")
                && attempt < RETRIES)
            {
                return Err(ProviderError::raised(format!("anthropic {code}: {detail}")));
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_secs_f64(BACKOFF * f64::from(attempt))).await;
        };

        Ok(consume_sse(resp))
    }
}

/// Drain the event-typed SSE body. Anthropic frames each event as an
/// `event:` line + a `data:` JSON line; the JSON carries its own `type`, so we
/// dispatch on that. PURE RAW TEXT — both `text_delta` and `thinking_delta` are
/// emitted (the latter wrapped in `<think>…</think>`); no
/// native `tool_use` is read (ai-core parses the `<call name=…>` envelope), and a
/// tool_use block that arrives anyway is a loud error.
/// Per-line SSE decode state — everything the old drain-to-`Vec` loop kept in
/// locals, made explicit so the parse can run one line at a time.
struct SseState {
    buf: Vec<u8>,
    stop: bool,
    stop_reason: Option<String>,
    /// The `type` of every `content_block_start` seen, in order — python's
    /// `blocks` list. A bare `stop_reason` names the symptom and nothing else;
    /// the block types are the one fact that distinguishes "the model wrote a
    /// native tool block" from "the connection died before any content". A
    /// block with no `type` key appends python's `None`.
    blocks: Vec<Value>,
    wrap: fantastic_ai_core::think::ThinkWrap,
}

impl SseState {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            stop: false,
            stop_reason: None,
            blocks: Vec::new(),
            wrap: fantastic_ai_core::think::ThinkWrap::new(),
        }
    }

    /// Terminality, decided by what the stream CONTAINED — not by what we asked
    /// for (py-canonical, measured 2026-08-07): a completion cut by max_tokens, a
    /// refusal, or a dropped connection was returned as if the model had
    /// finished; a create_agent turn stopped mid-`code`, the partial parsed as an
    /// unterminated envelope, and the chain died with no visible cause. A
    /// `/cc`-style route returns `tool_use` blocks for a request carrying no
    /// `tools` key at all — those now raise in `decode_sse_line`, so a
    /// `stop_reason` of `tool_use` with no block seen is an anomaly and says so.
    fn terminality(&self) -> Option<String> {
        // A turn that ended BECAUSE the model called tools is a complete turn —
        // python widens the terminal set when a `tool_use` block was actually
        // seen. (This crate raises on such a block the moment it arrives, so the
        // widening only ever matters if that raise is relaxed; the rule is
        // python's and travels with it.)
        let tool_use_seen = self.blocks.iter().any(|b| b.as_str() == Some("tool_use"));
        let terminal = match self.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence") => true,
            Some("tool_use") => tool_use_seen,
            _ => false,
        };
        if terminal {
            return None;
        }
        let reason = self
            .stop_reason
            .as_ref()
            .map_or(Value::Null, |s| json!(s.clone()));
        let tail = if self.stop_reason.is_none() {
            " (none seen — connection cut mid-stream)"
        } else {
            ""
        };
        Some(format!(
            "anthropic stream ended without end_turn: stop_reason={} blocks={}{tail}",
            py_repr_value(&reason),
            py_repr_value(&Value::Array(self.blocks.clone())),
        ))
    }
}

/// Decode ONE `data:` line into the events it produces. `Err` is terminal.
fn decode_sse_line(st: &mut SseState, line: &str) -> Result<Vec<ProviderEvent>, String> {
    let mut out: Vec<ProviderEvent> = Vec::new();
    if !line.starts_with("data:") {
        return Ok(out);
    }
    let data = line[5..].trim();
    if data.is_empty() {
        return Ok(out);
    }
    let Ok(obj) = serde_json::from_str::<Value>(data) else {
        return Ok(out);
    };
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        // A NATIVE tool_use BLOCK IS A LOUD ERROR. Its arguments are JSON we do
        // not read: the model wrote a call in a grammar this kernel does not
        // speak, and a silent drop would end the turn looking finished with the
        // call lost. Some `/cc`-style routes emit these for a request carrying
        // no `tools` key at all — that is exactly the case worth failing on.
        "content_block_start" => {
            // python `cb = evt.get("content_block") or {}` then
            // `blocks.append(cb.get("type"))` — a missing key appends `None`.
            let cb = obj.get("content_block").cloned().unwrap_or(json!({}));
            let btype = cb.get("type").cloned().unwrap_or(Value::Null);
            st.blocks.push(btype.clone());
            if btype.as_str() == Some("tool_use") {
                let name = cb.get("name").cloned().unwrap_or(Value::Null);
                return Err(format!(
                    "anthropic stream carried a native tool_use block ({}): \
                     the model wrote a JSON tool call, which this kernel does not \
                     parse — calls are `<call name=…>` text",
                    py_repr_value(&name)
                ));
            }
        }
        "content_block_delta" => {
            let delta = obj.get("delta").cloned().unwrap_or(Value::Null);
            let dtype = delta.get("type").and_then(Value::as_str);
            // BOTH CHANNELS ARE READ. A reasoning model streams its thought as
            // `thinking_delta` and its answer as `text_delta`; reading only the
            // latter drops the thought entirely — and with it any tool call the
            // model wrote MID-THOUGHT, which ai-core is built to still dispatch.
            if dtype == Some("thinking_delta") {
                if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                    if !t.is_empty() {
                        if let Some(m) = st.wrap.open_marker() {
                            out.push(ProviderEvent::Token(m.to_string()));
                        }
                        out.push(ProviderEvent::Token(t.to_string()));
                    }
                }
            }
            if dtype == Some("text_delta") {
                if let Some(t) = delta.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        // Content starting IS the end of the thought.
                        if let Some(m) = st.wrap.close_marker() {
                            out.push(ProviderEvent::Token(m.to_string()));
                        }
                        out.push(ProviderEvent::Token(t.to_string()));
                    }
                }
            }
        }
        // THE WHOLE `error` OBJECT, python-rendered — `raise RuntimeError(
        // f"anthropic stream error: {err}")` over `evt.get("error") or {}`.
        // Reading only `message` out of it dropped the `type`, which is the
        // field that says whether the endpoint overloaded or refused.
        "error" => {
            let err = obj.get("error").cloned().filter(py_truthy);
            return Err(format!(
                "anthropic stream error: {}",
                py_str(&err.unwrap_or(json!({})))
            ));
        }
        "message_delta" => {
            st.stop_reason = obj
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(st.stop_reason.take());
        }
        "message_stop" => {
            // Balance a span the model never closed itself, so the text the
            // parser sees is well-formed.
            if let Some(m) = st.wrap.close_marker() {
                out.push(ProviderEvent::Token(m.to_string()));
            }
            st.stop = true;
        }
        _ => {} // message_start, ping — ignored
    }
    Ok(out)
}

/// The SSE body as a LIVE stream of events.
///
/// This used to drain the whole body into a `Vec` and hand it over only when the
/// stream had ENDED — so nothing reached `text_so_far`, the `token` events, the
/// `streaming` phase or any subscriber until the completion was already
/// finished. A turn cut by the per-send ceiling therefore reported "0 chars
/// written" no matter how much the model had actually produced, which made a
/// long thought indistinguishable from a stalled request (measured 2026-09-04,
/// `subai_graph:rust`). Python and swift both yield per delta; this was the only
/// backend that did not.
fn consume_sse(resp: reqwest::Response) -> ProviderStream {
    let inner = resp.bytes_stream();
    // `pending` holds the events decoded from ONE chunk: a chunk can carry
    // several lines, and each line several tokens.
    let init = (inner, SseState::new(), VecDeque::new(), false);
    Box::pin(futures_util::stream::unfold(
        init,
        |(mut inner, mut st, mut pending, mut finished)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (inner, st, pending, finished)));
                }
                if finished {
                    return None;
                }
                // END OF BODY. The terminality verdict is the LAST event, so a
                // truncated completion still fails loudly instead of reading as
                // a finished turn.
                let Some(chunk_res) = inner.next().await else {
                    finished = true;
                    if let Some(why) = st.terminality() {
                        pending.push_back(Err(ProviderError::raised(why)));
                    }
                    continue;
                };
                match chunk_res {
                    Ok(bytes) => st.buf.extend_from_slice(&bytes),
                    Err(e) => {
                        finished = true;
                        pending.push_back(Err(ProviderError::raised(format!(
                            "stream: {}",
                            error_chain(&e)
                        ))));
                        continue;
                    }
                }
                while let Some(nl) = st.buf.iter().position(|b| *b == b'\n') {
                    let line_bytes: Vec<u8> = st.buf.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                        .trim_end_matches('\r')
                        .to_string();
                    match decode_sse_line(&mut st, &line) {
                        Ok(events) => pending.extend(events.into_iter().map(Ok)),
                        Err(e) => {
                            finished = true;
                            pending.push_back(Err(ProviderError::raised(e)));
                            break;
                        }
                    }
                    if st.stop {
                        finished = true;
                        if let Some(why) = st.terminality() {
                            pending.push_back(Err(ProviderError::raised(why)));
                        }
                        break;
                    }
                }
            }
        },
    ))
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
            "reflect" => reflect_reply(agent_id, kernel),
            // BOOT WITH A STORE RUNS `context_recall` — python `_make_boot`.
            "boot" => {
                // A BOOT IS A NEW INCARNATION (python `_make_boot`): a reply
                // answering a call the PREVIOUS one made is recorded as a row
                // and wakes no turn.
                state::bump_incarnation(agent_id);
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
                    let p = build_provider(agent_id, kernel);
                    retrieval::context_recall(agent_id, &json!({}), kernel, Some(&p)).await
                }
            }
            "send" => send_reply(agent_id, payload, kernel).await?,
            // ONE ASK VERB, WHOEVER IS ASKED: a person answers `inquiry` from a
            // record, this agent answers it by THINKING, and the asker writes
            // the same call either way.
            "inquiry" => match verbs::inquiry_open(payload) {
                Err(refusal) => refusal,
                Ok(sp) => verbs::inquiry_close(send_reply(agent_id, &sp, kernel).await?, payload),
            },
            "history" => verbs::history(agent_id, payload, kernel).await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            // FULL ASYNC: page a stored reply body by pid, or FIND the line first.
            "pull" => retrieval::pull(agent_id, payload, kernel).await,
            "grep" => retrieval::grep(agent_id, payload, kernel).await,
            // Rebuild the view from the store: on boot, after folds, on request.
            "context_recall" => {
                let p = build_provider(agent_id, kernel);
                retrieval::context_recall(agent_id, payload, kernel, Some(&p)).await
            }
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            // python's VERBS map has no `shutdown` — it is an unknown type like
            // any other, and the value is rendered with python's `repr`.
            _ => json!({
                "error": format!(
                    "anthropic_backend: unknown type {}",
                    py_repr_value(&payload.get("type").cloned().unwrap_or(Value::Null))
                )
            }),
        };
        Ok(Some(reply))
    }

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate drops
    /// the future (which frees the HTTP stream + the FIFO guard by itself).
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await;
        Ok(())
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

/// The provider this agent is configured for. Built per call (it is cheap and the
/// record can change between turns), and shared by `send` and the verbs that need
/// a completion of their own — `context_recall` asks the model which rows to keep
/// in full.
///
/// ALWAYS BUILDABLE: python's `make_provider` reads only the record, and the key
/// is the provider's own business at call time. Gating the build on the key made
/// an unwired connector answer a refusal python never writes.
fn build_provider(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
    let max_tokens = helpers::agent_meta(agent_id, kernel)
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let sampling: Map<String, Value> = helpers::agent_meta(agent_id, kernel)
        .get("sampling")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Arc::new(AnthropicProvider {
        self_id: agent_id.clone(),
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
        max_tokens,
        sampling,
    })
}

async fn send_reply(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    verbs::send(
        Some(build_provider(agent_id, kernel)),
        agent_id,
        payload,
        kernel,
        CFG,
    )
    .await
}

/// `reflect` — python `_make_reflect` with no `reflect_extra`: the shared base
/// body and nothing else. `max_tokens`, `tool_use`, `file_bridge_id` and
/// `has_api_key` were rust-only keys; a model that read them learned a surface
/// python's anthropic agent does not have.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    Value::Object(verbs::reflect_base(
        agent_id,
        kernel,
        SENTENCE,
        DEFAULT_MODEL,
        DEFAULT_ENDPOINT,
    ))
}

#[cfg(test)]
mod tests;
