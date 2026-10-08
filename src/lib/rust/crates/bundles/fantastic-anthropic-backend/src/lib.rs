//! anthropic_backend — an LLM agent over Anthropic's Messages API. Mirrors python's
//! `anthropic_backend` (canonical): every refusal text and reflect key is python's.
//!
//! `POST {endpoint}` (the record's Messages URL) with `stream: true`. NO native
//! tools: no `tools` array is sent; ai-core parses the `<call>` elements out of the
//! streamed text, as for every backend.
//!
//! - ONE `system` string, top level — every system message joined; `messages` holds
//!   only `user` / `assistant`, consecutive turns of one role merged.
//! - `max_tokens` on every request (the API requires it): the record's (read from
//!   the model entry when left out, [`server`]), or a side call's own bound.
//! - THE THOUGHT CHANNEL: with `thinking_budget` set, `thinking_delta`s stream
//!   before the answer, wrapped once in `<think>…</think>`.
//! - The SSE stream ends with `message_stop`; a stream that ends without it did not
//!   finish. An `error` event is raised with the API's own type and message.
//! - THE KEY: the record's optional `credentials` ref, sent as `x-api-key` (with
//!   `anthropic-version`); none, no key header.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, ReportCell,
    Unconfigured,
};
use fantastic_ai_core::schema::{self, Field};
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, verbs};
use fantastic_bundle::{py_float, py_repr_value};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub mod server;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "anthropic_backend.tools";

/// The bundle readme — python's, byte for byte.
pub const README: &str = include_str!("readme.md");

/// The name every refusal carries.
const NAME: &str = "anthropic_backend";

/// What `reflect` says this agent IS — python `tools.py`'s `sentence`.
pub const SENTENCE: &str = "Anthropic Messages-API LLM agent (raw prompt-and-parse tool-calling).";

/// The API version every request names.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Seconds to reach the API. The stream has no read clock: the turn's one bound is
/// ai-core's send timeout.
pub const CONNECT_TIMEOUT: f64 = 15.0;
/// Below this no thinking is asked for: the API needs `budget_tokens >= 1024`.
pub const THINKING_FLOOR: i64 = 1024;

const CFG: BackendConfig = BackendConfig {
    name: NAME,
    model_window: fantastic_ai_core::constants::DEFAULT_CONTEXT_WINDOW,
};

/// ai-core's messages → (`system`, the user/assistant turns): every system message
/// joins the one `system` string; consecutive turns of one role merge, blank-line
/// separated. Python `to_messages`.
#[must_use]
pub fn to_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let mut systems: Vec<String> = Vec::new();
    let mut turns: Vec<(String, String)> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let content = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => fantastic_bundle::py_str(v),
        };
        match role {
            "system" => {
                if !content.is_empty() {
                    systems.push(content);
                }
            }
            "user" | "assistant" => match turns.last_mut() {
                Some((r, c)) if r == role => {
                    c.push_str("\n\n");
                    c.push_str(&content);
                }
                _ => turns.push((role.to_string(), content)),
            },
            _ => {}
        }
    }
    let turns = turns
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    (systems.join("\n\n"), turns)
}

/// The Messages provider. PURE RAW TEXT.
struct AnthropicProvider {
    endpoint: String,
    model: String,
    key: Arc<dyn KeySource>,
    max_tokens: i64,
    temperature: Option<f64>,
    thinking_budget: Option<i64>,
    report: ReportCell,
}

