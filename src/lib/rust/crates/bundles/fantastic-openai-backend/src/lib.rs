//! openai_backend — a provider on any OpenAI-compatible `/chat/completions`
//! server (llama.cpp's llama-server, LM Studio, …). Streams raw text tokens.
//! Thin shell over `fantastic-ai-core`, as `ollama_backend` is: this crate
//! supplies only the `Provider` (SSE transport) + a `Bundle` that dispatches
//! every verb through ai-core.
//!
//! ### HTTP
//!
//! `POST {endpoint}/chat/completions` with `{model, messages, stream:true,
//! stream_options:{include_usage:true}, max_tokens, temperature?, stop?}` (`stop`: the
//! chat's knob) → SSE; each
//! `data:` line carries `{choices:[{delta:{content?, reasoning_content?}}]}`
//! and the last one the `usage`. BOTH text channels are read (a reasoning model
//! streams its deliberation as `reasoning_content`, some servers as
//! `reasoning`); the deliberation is wrapped in `<think>…</think>` once per turn
//! (`fantastic_ai_core::think`). RAW MODE: NO `tools` array is sent and NO
//! `tool_calls` are read — ai-core parses the `<call name=…>` envelope from the
//! content text.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::constants::DEFAULT_TURN_TOKENS;
use fantastic_ai_core::credentials::KeySource;
use fantastic_ai_core::declared::provider;
use fantastic_ai_core::provider::{
    stream_idle_of, within, Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream,
    ReportCell, Unconfigured,
};
use fantastic_ai_core::record::{Credentials, ProviderRecord};
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, verbs};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use schemars::JsonSchema;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "openai_backend.tools";

/// The name a required-field refusal carries.
const BACKEND: &str = "openai_backend";


const CFG: BackendConfig = BackendConfig {
    name: "openai_backend",
    // the server's window is the record's to name (`context_window`)
    model_window: fantastic_ai_core::constants::DEFAULT_CONTEXT_WINDOW,
};

// ── provider (SSE transport) ────────────────────────────────────────

struct OpenAiProvider {
    endpoint: String,
    model: String,
    /// The chat's `temperature` knob; `None` sends none, so the server's own setting applies.
    temperature: Option<f64>,
    max_tokens: u64,
    /// The chat's `stop` knob (fantastic#632): strings that end the completion, sent as is.
    stop: Option<Value>,
    key: Arc<dyn KeySource>,
    report: ReportCell,
    stream_idle: Duration,
}

#[async_trait]
impl Provider for OpenAiProvider {
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
        if self.model.is_empty() {
            return Err(ProviderError::raised(
                "openai_backend: no `model` set — this connector does not guess one. \
                 Name a model on the agent record (model=...).",
            ));
        }
        // the agent's cache marks are anthropic's; the OpenAI wire takes role and content only
        let messages: Vec<Value> = messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(o) = m.as_object_mut() {
                    o.remove(fantastic_ai_core::caching::CACHE_MARK);
                }
                m
            })
            .collect();
        // A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
        let cap = max_tokens.map(|n| n.max(1) as u64).unwrap_or(self.max_tokens);
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": cap,
        });
        if let (Some(t), Some(obj)) = (self.temperature, body.as_object_mut()) {
            obj.insert("temperature".into(), json!(t));
        }
        if let (Some(stop), Some(obj)) = (&self.stop, body.as_object_mut()) {
            obj.insert("stop".into(), stop.clone());
        }
        let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
        let key = self.key.key(&self.endpoint).await?.filter(|k| !k.is_empty());
        let mut req = reqwest::Client::new().post(&url).json(&body);
        if let Some(key) = &key {
            req = req.bearer_auth(key);
        }
        let sent = within(self.stream_idle, req.send())
            .await
            .map_err(|s| s.error(BACKEND, &self.endpoint))?;
        let resp =
            sent.map_err(|e| ProviderError::transport(format!("openai: request failed: {e}")))?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let error = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    let e = v.get("error")?;
                    e.get("message").and_then(Value::as_str).or_else(|| e.as_str()).map(str::to_string)
                })
                .unwrap_or(text);
            return Err(ProviderError {
                message: format!("{error} (status code: {code})"),
                kind: "ResponseError",
                // the shared classifier: a 5xx is the link, a 401 or 404 no retry mends
                class: fantastic_ai_core::provider::http_class(code),
            });
        }
        Ok(consume_sse(resp, Arc::clone(&self.report), self.stream_idle, self.endpoint.clone()))
    }
}

