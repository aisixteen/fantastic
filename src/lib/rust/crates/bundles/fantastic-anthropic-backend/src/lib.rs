//! anthropic_backend — Anthropic (Claude) Messages-API LLM bundle.
//! Thin shell over `fantastic-ai-core`: this crate supplies only the
//! Anthropic `Provider` (HTTPS + `x-api-key` + `anthropic-version`
//! header + event-typed SSE, RAW TEXT + 429 retry) plus a `Bundle` that
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
//! - `x-api-key` HTTP client cache (dropped on key change), 429 retry.
//!
//! Wire translation: ai-core's plain messages → Anthropic's top-level `system`
//! param + alternating user/assistant TEXT turns. NO native tools — Fantastic
//! NEVER uses Anthropic `tool_use`; tool-calling is RAW prompt-and-parse owned
//! by ai-core (the model emits a `<call name=…>` envelope in the text). Response
//! `content_block_delta`(`text_delta` + `thinking_delta`) → `ProviderEvent::Token`;
//! the reasoning channel is wrapped once per turn in `<think>…</think>`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::events::{emit_status, to_caller, CallerRoute};
use fantastic_ai_core::provider::{Provider, ProviderEvent, ProviderStream};
use fantastic_ai_core::retrieval;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
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

/// Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` is REQUIRED by the Messages API — default if unset on the meta.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Retries for a NON-JSON gateway 5xx (a CDN error page), with 1s/2s backoff.
/// A JSON body is the API answering deliberately and is never retried.
const GATEWAY_MAX_RETRIES: u32 = 2;

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

/// Per-backend config: per-client-inbox routing; serial dispatch.
const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::PerClientInbox,
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

/// The refusal for an unwired connector — names the variables to set.
fn no_key_error() -> String {
    format!(
        "anthropic_backend: no api key — set {}",
        KEY_VARS.join(" or ")
    )
}

// ── plain messages → Anthropic translation ──────────────────────────

/// Split ai-core's messages into Anthropic's top-level `system` string + the
/// `messages` array of plain user/assistant TEXT turns. Tool calls/replies are
/// already inline text (`<call name=…>` / its reply tag); ai-core's
/// `render_for_model` mapped tool replies to user-role text before we see them,
/// so there is NO tool_use/tool_result translation. System blocks concatenate;
/// empty turns are dropped (Anthropic rejects empty content).
fn translate_messages(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system: Option<String> = None;
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let content = m.get("content").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" => {
                system = Some(match system.take() {
                    Some(prev) if !prev.is_empty() => format!("{prev}\n\n{content}"),
                    _ => content.to_string(),
                });
            }
            "user" | "tool" => {
                out.push(json!({"role": "user", "content": content}));
            }
            "assistant" if !content.is_empty() => {
                out.push(json!({"role": "assistant", "content": content}));
            }
            _ => {}
        }
    }
    (system, out)
}

