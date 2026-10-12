//! responses_backend — OpenAI Responses-API LLM bundle.
//! Thin shell over `fantastic-ai-core`: this crate supplies only the Responses
//! `Provider` (HTTPS + `Authorization: Bearer` + event-typed SSE, RAW TEXT) plus a
//! `Bundle` that dispatches every verb through ai-core. The FIFO lock, prompt
//! assembly, the one-pass loop (incl. RAW tool-call parsing), status phase
//! machine and history persistence live in ai-core.
//!
//! Python `responses_backend` is canonical. Responses-local extras:
//!
//! - the key is read before EACH request from the record's `credentials: {store,
//!   ref}` — a ref in a `credential_store`, sent only to the ref's host
//!   (`fantastic_ai_core::credentials`) — never from the environment or the
//!   record. `credentials` is REQUIRED; a refused read fails the turn, naming
//!   the store and the ref. No key-storage verb, no sidecar.
//! - NO guessed host and NO guessed model: `endpoint` (the full `…/v1/responses`
//!   URL, used verbatim) and `model` must be on the record; the sentinels exist
//!   only so `reflect` can say what is true.
//!
//! Wire translation: ai-core's plain messages → `instructions` (the system block)
//! and typed `message` input items (`input_text` for the user, `output_text` for
//! the assistant). STATELESS: `store:false`, the whole conversation every pass. NO
//! native tools — a `function_call` output item is a loud error. The stream's
//! `response.reasoning_text` / `reasoning_summary_text` deltas are wrapped once per
//! turn in `<think>…</think>`; `response.output_text` deltas are the output.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, ReportCell,
    Unconfigured,
};
use fantastic_ai_core::schema::{self, Field};
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, verbs};
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
pub const HANDLER_MODULE: &str = "responses_backend.tools";

/// readme.md — the PYTHON bundle's, byte for byte.
pub const README: &str = include_str!("readme.md");

/// The name a required-field refusal carries.
const BACKEND: &str = "responses_backend";

/// What `reflect` says this agent IS — python `responses_backend/tools.py`'s
/// `sentence`, verbatim.
pub const SENTENCE: &str = "OpenAI Responses-API LLM agent (raw prompt-and-parse tool-calling).";

/// Per-turn output budget (`max_output_tokens`) when the record names none.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// EDGE TRANSPORT FAILURES. A 4xx, or any 5xx
/// carrying a JSON body, is the endpoint deliberately answering and stays loud;
/// these codes with a NON-JSON body are a CDN/proxy error page and are retried,
/// bounded. Python `provider.py`'s `TRANSPORT_CODES`.
const TRANSPORT_CODES: [u16; 8] = [502, 503, 504, 520, 521, 522, 523, 524];

/// Attempts spent on a transport code before giving up — python `RETRIES`.
const RETRIES: u32 = 2;

/// Seconds slept between transport attempts, times the attempt number — python
/// `BACKOFF`.
const BACKOFF: f64 = 1.5;

/// Reasoning deltas: raw chain of thought (an open-weights model behind a proxy)
/// or OpenAI's own summary — both the thought channel, wrapped, never output.
const REASONING_DELTAS: [&str; 2] = [
    "response.reasoning_text.delta",
    "response.reasoning_summary_text.delta",
];

/// Output deltas. A refusal is text the model produced; dropping it would return
/// a refused turn as an EMPTY completion.
const TEXT_DELTAS: [&str; 2] = ["response.output_text.delta", "response.refusal.delta"];

/// Per-backend config: per-client-inbox routing; serial dispatch.
const CFG: BackendConfig = BackendConfig {
    name: "responses_backend",
    // A server's window is the record's to name (`context_window`); the kernel
    // cannot read it.
    model_window: fantastic_ai_core::constants::DEFAULT_CONTEXT_WINDOW,
};

// ── HTTP client cache (the key rides each request, never the client) ─

static HTTP_CLIENTS: OnceLockHttpMap = OnceLockHttpMap::new();

