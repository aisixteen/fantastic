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
use fantastic_ai_core::events::emit_status;
use fantastic_ai_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, ReportCell,
    Unconfigured,
};
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle::py_repr_value;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "nvidia_nim_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The name a required-field refusal carries.
const BACKEND: &str = "nvidia_nim_backend";

/// What `reflect` says this agent IS — python `nvidia_nim_backend/tools.py`'s
/// `sentence`, verbatim.
pub const SENTENCE: &str =
    "NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).";

/// Headless / REPL caller default.
pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;

/// Hard per-generation ceiling (seconds). Releases the FIFO lock.
pub const SEND_TIMEOUT_SECS: f64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

/// Max wait honored from a `Retry-After` header on 429.
pub const RATE_LIMIT_MAX_WAIT_SECS: u64 = 60;

/// Default wait when `Retry-After` is absent / unparseable.
pub const RATE_LIMIT_DEFAULT_WAIT_SECS: u64 = 5;

/// Per-agent retry budget on 429 before any chunk has been yielded.
pub const RATE_LIMIT_MAX_RETRIES: u32 = 1;

/// Per-backend config: per-client-inbox routing, serial tool dispatch.
const CFG: BackendConfig = BackendConfig {
    name: "nvidia_nim_backend",
    // python `require_provider=True` + `provider_missing_error`: the key is
    // the precondition, and `send` refuses with this text after the guard.
    provider_missing_error: Some(NO_KEY_ERROR),
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
        return Err(NO_KEY_ERROR.to_string());
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
/// never written to disk by this bundle and never appears in `agent.yaml`.
fn read_api_key() -> Option<String> {
    std::env::var(KEY_VAR)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The refusal for an unwired connector — names the variable to set. python
/// `_no_key_error()`, handed to `send` as the config's `provider_missing_error`
/// so the refusal is minted where python mints it: inside the shared verb,
/// after the recursion guard, with the caller's `client_id`.
const NO_KEY_ERROR: &str = "nvidia_nim_backend: no api key — set NVIDIA_API_KEY";

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
    /// What the endpoint reported about the last pass — `usage.prompt_tokens` on
    /// the last chunk, and the `finish_reason` (`length` ⇒ `max_tokens`).
    report: ReportCell,
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

    fn report(&self) -> ProviderReport {
        self.report.lock().expect("report poisoned").clone()
    }

    fn clear_prompt_tokens(&self) {
        self.report.lock().expect("report poisoned").prompt_tokens = None;
    }

    fn clear_stop_reason(&self) {
        self.report.lock().expect("report poisoned").stop_reason = None;
    }

    async fn chat(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        // NO guessy default: fail loud before the wire if no model is named.
        if self.model.is_empty() {
            return Err(ProviderError::raised(
                "nvidia_nim_backend: no `model` set — this connector does \
                 not guess one. Name a model on the agent record (model=...).",
            ));
        }
        // RAW: no native `tools`/`tool_choice` — ai-core parses the `<tool_call>`
        // envelope out of the streamed text.
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // PROVIDER-REPORTED USAGE on the last chunk (`usage.prompt_tokens`).
            "stream_options": {"include_usage": true},
        });
        if let Some(n) = max_tokens {
            // a side call's own bound (a fold summary's budget)
            body["max_tokens"] = json!(n);
        }
        // Retry-once-on-429 (before any chunk yielded). Emits a
        // rate-limit say + status during the backoff.
        let mut attempt: u32 = 0;
        let resp = loop {
            let client = get_or_build_client(&self.self_id).map_err(ProviderError::raised)?;
            let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::raised(format!("http: {}", error_chain(&e))))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt < RATE_LIMIT_MAX_RETRIES {
                    let wait = parse_retry_after(resp.headers());
                    attempt += 1;
                    drop(resp);
                    // STATUS ONLY. python `_stream_with_rate_limit_retry` emits
                    // one `status(thinking, waiting_on='rate_limit', wait_s)` and
                    // nothing else; the `say` rust pushed alongside it put a
                    // model-visible line in the caller's stream that the
                    // canonical runtime never writes.
                    let mut extras = Map::new();
                    extras.insert("waiting_on".into(), json!("rate_limit"));
                    extras.insert("wait_s".into(), json!(wait));
                    emit_status(
                        &self.kernel,
                        &state::state_for(&self.self_id),
                        &self.self_id,
                        &self.client_id,
                        "thinking",
                        extras,
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                } else {
                    // MAPPED — python's `_map_error` turns the provider's
                    // `httpx.HTTPStatusError` into this sentence and `send`
                    // RETURNS it; only this backend has a mapper.
                    let wait = parse_retry_after(resp.headers());
                    return Err(ProviderError::mapped(format!(
                        "send: rate limited (429); retry in {wait}s"
                    )));
                }
            }
            if !status.is_success() {
                // MAPPED, as above — `_map_error`'s non-429 branch.
                return Err(ProviderError::mapped(format!(
                    "send: provider HTTP {}",
                    status.as_u16()
                )));
            }
            break resp;
        };

        Ok(consume_sse(resp, Arc::clone(&self.report)))
    }
}

