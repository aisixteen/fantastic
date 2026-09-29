//! `llama-server` (llama.cpp) LLM bundle.
//!
//! Mirrors python's `llamacpp_backend`. llama-server speaks the
//! OpenAI-compatible surface at `<endpoint>/chat/completions` and streams RAW
//! TEXT via SSE, so the per-delta decode is the SHARED
//! [`fantastic_ai_core::think::decode_openai_delta`] — the same one the NIM
//! backend uses.
//!
//! NO NATIVE TOOL-CALLING: the provider streams text and ai-core parses the
//! `<call name=…>` envelope out of it.
//!
//! Three deliberate differences from the hosted backends:
//!
//! - **No api_key gate.** A local server usually has none, so this bundle is
//!   available by default instead of refusing until a key is set. A bearer
//!   token is still sent when the record carries one.
//! - **No model default.** llama-server serves exactly ONE model (whichever
//!   `-hf`/`-m` loaded) and IGNORES the request's `model` field, so nothing is
//!   sent unless the record names one — naming a default would be a guess that
//!   the server discards anyway.
//! - **No context-window knob.** Fixed at server start (`-c N`); there is no
//!   per-request equivalent, so this bundle does not pretend to own one.

use async_trait::async_trait;
use fantastic_ai_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, ReportCell,
    Unconfigured,
};
use fantastic_ai_core::think::{decode_openai_delta, ThinkWrap};
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle::{py_float, py_int, py_repr_value, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "llamacpp_backend.tools";

pub const README: &str = include_str!("readme.md");

/// The name a required-field refusal carries.
const BACKEND: &str = "llamacpp_backend";

/// A SENTINEL, never sent on the wire — python `llamacpp_backend/provider.py`.
/// `llama-server` serves exactly ONE model (whichever `-hf`/`-m` loaded it) and
/// IGNORES the request's `model` field, so there is no model to guess and none
/// to get wrong; this string exists only so `reflect` can say what is true.
pub const DEFAULT_MODEL: &str = "(server-loaded)";

/// What `reflect` says this agent IS — python `llamacpp_backend/tools.py`'s
/// `sentence`, verbatim.
pub const SENTENCE: &str = "llama.cpp-backed local LLM agent (raw prompt-and-parse tool-calling).";

/// Sampling temperature when the record names none. An agentic backend must
/// emit EXACT tool calls, not improvise, so the default is LOW — python
/// `provider.py:51`, which folds the default in at CONSTRUCTION, so the key is
/// on the wire of every request.
pub const DEFAULT_TEMPERATURE: f64 = 0.3;

pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;
pub const SEND_TIMEOUT_SECS: f64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

const CFG: BackendConfig = BackendConfig {
    name: "llamacpp_backend",
    provider_missing_error: None,
};

// ── provider ────────────────────────────────────────────────────────

struct LlamaCppProvider {
    endpoint: String,
    /// What the RECORD asked for, if anything. Usually `None` — see the module
    /// note on why no default is invented here.
    requested_model: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u64>,
    /// ALWAYS SENT. python resolves the default in `__init__`, so `temperature`
    /// is never absent from the body; rust omitted the field entirely when the
    /// record named none, which let the SERVER's default (often 0.8) decide how
    /// literal a tool-calling model was.
    temperature: f64,
    /// `None` means DO NOT SEND the kwarg: a model without a thinking gate
    /// rejects an unknown one, so this is three-valued on purpose.
    think: Option<bool>,
    /// What the endpoint reported about the last pass — `usage.prompt_tokens`
    /// and the `finish_reason` (`length` ⇒ `max_tokens`).
    report: ReportCell,
}

#[async_trait]
impl Provider for LlamaCppProvider {
    fn model(&self) -> String {
        // Reporting a guess here would be a lie that `reflect` then repeats;
        // python names the same sentinel (`provider.model`).
        self.requested_model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
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
        // RAW: no native `tools`/`tool_choice` — ai-core parses the `<tool_call>`
        // envelope out of the streamed text.
        let mut body = serde_json::Map::new();
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));
        // PROVIDER-REPORTED USAGE on the last chunk (`usage.prompt_tokens`).
        body.insert("stream_options".into(), json!({"include_usage": true}));
        if let Some(m) = &self.requested_model {
            body.insert("model".into(), json!(m));
        }
        body.insert("temperature".into(), json!(self.temperature));
        if let Some(n) = max_tokens {
            // a side call's own bound (a fold summary's budget)
            body.insert("max_tokens".into(), json!(n));
        } else if let Some(n) = self.max_tokens {
            body.insert("max_tokens".into(), json!(n));
        }
        if let Some(t) = self.think {
            body.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": t }),
            );
        }
        let body = Value::Object(body);

        let mut req = reqwest::Client::new()
            .post(format!(
                "{}/chat/completions",
                self.endpoint.trim_end_matches('/')
            ))
            .json(&body);
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::raised(format!("http: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ProviderError::raised(format!(
                "send: provider HTTP {}",
                status.as_u16()
            )));
        }
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
/// PURE RAW TEXT — no native `tool_calls` are read.
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
                        pending.push_back(Err(ProviderError::raised(format!("stream: {e}"))));
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
        for piece in decode_openai_delta(&delta, &mut self.wrap) {
            out.push_back(Ok(ProviderEvent::Token(piece)));
        }
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