/// Per agent: its client.
type ClientMap = HashMap<AgentId, Arc<reqwest::Client>>;

struct OnceLockHttpMap(OnceLock<Mutex<ClientMap>>);
impl OnceLockHttpMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, ClientMap> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("HTTP_CLIENTS poisoned")
    }
}

/// An error WITH ITS CAUSES — reqwest's `Display` prints only the outer layer.
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

fn build_client() -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("text/event-stream"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        // A fresh connection per request and HTTP/1.1 only — python opens a new
        // `httpx.AsyncClient` per pass; a pooled or h2 connection through the
        // CloudFront proxy was measured dying mid-request.
        .pool_max_idle_per_host(0)
        .http1_only()
        .build()
        .map_err(|e| format!("build client: {e}"))
}

/// The cached client for this agent — the transport only: the key is read per
/// request and rides that request's headers.
fn get_or_build_client(self_id: &AgentId) -> Result<Arc<reqwest::Client>, String> {
    if let Some(c) = HTTP_CLIENTS.lock().get(self_id).cloned() {
        return Ok(c);
    }
    let client = Arc::new(build_client()?);
    HTTP_CLIENTS
        .lock()
        .insert(self_id.clone(), Arc::clone(&client));
    Ok(client)
}

fn drop_cached_client(self_id: &AgentId) {
    HTTP_CLIENTS.lock().remove(self_id);
}

// ── plain messages → Responses translation ──────────────────────────

/// Python `responses_backend.provider._to_responses`: ai-core's messages →
/// (`instructions`, typed input items).
///
/// The LAST system message wins (an assignment in python) and a falsy one means
/// no `instructions` key at all; a falsy user/assistant content becomes
/// `(no content)`; every other role is dropped. Tool calls and replies are already
/// inline text, so there is no function_call/function_call_output translation.
fn to_responses(messages: &[Value]) -> (Option<Value>, Vec<Value>) {
    let mut instructions: Option<Value> = None;
    let mut items: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let content = match m.get("content") {
            Some(v) if py_truthy(v) => v.clone(),
            _ => json!(""),
        };
        match role {
            "system" => instructions = Some(content),
            "user" | "assistant" => {
                let part = if role == "user" {
                    "input_text"
                } else {
                    "output_text"
                };
                let text = if py_truthy(&content) {
                    content
                } else {
                    json!("(no content)")
                };
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": part, "text": text}],
                }));
            }
            _ => {}
        }
    }
    (instructions.filter(py_truthy), items)
}

// ── the provider ────────────────────────────────────────────────────

struct ResponsesProvider {
    self_id: AgentId,
    endpoint: String,
    model: String,
    /// The key reader, awaited before EACH request.
    key: Arc<dyn KeySource>,
    max_tokens: i64,
    /// The record's `sampling` object, merged VERBATIM into the body after the
    /// fixed fields (python `body.update(self._sampling)`); it may not name a
    /// key the provider sets itself ([`OWNED_KEYS`]).
    sampling: Map<String, Value>,
    /// The endpoint's reported prompt size and the last stop reason — ai_core
    /// calibrates on the first and refuses a cut fold summary on the second.
    report: ReportCell,
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    fn report(&self) -> ProviderReport {
        self.report.lock().expect("report poisoned").clone()
    }

    fn clear_prompt_tokens(&self) {
        let mut r = self.report.lock().expect("report poisoned");
        r.prompt_tokens = None;
        r.completion_tokens = None;
    }

    fn clear_stop_reason(&self) {
        self.report.lock().expect("report poisoned").stop_reason = None;
    }