/// The SSE body as a LIVE stream of content tokens, per `data:` line.
fn consume_sse(
    resp: reqwest::Response,
    report: ReportCell,
    stream_idle: Duration,
    endpoint: String,
) -> ProviderStream {
    let inner = resp.bytes_stream();
    let init = (inner, SseState { buf: Vec::new(), wrap: ThinkWrap::new(), report }, VecDeque::new(), false);
    Box::pin(futures_util::stream::unfold(
        init,
        move |(mut inner, mut st, mut pending, mut finished)| {
            let endpoint = endpoint.clone();
            async move {
                loop {
                    if let Some(ev) = pending.pop_front() {
                        return Some((ev, (inner, st, pending, finished)));
                    }
                    if finished {
                        return None;
                    }
                    let chunk = match within(stream_idle, inner.next()).await {
                        Ok(chunk) => chunk,
                        Err(stalled) => {
                            finished = true;
                            pending.push_back(Err(stalled.error(BACKEND, &endpoint)));
                            continue;
                        }
                    };
                    let Some(chunk_res) = chunk else {
                        finished = true;
                        if !st.buf.is_empty() {
                            let line = std::mem::take(&mut st.buf);
                            decode_line(&line, &mut st, &mut pending);
                        }
                        if let Some(m) = st.wrap.close_marker() {
                            pending.push_back(Ok(ProviderEvent::Token(m.to_string())));
                        }
                        continue;
                    };
                    match chunk_res {
                        Ok(bytes) => st.buf.extend_from_slice(&bytes),
                        Err(e) => {
                            finished = true;
                            pending.push_back(Err(ProviderError::transport(format!(
                                "openai: stream error: {e}"
                            ))));
                            continue;
                        }
                    }
                    while let Some(pos) = st.buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = st.buf.drain(..=pos).collect();
                        decode_line(&line[..line.len() - 1], &mut st, &mut pending);
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
}

/// One SSE line: `data: {json}` (or `data: [DONE]`); anything else is skipped.
fn decode_line(line: &[u8], st: &mut SseState, out: &mut VecDeque<Result<ProviderEvent, ProviderError>>) {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches('\r');
    let Some(data) = line.strip_prefix("data:") else {
        return;
    };
    let data = data.trim_start();
    if data.is_empty() || data == "[DONE]" {
        return;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(data) else {
        return;
    };
    note_usage(&parsed, &st.report);
    decode_chunk_into(&parsed, &mut st.wrap, out);
}

/// `usage` (the last chunk, with `include_usage`) and `finish_reason`
/// (`length` ⇒ `max_tokens`: the output stopped at its bound).
fn note_usage(chunk: &Value, report: &ReportCell) {
    let mut r = report.lock().expect("report poisoned");
    if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
        if let Some(n) = u.get("prompt_tokens").and_then(Value::as_i64) {
            r.prompt_tokens = Some(n);
        }
        if let Some(n) = u.get("completion_tokens").and_then(Value::as_i64) {
            r.completion_tokens = Some(n);
        }
    }
    let reason = chunk
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str);
    if let Some(fr) = reason {
        r.stop_reason = Some(if fr == "length" { "max_tokens".to_string() } else { fr.to_string() });
    }
}

/// One chunk's `choices[0].delta` into tokens, reading BOTH text channels.
fn decode_chunk_into(
    parsed: &Value,
    wrap: &mut ThinkWrap,
    out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
) {
    let Some(delta) = parsed.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("delta")) else {
        return;
    };
    let think = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .or_else(|| delta.get("reasoning").and_then(Value::as_str))
        .unwrap_or("");
    if !think.is_empty() {
        if let Some(m) = wrap.open_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        out.push_back(Ok(ProviderEvent::Token(think.to_string())));
    }
    // RAW: text only. ai-core parses any `<call name=…>` envelope out of it.
    let content = delta.get("content").and_then(Value::as_str).unwrap_or("");
    if !content.is_empty() {
        if let Some(m) = wrap.close_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        out.push_back(Ok(ProviderEvent::Token(content.to_string())));
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

const MAN: &str = "An LLM provider on an OpenAI-compatible server (/chat/completions, raw text): it streams one completion pass for a chat that names it, lists its models and keeps a default model.";

/// The fields an openai_backend record carries.
#[derive(Debug, JsonSchema)]
pub struct OpenAiRecord {
    /// The server's base URL, up to `/v1` (the provider appends `/chat/completions`).
    #[schemars(length(min = 1))]
    pub endpoint: String,
    /// The model name the server serves.
    pub model: Option<String>,
    /// Sampling temperature; left out, none is sent and the server's own setting applies.
    pub temperature: Option<f64>,
    /// A turn's output budget (tokens).
    #[schemars(range(min = 1), default = "default_max_tokens")]
    pub max_tokens: Option<i64>,
    /// The read clock of the answer stream.
    #[serde(flatten)]
    pub clock: fantastic_ai_core::record::StreamClock,
    /// How the conversation is kept in the model's view.
    pub caching: Option<fantastic_ai_core::record::CachingMode>,
    /// A door's key, when the server sits behind one — absent = no header.
    pub credentials: Option<Credentials>,
    /// The memory store agent that keeps this agent's settings (e.g. a ram_store).
    pub memory_store_id: String,
    /// The fields every provider takes.
    #[serde(flatten)]
    pub provider: ProviderRecord,
}

fn default_max_tokens() -> Option<i64> {
    Some(DEFAULT_TURN_TOKENS)
}

fantastic_kernel::record!(OpenAiRecord);

/// The OpenAI-compatible backend bundle.
pub struct OpenAiBackendBundle;

#[async_trait]
impl Handler for OpenAiBackendBundle {
    type Call = provider::Call;

    fn name(&self) -> &str {
        "openai_backend"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        provider::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        provider::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        fantastic_ai_core::remote_provider::handle(
            agent_id,
            call,
            kernel,
            Some(KEYS),
            None,
            build_provider_with,
            fantastic_ai_core::remote_provider::NO_LIVE,
            || reflect_reply(agent_id, kernel),
        )
        .await
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

/// How this backend reads its key: a door's, optional.
const KEYS: fantastic_ai_core::remote_provider::Keys =
    fantastic_ai_core::remote_provider::Keys { backend: BACKEND, required: false };

/// The provider this agent is configured for, built per call with the key read for
/// it. `endpoint` and `model` are REQUIRED — nothing is guessed.
fn build_provider_with(meta: Map<String, Value>, key: Arc<dyn KeySource>) -> Arc<dyn Provider> {
    let (endpoint, model) = match (
        helpers::required_meta(&meta, "endpoint", BACKEND),
        helpers::required_meta(&meta, "model", BACKEND),
    ) {
        (Ok(e), Ok(m)) => (e, m),
        (Err(e), _) | (_, Err(e)) => return Arc::new(Unconfigured(e)),
    };
    Arc::new(OpenAiProvider {
        endpoint,
        model,
        temperature: meta.get("temperature").and_then(Value::as_f64),
        max_tokens: RECORD
            .get(&meta, "max_tokens")
            .and_then(|v| v.as_u64())
            .expect("max_tokens declares its default"),
        stop: stop_of(&meta),
        key,
        report: ReportCell::default(),
        stream_idle: stream_idle_of(&meta),
    })
}

/// The chat's `stop` knob, laid over the record by `complete`: a string or a list of
/// strings; anything else (or an empty list) sends none.
fn stop_of(meta: &Map<String, Value>) -> Option<Value> {
    match meta.get("stop")? {
        Value::String(s) if !s.is_empty() => Some(json!([s])),
        Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_string) => Some(Value::Array(a.clone())),
        _ => None,
    }
}

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let mut out = verbs::reflect_base(agent_id, kernel, MAN, CFG.model_window);
    let meta = helpers::agent_meta(agent_id, kernel);
    for key in ["temperature", "max_tokens"] {
        out.insert(key.to_string(), RECORD.get(&meta, key).unwrap_or(Value::Null));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests;