// ── verbs ───────────────────────────────────────────────────────────

fn meta_bool(agent_id: &AgentId, kernel: &Arc<Kernel>, key: &str) -> Option<bool> {
    helpers::agent_meta(agent_id, kernel)
        .get(key)
        .and_then(Value::as_bool)
}

fn meta_u64(agent_id: &AgentId, kernel: &Arc<Kernel>, key: &str) -> Option<u64> {
    helpers::agent_meta(agent_id, kernel)
        .get(key)
        .and_then(Value::as_u64)
}

/// The provider this agent is configured for. Built per call (it is cheap and the
/// record can change between turns), and shared by `send` and the verbs that need
/// a completion of their own — `context_recall` asks the model which rows to keep
/// in full. `None` when the backend is not configured to reach a model at all;
/// the caller says so in its own words rather than guessing a provider.
fn build_provider(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    _client_id: &str,
) -> Option<Arc<dyn Provider>> {
    // `endpoint` is REQUIRED on the record — nothing is guessed. `model` is
    // optional: llama-server serves the one model it loaded.
    let meta = helpers::agent_meta(agent_id, kernel);
    let endpoint = match helpers::required_meta(&meta, "endpoint", BACKEND) {
        Ok(e) => e,
        Err(e) => return Some(Arc::new(Unconfigured(e))),
    };
    Some(Arc::new(LlamaCppProvider {
        endpoint,
        requested_model: helpers::meta_string(agent_id, kernel, "model"),
        api_key: helpers::meta_string(agent_id, kernel, "api_key"),
        max_tokens: meta_u64(agent_id, kernel, "num_predict"),
        temperature: helpers::agent_meta(agent_id, kernel)
            .get("temperature")
            .filter(|v| !v.is_null())
            .and_then(|v| py_float(v).ok())
            .unwrap_or(DEFAULT_TEMPERATURE),
        think: meta_bool(agent_id, kernel, "think"),
        report: ReportCell::default(),
    }))
}

/// `reflect` — python `_make_reflect` + `llamacpp_backend/tools.py`'s
/// `_reflect_extra`: the shared base body, then the sampling knobs. `num_ctx` is
/// deliberately ABSENT (llama-server fixes the window at start).
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Result<Value, BundleError> {
    let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE);
    let meta = helpers::agent_meta(agent_id, kernel);
    out.insert(
        "temperature".to_string(),
        match meta.get("temperature").filter(|v| !v.is_null()) {
            Some(v) => json!(py_float(v)?),
            None => json!(DEFAULT_TEMPERATURE),
        },
    );
    out.insert(
        "num_predict".to_string(),
        match meta.get("num_predict").filter(|v| py_truthy(v)) {
            Some(v) => json!(py_int(v)?),
            None => Value::Null,
        },
    );
    out.insert(
        "think".to_string(),
        match meta.get("think").filter(|v| !v.is_null()) {
            Some(v) => json!(py_truthy(v)),
            None => Value::Null,
        },
    );
    out.insert(
        "has_api_key".to_string(),
        json!(meta.get("api_key").is_some_and(py_truthy)),
    );
    Ok(Value::Object(out))
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The llama.cpp backend bundle.
pub struct LlamaCppBundle;

#[async_trait]
impl Bundle for LlamaCppBundle {
    fn name(&self) -> &str {
        "llamacpp_backend"
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
            return Ok(Some(reflect_reply(agent_id, kernel)?));
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
                "llamacpp_backend: unknown type {}",
                py_repr_value(&payload.get("type").cloned().unwrap_or(Value::Null))
            )
        })))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        state::drop_state(agent_id);
        Ok(())
    }

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

#[cfg(test)]
mod tests;