impl AnthropicProvider {
    /// The request body. Python `AnthropicProvider.body`.
    fn body(&self, messages: &[Value], max_tokens: Option<i64>) -> Value {
        let (system, turns) = to_messages(messages);
        let call_max = max_tokens.unwrap_or(self.max_tokens);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(call_max));
        body.insert("messages".into(), Value::Array(turns));
        body.insert("stream".into(), json!(true));
        if !system.is_empty() {
            body.insert("system".into(), json!(system));
        }
        if let Some(t) = self.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(b) = self
            .thinking_budget
            .filter(|b| *b >= THINKING_FLOOR && *b < call_max)
        {
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": b}),
            );
        }
        Value::Object(body)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
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
        let body = self.body(messages, max_tokens);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs_f64(CONNECT_TIMEOUT))
            .build()
            .map_err(|e| ProviderError::raised(format!("{NAME}: http client: {e}")))?;
        let key = self
            .key
            .key(&self.endpoint)
            .await?
            .filter(|k| !k.is_empty());
        let mut req = client
            .post(&self.endpoint)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body);
        if let Some(key) = &key {
            req = req.header("x-api-key", key);
        }
        let resp = req.send().await.map_err(|e| {
            ProviderError::raised(if e.is_timeout() {
                format!(
                    "{NAME}: no answer from {} within {CONNECT_TIMEOUT}s",
                    self.endpoint
                )
            } else {
                format!("{NAME}: could not connect to {}", self.endpoint)
            })
        })?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let detail: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect();
            let why = if matches!(status, 401 | 403) {
                " — the API wants a key: name it in `credentials` ({store, ref})"
            } else {
                ""
            };
            return Err(ProviderError::raised(format!(
                "{NAME}: {} answered HTTP {status}{why}: {detail}",
                self.endpoint
            )));
        }
        Ok(consume_sse(
            resp,
            Arc::clone(&self.report),
            self.endpoint.clone(),
            max_tokens.is_none().then_some(self.max_tokens),
        ))
    }
}

/// The SSE body as a LIVE stream of tokens, per line.
fn consume_sse(
    resp: reqwest::Response,
    report: ReportCell,
    endpoint: String,
    main_turn_max: Option<i64>,
) -> ProviderStream {
    let inner = resp.bytes_stream();
    let init = (
        inner,
        SseState {
            buf: Vec::new(),
            wrap: ThinkWrap::new(),
            report,
            stopped: false,
            endpoint,
            main_turn_max,
        },
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
                    st.end(&mut pending);
                    continue;
                };
                match chunk_res {
                    Ok(bytes) => st.buf.extend_from_slice(&bytes),
                    Err(_) => {
                        finished = true;
                        pending.push_back(Err(ProviderError::raised(format!(
                            "{NAME}: {} dropped the stream mid-answer; this turn is NOT \
                             complete",
                            st.endpoint
                        ))));
                        continue;
                    }
                }
                while let Some(nl) = st.buf.iter().position(|b| *b == b'\n') {
                    let line_bytes: Vec<u8> = st.buf.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                        .trim_end_matches('\r')
                        .to_string();
                    if let Err(e) = st.decode_line(&line, &mut pending) {
                        finished = true;
                        pending.push_back(Err(e));
                        break;
                    }
                }
            }
        },
    ))
}

struct SseState {
    buf: Vec<u8>,
    wrap: ThinkWrap,
    report: ReportCell,
    stopped: bool,
    endpoint: String,
    /// `Some(bound)` on a MAIN turn, where a cut is a failure.
    main_turn_max: Option<i64>,
}