    async fn chat(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        // Python's order: the endpoint, then the model, then the key (read
        // inside the turn, before the request).
        if self.endpoint.is_empty() {
            return Err(ProviderError::raised(
                "responses_backend: no `endpoint` set — this connector does not \
                 guess a host. Name the full …/v1/responses URL on the agent \
                 record (endpoint=...).",
            ));
        }
        if self.model.is_empty() {
            return Err(ProviderError::raised(
                "responses_backend: no `model` set — this connector does not guess \
                 one. Name a model on the agent record (model=...).",
            ));
        }
        // PYTHON'S KEY ORDER — `model, input, max_output_tokens, stream, store,
        // instructions`, then the record's sampling knobs.
        let (instructions, items) = to_responses(messages);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("input".into(), json!(items));
        // A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
        match max_tokens.filter(|n| *n != 0) {
            Some(n) => body.insert("max_output_tokens".into(), json!(n)),
            None => body.insert("max_output_tokens".into(), json!(self.max_tokens)),
        };
        body.insert("stream".into(), json!(true));
        // STATELESS: the conversation is ai-core's, never the endpoint's.
        body.insert("store".into(), json!(false));
        if let Some(ins) = instructions {
            body.insert("instructions".into(), ins);
        }
        for (k, v) in &self.sampling {
            body.insert(k.clone(), v.clone());
        }
        let body = Value::Object(body);
        // THE KEY, read before EACH request (python `await self._key(...)` in
        // the headers): a refused read raises inside the turn.
        let key = self.key.key(&self.endpoint).await?.unwrap_or_default();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|e| ProviderError::raised(format!("api_key has illegal header chars: {e}")))?;
        auth.set_sensitive(true);
        if wire_on() {
            println!("{}", wire_request_line(&body, items.len()));
        }

        let mut attempt: u32 = 0;
        let resp = loop {
            let client = get_or_build_client(&self.self_id).map_err(ProviderError::raised)?;
            let resp = client
                .post(&self.endpoint)
                .header(reqwest::header::AUTHORIZATION, auth.clone())
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
            let detail = match resp.bytes().await {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => return Err(ProviderError::raised(format!("http: {}", error_chain(&e)))),
            };
            if !(TRANSPORT_CODES.contains(&code)
                && !ctype.to_lowercase().contains("json")
                && attempt < RETRIES)
            {
                return Err(ProviderError::raised(format!("responses {code}: {detail}")));
            }
            attempt += 1;
            let ctype_shown = if ctype.is_empty() {
                "no content-type"
            } else {
                ctype.as_str()
            };
            println!(
                "[responses] {code} ({ctype_shown}) — transport error, retry {attempt}/{RETRIES}"
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
            tokio::time::sleep(Duration::from_secs_f64(BACKOFF * f64::from(attempt))).await;
        };

        Ok(consume_sse(
            resp,
            Arc::clone(&self.report),
            max_tokens.is_some(),
        ))
    }
}

/// Per-line SSE decode state.
struct SseState {
    buf: Vec<u8>,
    /// The terminal response object (`response.completed` / `.incomplete`) —
    /// `None` until one arrives. A stream that ends without one did not finish.
    final_response: Option<Value>,
    /// Set once the terminal event arrived: nothing after it is read.
    stop: bool,
    /// The `type` of every `response.output_item.added` item, in order — python's
    /// `kinds`. The one fact that tells "the model wrote a native call" from "the
    /// connection died before any content". A missing type appends `None`.
    kinds: Vec<Value>,
    wrap: fantastic_ai_core::think::ThinkWrap,
    report: ReportCell,
    /// A BOUNDED side call (`max_tokens` given): stopping at the bound is an
    /// ANSWER, not a fault — the caller reads `stop_reason`.
    bounded: bool,
}

impl SseState {
    fn new(report: ReportCell, bounded: bool) -> Self {
        Self {
            buf: Vec::new(),
            final_response: None,
            stop: false,
            kinds: Vec::new(),
            wrap: fantastic_ai_core::think::ThinkWrap::new(),
            report,
            bounded,
        }
    }