/// The SSE body as a LIVE stream of content tokens.
///
/// PER LINE, NOT PER BODY. This used to drain the whole response into a `Vec`
/// and hand it over only once the stream had ENDED — so nothing reached
/// `text_so_far`, the `token` events or the `streaming` phase until the
/// completion was already finished, and a long turn was indistinguishable from
/// a stalled request. Python's provider is an async generator over
/// `resp.aiter_lines()`; this yields on the same boundary.
///
/// PURE RAW TEXT — no native `tool_calls` are read; ai-core parses the
/// `<call name=…>` envelope out of the token text.
fn consume_sse(resp: reqwest::Response, report: ReportCell) -> ProviderStream {
    let inner = resp.bytes_stream();
    // `pending` holds the events decoded from ONE chunk: a chunk can carry
    // several lines, and each line several tokens.
    let init = (inner, SseState::new(report), VecDeque::new(), false);
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
                let Some(chunk_res) = inner.next().await else {
                    finished = true;
                    st.close_span(&mut pending);
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
                    if st.decode_line(&line, &mut pending) {
                        finished = true;
                        st.close_span(&mut pending);
                        break;
                    }
                }
            }
        },
    ))
}

/// Per-line SSE decode state — the buffer plus the per-TURN reasoning span.
struct SseState {
    buf: Vec<u8>,
    wrap: ThinkWrap,
    report: ReportCell,
}

impl SseState {
    fn new(report: ReportCell) -> Self {
        Self {
            buf: Vec::new(),
            wrap: ThinkWrap::new(),
            report,
        }
    }

    /// Decode ONE line into the tokens it produces. `true` = the `[DONE]`
    /// marker: the turn is over.
    fn decode_line(
        &mut self,
        line: &str,
        out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
    ) -> bool {
        if line.is_empty() || !line.starts_with("data:") {
            return false;
        }
        let data = line[5..].trim();
        if data.is_empty() {
            return false;
        }
        if data == "[DONE]" {
            return true;
        }
        let Ok(obj) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        fantastic_ai_core::think::note_openai_usage(&obj, &self.report);
        let Some(choices) = obj.get("choices").and_then(Value::as_array) else {
            return false;
        };
        let Some(first) = choices.first() else {
            return false;
        };
        let delta = first.get("delta").cloned().unwrap_or(Value::Null);
        decode_delta_into(&delta, &mut self.wrap, out);
        false
    }

    /// The turn ended while still reasoning (content never started) — close the
    /// span exactly once so the deliberation is strippable.
    fn close_span(&mut self, out: &mut VecDeque<Result<ProviderEvent, ProviderError>>) {
        if let Some(m) = self.wrap.close_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
    }
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
    out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
) {
    // DELEGATES to the shared decode: NIM and `llama-server` speak the same
    // wire, and two copies would drift on the one thing this gets right —
    // reading BOTH channels, not just `content`.
    for piece in fantastic_ai_core::think::decode_openai_delta(delta, wrap) {
        out.push_back(Ok(ProviderEvent::Token(piece)));
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
        if verb == "reflect" {
            return Ok(Some(reflect_reply(agent_id, kernel)));
        }
        // EVERY OTHER ai_core VERB, one table for every backend (python `build`).
        let provider = |client: &str| build_provider(agent_id, kernel, client);
        if let Some(reply) = verbs::handle_common(agent_id, payload, kernel, CFG, provider).await? {
            return Ok(Some(reply));
        }
        // python's VERBS map has no `shutdown` — it is an unknown type like any
        // other, and the value is rendered with python's `repr`.
        Ok(Some(json!({
            "error": format!(
                "nvidia_nim_backend: unknown type {}",
                py_repr_value(&payload.get("type").cloned().unwrap_or(Value::Null))
            )
        })))
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
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await;
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
    // `endpoint` and `model` are REQUIRED on the record — nothing is guessed.
    let meta = helpers::agent_meta(agent_id, kernel);
    let (endpoint, model) = match (
        helpers::required_meta(&meta, "endpoint", BACKEND),
        helpers::required_meta(&meta, "model", BACKEND),
    ) {
        (Ok(e), Ok(m)) => (e, m),
        (Err(e), _) | (_, Err(e)) => return Some(Arc::new(Unconfigured(e))),
    };
    Some(Arc::new(NimProvider {
        self_id: agent_id.clone(),
        endpoint,
        model,
        kernel: Arc::clone(kernel),
        client_id: client_id.to_string(),
        report: ReportCell::default(),
    }))
}

/// `reflect` — python `_make_reflect` + `nvidia_nim_backend/tools.py`'s
/// `_reflect_extra`: the shared base body, then `has_api_key`. The KEY itself is
/// never returned.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE);
    out.insert("has_api_key".to_string(), json!(read_api_key().is_some()));
    Value::Object(out)
}

#[cfg(test)]
mod tests;