// ── Anthropic provider (x-api-key + event-typed SSE + 429 retry) ─────

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
impl Provider for AnthropicProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String> {
        // NO guessy default: fail loud before the wire if no model is named.
        if self.model.is_empty() || self.model == DEFAULT_MODEL {
            return Err("anthropic_backend: no `model` set — this connector does \
                not guess one. Name a model on the agent record (model=...)."
                .to_string());
        }
        // RAW, always: no native `tools`/`tool_choice` — ai-core parses the
        // `<call name=…>` envelope out of the streamed text. The optional
        // `tool_use` route that declared one `send` tool and translated the
        // blocks back is DELETED: a model writing JSON tool calls is writing a
        // grammar this kernel does not parse, and the honest answer is to say so
        // (see `decode_sse_line`), not to translate a second dialect at the edge.
        let (system, anthropic_messages) = translate_messages(messages);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(self.max_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("messages".into(), json!(anthropic_messages));
        if let Some(sys) = system {
            body.insert("system".into(), json!(sys));
        }
        for (k, v) in &self.sampling {
            body.insert(k.clone(), v.clone());
        }
        let body = Value::Object(body);

        // Retry-once-on-429 (before any chunk yielded). Emits a rate-limit
        // say + status during the backoff.
        let mut attempt: u32 = 0;
        let mut gw_attempt: u32 = 0;
        let resp = loop {
            let client = get_or_build_client(&self.self_id)?;
            // VERBATIM — `endpoint` IS the messages URL (see DEFAULT_ENDPOINT).
            let url = self.endpoint.clone();
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
            // A GATEWAY HICCUP, not an answer. 502/503/504/520-524 from a CDN or
            // reverse proxy in front of the API is transient; a 1-2s restart
            // returned a Cloudflare HTML page and killed a 26-pass run at pass 5.
            //
            // Retried ONLY when the body is not JSON. A JSON body is the API
            // speaking deliberately — that stays loud and is never retried, or a
            // real refusal would be re-sent until the budget ran out.
            let code = status.as_u16();
            let gateway = code == 502 || code == 503 || code == 504 || (520..=524).contains(&code);
            if gateway && gw_attempt < GATEWAY_MAX_RETRIES {
                let body_text = resp.text().await.unwrap_or_default();
                if serde_json::from_str::<Value>(&body_text).is_err() {
                    let wait = 1u64 << gw_attempt; // 1s, 2s
                    gw_attempt += 1;
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                return Err(format!("send: provider HTTP {code}"));
            }
            if !status.is_success() {
                return Err(format!("send: provider HTTP {code}"));
            }
            break resp;
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
    wrap: fantastic_ai_core::think::ThinkWrap,
}

impl SseState {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            stop: false,
            stop_reason: None,
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
        match self.stop_reason.as_deref() {
            Some("end_turn") | Some("stop_sequence") => None,
            Some(other) => Some(format!(
                "anthropic stream ended without end_turn: stop_reason={other:?}"
            )),
            None => Some(
                "anthropic stream ended without end_turn: no stop_reason seen \
                 (connection cut mid-stream)"
                    .to_string(),
            ),
        }
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
            let cb = obj.get("content_block");
            if cb.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                let name = cb
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                return Err(format!(
                    "anthropic stream carried a native tool_use block ({name:?}): \
                     the model wrote a JSON tool call, which this kernel does not \
                     parse — calls are `<call name=…>` text"
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
        "error" => {
            let msg = obj
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("anthropic stream error");
            return Err(format!("stream: {msg}"));
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
                        pending.push_back(Err(why));
                    }
                    continue;
                };
                match chunk_res {
                    Ok(bytes) => st.buf.extend_from_slice(&bytes),
                    Err(e) => {
                        finished = true;
                        pending.push_back(Err(format!("stream: {}", error_chain(&e))));
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
                            pending.push_back(Err(e));
                            break;
                        }
                    }
                    if st.stop {
                        finished = true;
                        if let Some(why) = st.terminality() {
                            pending.push_back(Err(why));
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
            other => json!({"error": format!("anthropic_backend: unknown type {other:?}")}),
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
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel, CFG.route).await;
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
/// in full. `None` when the backend is not configured to reach a model at all;
/// the caller says so in its own words rather than guessing a provider.
fn build_provider(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Option<Arc<dyn Provider>> {
    read_api_key()?;
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
    Some(Arc::new(AnthropicProvider {
        self_id: agent_id.clone(),
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
        max_tokens,
        sampling,
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
        "sentence": "Anthropic (Claude) Messages-API LLM agent (raw prompt-and-parse tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "max_tokens": max_tokens,
        // Which ROUTE this agent is on. Readable, because a model debugging why
        // its envelope did or did not reach the wire cannot otherwise tell.
        "tool_use": meta.get("tool_use").and_then(Value::as_bool).unwrap_or(false),
        "file_bridge_id": file_bridge_id_v,
        "has_api_key": has_key,
        "generating": generating,
        "context_window": context_window,
        "context_strategy": context_strategy,
        "verbs": {
            "reflect": "Identity + model + endpoint + max_tokens + has_api_key + generating + file_bridge_id binding. No args. `has_api_key` reports whether the ENV key is set; the value is NEVER returned.",
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
            "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the Context Protocol push half. Pull counterpart: the context_status verb.",
        },
        "concurrency": "Per-backend FIFO lock around `send`. reflect/history skip the lock.",
    })
}

#[cfg(test)]
mod tests;