impl SseState {
    fn decode_line(
        &mut self,
        line: &str,
        out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let Some(rest) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = rest.trim();
        if data.is_empty() {
            return Ok(());
        }
        let evt: Value = serde_json::from_str(data).map_err(|e| ProviderError {
            message: e.to_string(),
            kind: "JSONDecodeError",
        })?;
        match evt.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let u = &evt["message"]["usage"];
                let total: i64 = [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .map(|k| u.get(*k).and_then(Value::as_i64).unwrap_or(0))
                .sum();
                self.report.lock().expect("report poisoned").prompt_tokens =
                    (total > 0).then_some(total);
            }
            "content_block_delta" => {
                let delta = &evt["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        let t = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            if let Some(m) = self.wrap.open_marker() {
                                out.push_back(Ok(ProviderEvent::Token(m.to_string())));
                            }
                            out.push_back(Ok(ProviderEvent::Token(t.to_string())));
                        }
                    }
                    Some("text_delta") => {
                        let t = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            if let Some(m) = self.wrap.close_marker() {
                                out.push_back(Ok(ProviderEvent::Token(m.to_string())));
                            }
                            out.push_back(Ok(ProviderEvent::Token(t.to_string())));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = evt["delta"].get("stop_reason").and_then(Value::as_str) {
                    self.report.lock().expect("report poisoned").stop_reason = Some(r.to_string());
                }
            }
            "message_stop" => self.stopped = true,
            "error" => {
                let err = &evt["error"];
                return Err(ProviderError::raised(format!(
                    "{NAME}: {} stream error: {}: {}",
                    self.endpoint,
                    fantastic_bundle::py_str(err.get("type").unwrap_or(&Value::Null)),
                    fantastic_bundle::py_str(err.get("message").unwrap_or(&Value::Null)),
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, out: &mut VecDeque<Result<ProviderEvent, ProviderError>>) {
        if let Some(m) = self.wrap.close_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        if !self.stopped {
            out.push_back(Err(ProviderError::raised(format!(
                "{NAME}: the stream from {} ended without message_stop; this turn is NOT \
                 complete",
                self.endpoint
            ))));
            return;
        }
        let cut = self
            .report
            .lock()
            .expect("report poisoned")
            .stop_reason
            .as_deref()
            == Some("max_tokens");
        if let (true, Some(bound)) = (cut, self.main_turn_max) {
            out.push_back(Err(ProviderError::raised(format!(
                "{NAME}: the turn was cut at max_tokens={bound} (stop_reason=max_tokens) — it \
                 did not finish; raise max_tokens on the record, or lower thinking_budget"
            ))));
        }
    }
}

// ─── the record ─────────────────────────────────────────────────────────────

fn positive_int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64).filter(|n| *n >= 1)
}

fn required_int(meta: &Map<String, Value>, key: &str) -> Result<i64, ProviderError> {
    positive_int(meta.get(key)).ok_or_else(|| {
        ProviderError::value_error(format!(
            "{NAME}: `{key}` is not on the agent record, a positive int, and was not read \
             from the API (update_agent <id> {key}=…)"
        ))
    })
}

fn build_provider(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
    let meta = helpers::agent_meta(agent_id, kernel);
    let built = (|| -> Result<AnthropicProvider, ProviderError> {
        Ok(AnthropicProvider {
            endpoint: helpers::required_meta(&meta, "endpoint", NAME)?
                .trim_end_matches('/')
                .to_string(),
            model: helpers::required_meta(&meta, "model", NAME)?,
            key: Arc::new(key_reader(agent_id, kernel, NAME, false)),
            max_tokens: required_int(&meta, "max_tokens")?,
            temperature: meta.get("temperature").and_then(Value::as_f64),
            thinking_budget: positive_int(meta.get("thinking_budget")),
            report: ReportCell::default(),
        })
    })();
    match built {
        Ok(p) => Arc::new(p),
        Err(e) => Arc::new(Unconfigured(e)),
    }
}

/// `reflect` — the shared base body, then what the next request will send.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Result<Value, BundleError> {
    let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE, CFG.model_window);
    let meta = helpers::agent_meta(agent_id, kernel);
    for k in ["max_tokens", "temperature", "thinking_budget"] {
        let v = meta.get(k).cloned().unwrap_or(Value::Null);
        let v = if k == "temperature" && !v.is_null() {
            json!(py_float(&v)?)
        } else {
            v
        };
        out.insert(k.to_string(), v);
    }
    Ok(Value::Object(out))
}