    fn status(&self) -> Value {
        self.final_response
            .as_ref()
            .and_then(|r| r.get("status"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn reason(&self) -> Value {
        self.final_response
            .as_ref()
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// The stream ended: record the stop reason, then the terminality verdict.
    /// Python: `"max_tokens" if reason == "max_output_tokens" else (reason or
    /// status)`; a bounded side call cut at its bound is an answer; anything but
    /// `completed` is a loud error naming what the stream contained.
    fn finish(&self) -> Option<String> {
        let status = self.status();
        let reason = self.reason();
        let stop = if reason.as_str() == Some("max_output_tokens") {
            Some("max_tokens".to_string())
        } else if py_truthy(&reason) {
            Some(py_str(&reason))
        } else if py_truthy(&status) {
            Some(py_str(&status))
        } else {
            None
        };
        self.report.lock().expect("report poisoned").stop_reason = stop.clone();
        if self.bounded && stop.as_deref() == Some("max_tokens") {
            return None;
        }
        if status.as_str() == Some("completed") {
            return None;
        }
        let tail = if self.final_response.is_none() {
            " (no terminal event — connection cut mid-stream)"
        } else {
            ""
        };
        Some(format!(
            "responses stream ended without completing: status={} reason={} items={}{tail}",
            py_repr_value(&status),
            py_repr_value(&reason),
            py_repr_value(&Value::Array(self.kinds.clone())),
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
    let etype = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if REASONING_DELTAS.contains(&etype) {
        // BOTH CHANNELS ARE READ: a turn spent thinking would otherwise arrive
        // EMPTY, and a `<call>` written mid-thought would be lost with it.
        if let Some(t) = obj.get("delta").and_then(Value::as_str) {
            if !t.is_empty() {
                if let Some(m) = st.wrap.open_marker() {
                    out.push(ProviderEvent::Token(m.to_string()));
                }
                out.push(ProviderEvent::Token(t.to_string()));
            }
        }
        return Ok(out);
    }
    if TEXT_DELTAS.contains(&etype) {
        if let Some(t) = obj.get("delta").and_then(Value::as_str) {
            if !t.is_empty() {
                if let Some(m) = st.wrap.close_marker() {
                    out.push(ProviderEvent::Token(m.to_string()));
                }
                out.push(ProviderEvent::Token(t.to_string()));
            }
        }
        return Ok(out);
    }
    match etype {
        // A NATIVE function_call ITEM IS A LOUD ERROR — JSON arguments this kernel
        // does not read; a silent drop would end the turn with the call lost.
        "response.output_item.added" => {
            let item = obj.get("item").cloned().unwrap_or(json!({}));
            let kind = item.get("type").cloned().unwrap_or(Value::Null);
            st.kinds.push(kind.clone());
            if kind.as_str() == Some("function_call") {
                let name = item.get("name").cloned().unwrap_or(Value::Null);
                return Err(format!(
                    "responses stream carried a native function_call item ({}): the \
                     model wrote a JSON tool call, which this kernel does not parse — \
                     calls are `<call name=…>` text",
                    py_repr_value(&name)
                ));
            }
        }
        "response.completed" | "response.incomplete" => {
            let resp = obj.get("response").cloned().unwrap_or(json!({}));
            // PROVIDER-REPORTED USAGE — what the window really held.
            let n = resp
                .get("usage")
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output = resp
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            {
                let mut r = st.report.lock().expect("report poisoned");
                r.prompt_tokens = (n != 0).then_some(n);
                r.completion_tokens = (output != 0).then_some(output);
            }
            st.final_response = Some(resp);
            // A turn spent ENTIRELY reasoning ends inside the span — balance it.
            if let Some(m) = st.wrap.close_marker() {
                out.push(ProviderEvent::Token(m.to_string()));
            }
            st.stop = true;
        }
        "response.failed" => {
            let err = obj
                .get("response")
                .and_then(|r| r.get("error"))
                .cloned()
                .unwrap_or(Value::Null);
            return Err(format!("responses stream failed: {}", py_str(&err)));
        }
        "error" => {
            return Err(format!("responses stream error: {}", py_str(&obj)));
        }
        _ => {} // created / in_progress / *.done / content_part.* — ignored
    }
    Ok(out)
}

/// `FANTASTIC_AI_WIRE=1` prints the outgoing budget and each completion's terminal
/// status with its item types — python `provider.py`'s two lines, nothing else.
fn wire_on() -> bool {
    std::env::var("FANTASTIC_AI_WIRE").as_deref() == Ok("1")
}

/// python `f"[wire] -> max_output_tokens={…} items={len(items)}"`.
fn wire_request_line(body: &Value, items: usize) -> String {
    format!(
        "[wire] -> max_output_tokens={} items={}",
        body["max_output_tokens"], items
    )
}

/// python `f"[wire] <- status={status!r} reason={reason!r} items={kinds!r}"`.
fn wire_reply_line(st: &SseState) -> String {
    format!(
        "[wire] <- status={} reason={} items={}",
        py_repr_value(&st.status()),
        py_repr_value(&st.reason()),
        py_repr_value(&Value::Array(st.kinds.clone()))
    )
}

/// The SSE body as a LIVE stream of events — one per delta, the terminality
/// verdict last, so a cut completion fails loudly instead of reading as finished.
fn consume_sse(resp: reqwest::Response, report: ReportCell, bounded: bool) -> ProviderStream {
    let inner = resp.bytes_stream();
    let init = (
        inner,
        SseState::new(report, bounded),
        VecDeque::new(),
        false,
    );
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
                    // A span the stream never closed is balanced before the verdict.
                    if let Some(m) = st.wrap.close_marker() {
                        pending.push_back(Ok(ProviderEvent::Token(m.to_string())));
                    }
                    if wire_on() {
                        println!("{}", wire_reply_line(&st));
                    }
                    if let Some(why) = st.finish() {
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
                        if wire_on() {
                            println!("{}", wire_reply_line(&st));
                        }
                        if let Some(why) = st.finish() {
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

/// THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read
/// with no agent existing (`reflect bundles=all`), from the constants the
/// provider runs on. `validate_meta` enforces it at the write, so the page and
/// the kernel cannot disagree about what a record needs. Python
/// `RECORD_SCHEMA`, field for field.
#[must_use]
pub fn record_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        schema::schema(vec![
            Field::new(
                "endpoint",
                "str",
                "the full Responses API URL (…/v1/responses)",
            )
            .required(),
            Field::new("model", "str", "the model name").required(),
            Field::new("max_tokens", "int", "a turn's output budget (tokens)")
                .default(json!(DEFAULT_MAX_TOKENS)),
            Field::new(
                "sampling",
                "map",
                "runner params merged verbatim into the request body",
            ),
            schema::credentials_field(true),
        ])
    })
}

/// The OpenAI Responses-API backend bundle.
pub struct ResponsesBundle;

#[async_trait]
impl Bundle for ResponsesBundle {
    fn name(&self) -> &str {
        "responses_backend"
    }

    fn verb_docs(&self) -> Value {
        fantastic_ai_core::verbs::verb_docs()
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(fantastic_ai_core::verbs::emits())
    }

    fn cancel_doc(&self) -> Option<Value> {
        Some(Value::String(
            fantastic_ai_core::verbs::CANCEL_DOC.to_string(),
        ))
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(fantastic_ai_core::verbs::notes())
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// Refuse, AT THE WRITE, a record missing a required field or naming an enum
    /// value outside its set — every missing field named at once, from the schema.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        schema::validate(record_schema(), "responses_backend", meta, record)
    }

    fn record_schema(&self) -> Option<Value> {
        Some(record_schema().clone())
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
        let provider = |_client: &str| build_provider(agent_id, kernel);
        if let Some(reply) = verbs::handle_common(agent_id, payload, kernel, CFG, provider).await? {
            return Ok(Some(reply));
        }
        Ok(Some({
            let raw = payload.get("type").cloned().unwrap_or(Value::Null);
            let mut r = fantastic_kernel::refusal::refusal(
                fantastic_kernel::refusal::UNHANDLED,
                format!("responses_backend: unknown type {}", py_repr_value(&raw)),
            );
            fantastic_kernel::refusal::with(&mut r, "verb", raw);
            r
        }))
    }

    /// The kernel is killing a task of this agent: close the in-flight turn out
    /// for the caller before the substrate drops the future.
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        drop_cached_client(agent_id);
        verbs::on_delete(agent_id, kernel).await;
        Ok(())
    }
}

/// The provider this agent is configured for — built per call from the record
/// (python `make_provider`); the key is read through the reader inside each pass,
/// from the record's `credentials`.
fn build_provider(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
    let meta = helpers::agent_meta(agent_id, kernel);
    // python's argument order: `endpoint`, `model`, `max_tokens`, then the
    // provider's own check of `sampling`.
    // `endpoint` and `model` are REQUIRED on the record — nothing is guessed.
    let (endpoint, model) = match (
        helpers::required_meta(&meta, "endpoint", BACKEND),
        helpers::required_meta(&meta, "model", BACKEND),
    ) {
        (Ok(e), Ok(m)) => (e, m),
        (Err(e), _) | (_, Err(e)) => return Arc::new(Unconfigured(e)),
    };
    let max_tokens = match py_int_or(meta.get("max_tokens"), DEFAULT_MAX_TOKENS) {
        Ok(n) => n,
        Err(e) => return Arc::new(Unconfigured(ProviderError::value_error(e))),
    };
    let raw_sampling = meta.get("sampling").and_then(Value::as_object);
    let mut owned: Vec<&str> = raw_sampling
        .map(|m| {
            m.keys()
                .map(String::as_str)
                .filter(|k| OWNED_KEYS.contains(k))
                .collect()
        })
        .unwrap_or_default();
    if !owned.is_empty() {
        owned.sort_unstable();
        return Arc::new(Unconfigured(ProviderError::value_error(format!(
            "responses_backend: `sampling` may not set {} — those are the record's \
             (`model`, `max_tokens`) or the provider's own",
            owned.join(", ")
        ))));
    }
    let sampling: Map<String, Value> = raw_sampling
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Arc::new(ResponsesProvider {
        self_id: agent_id.clone(),
        endpoint,
        model,
        key: Arc::new(key_reader(agent_id, kernel, BACKEND, true)),
        max_tokens,
        sampling,
        report: ReportCell::default(),
    })
}

/// Request-body keys this provider sets itself; `sampling` may not touch them
/// (python `provider._OWNED_KEYS`).
const OWNED_KEYS: [&str; 6] = [
    "model",
    "input",
    "instructions",
    "max_output_tokens",
    "stream",
    "store",
];

/// python `int(v or default)`: a falsy value takes the default; an int is itself;
/// a float truncates; a bool is 0/1; a numeric string parses. Anything else is
/// python's `int()` error.
fn py_int_or(v: Option<&Value>, default: u64) -> Result<i64, String> {
    let default = i64::try_from(default).unwrap_or(i64::MAX);
    let Some(v) = v.filter(|v| py_truthy(v)) else {
        return Ok(default);
    };
    match v {
        Value::Bool(_) => Ok(1),
        Value::Number(n) => Ok(n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or(default)),
        Value::String(s) => {
            let t = s.trim();
            let digits_ok = !t.is_empty() && !t.starts_with('_') && !t.ends_with('_');
            t.replace('_', "")
                .parse::<i64>()
                .ok()
                .filter(|_| digits_ok)
                .ok_or_else(|| {
                    format!(
                        "invalid literal for int() with base 10: {}",
                        fantastic_bundle::py_repr(s)
                    )
                })
        }
        Value::Array(_) => Err(
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                .to_string(),
        ),
        _ => Err(
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                .to_string(),
        ),
    }
}

/// `reflect` — python `_make_reflect` with no `reflect_extra`.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    Value::Object(verbs::reflect_base(
        agent_id,
        kernel,
        SENTENCE,
        CFG.model_window,
    ))
}

#[cfg(test)]
mod tests;