/// Refuse a write this backend cannot honour, AT THE WRITE, on the MERGED record —
/// then every required field, named at once. Python `validate_meta`, text for text.
#[must_use]
pub fn validate(meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
    let mut merged = record.clone();
    for (k, v) in meta {
        merged.insert(k.clone(), v.clone());
    }
    for key in ["max_tokens", "context_window", "thinking_budget"] {
        if let Some(v) = meta.get(key).filter(|v| !v.is_null()) {
            if positive_int(Some(v)).is_none() || v.is_boolean() {
                return Some(format!("{key} must be a positive int"));
            }
        }
    }
    if let Some(t) = meta.get("temperature").filter(|v| !v.is_null()) {
        if !t.is_number() {
            return Some("temperature must be a number".to_string());
        }
    }
    if let Some(budget) = positive_int(merged.get("thinking_budget")) {
        if budget < THINKING_FLOOR {
            return Some(format!(
                "thinking_budget={budget} is below the API's {THINKING_FLOOR}"
            ));
        }
        if let Some(mx) = positive_int(merged.get("max_tokens")) {
            if budget >= mx {
                return Some(format!(
                    "thinking_budget={budget} is not below max_tokens={mx} — the API refuses \
                     it; keep it below max_tokens"
                ));
            }
        }
    }
    let endpoint = merged
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let left = server::missing(&merged);
    if !endpoint.is_empty() && !left.is_empty() && server::models_url(&endpoint).is_none() {
        return Some(format!(
            "{} is left to the API, but the endpoint '{endpoint}' does not end in /messages, \
             so the model list is not known — set {}",
            left[0],
            left.join(", ")
        ));
    }
    schema::validate(record_schema(), NAME, meta, record)
}

/// THE RECORD SCHEMA — python `RECORD_SCHEMA`, field for field.
#[must_use]
pub fn record_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        const FROM_API: &str = "; left out, read from the API before a turn and written here";
        schema::schema(vec![
            Field::new(
                "endpoint",
                "str",
                "the Messages API URL, e.g. https://api.anthropic.com/v1/messages",
            )
            .required(),
            Field::new("model", "str", "the model id, e.g. claude-sonnet-5").required(),
            Field::new(
                "max_tokens",
                "int",
                format!("a turn's output budget (tokens){FROM_API} (the model's max_tokens)"),
            ),
            Field::new(
                "context_window",
                "int",
                format!("the model's input window (tokens){FROM_API} (max_input_tokens)"),
            ),
            Field::new(
                "temperature",
                "float",
                "sampling temperature; sent only when set",
            ),
            Field::new(
                "thinking_budget",
                "int",
                format!(
                    "extended thinking per turn (tokens, at least {THINKING_FLOOR}, below \
                     max_tokens); unset, no thinking is asked for"
                ),
            ),
            // the API key — optional on the record: absent, no key header goes out
            schema::credentials_field(false),
        ])
    })
}

/// The Anthropic backend bundle.
pub struct AnthropicBundle;

#[async_trait]
impl Bundle for AnthropicBundle {
    fn name(&self) -> &str {
        NAME
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

    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        validate(meta, record)
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
            return Ok(Some(reflect_reply(agent_id, kernel)?));
        }
        // A verb that runs a turn first reads what the record leaves to the API; an
        // API that does not answer refuses the verb, naming the field.
        if server::TURN_VERBS.contains(&verb) {
            if let Some(refused) = server::resolve(agent_id, kernel).await {
                return Ok(Some(refused));
            }
        }
        let provider = |_client: &str| build_provider(agent_id, kernel);
        if let Some(reply) = verbs::handle_common(agent_id, payload, kernel, CFG, provider).await? {
            return Ok(Some(reply));
        }
        Ok(Some({
            let raw = payload.get("type").cloned().unwrap_or(Value::Null);
            let mut r = fantastic_kernel::refusal::refusal(
                fantastic_kernel::refusal::UNHANDLED,
                format!("{NAME}: unknown type {}", py_repr_value(&raw)),
            );
            fantastic_kernel::refusal::with(&mut r, "verb", raw);
            r
        }))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        verbs::on_delete(agent_id, kernel).await;
        Ok(())
    }

    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await
    }
}

#[cfg(test)]
mod tests;
