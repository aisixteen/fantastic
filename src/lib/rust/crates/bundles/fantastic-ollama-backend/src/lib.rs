//! ollama_backend — local LLM (ollama) bundle. Streams raw text tokens.
//! Thin shell over `fantastic-ai-core`: this crate supplies only the ollama
//! `Provider` (NDJSON transport) + a `Bundle` that dispatches every verb through
//! ai-core. The per-client chat threads, FIFO lock, menu cache, prompt assembly,
//! agentic loop (incl. RAW tool-call parsing), status phase machine, and history
//! persistence all live in ai-core.
//!
//! See `fantastic_ai_core`'s module header for the canonical LLM backend
//! contract (verbs + events).
//!
//! ### HTTP
//!
//! `POST {endpoint}/api/chat` with `{model, messages, stream:true}` →
//! line-delimited JSON; each line carries `{message:{content?, thinking?}}`.
//! BOTH text channels are read: a reasoning model streams its deliberation as
//! `thinking` and only its conclusion as `content`, so reading `content` alone
//! reports the whole turn as an empty completion. The deliberation is wrapped in
//! `<think>…</think>` once per turn (see `fantastic_ai_core::think`). NO `tools`
//! array is sent and NO `tool_calls` are read — tool-calling is RAW
//! prompt-and-parse owned by ai-core (the model emits a `<call name=…>` envelope
//! in the content text).

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use fantastic_ai_core::retrieval;
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle::{py_float, py_int, py_repr_value, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "ollama_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Hard ceiling per `send` (mirrors Python's `SEND_TIMEOUT`).
pub const SEND_TIMEOUT_SECS: f64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

/// Default `client_id` for callers that don't supply one.
pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;

/// Default ollama HTTP endpoint.
pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// NO guessy default (by default, no defaults): never name a network-reaching
/// model. The sentinel exists only so reflect can say what's true; the request
/// path fails loud when the model is unset/sentinel.
pub const DEFAULT_MODEL: &str = "(no default — set the `model` meta)";

/// What `reflect` says this agent IS — python `ollama_backend/tools.py`'s
/// `sentence`, verbatim.
pub const SENTENCE: &str = "Ollama-backed LLM agent (raw prompt-and-parse tool-calling).";

/// Default sampling temperature sent to ollama (overridable via the record's
/// `temperature` field). ollama's own default is 0.8 (creative) — wrong for an
/// agentic backend whose output is tool-calls + code. Mirrors the python
/// canonical default (0.3 — precise, with a touch of variety); a record
/// `temperature` overrides it.
pub const DEFAULT_TEMPERATURE: f64 = 0.3;

/// Per-backend config: cli round-trip routing, parallel tool dispatch.
const CFG: BackendConfig = BackendConfig {
    name: "ollama_backend",
    provider_missing_error: None,
};

// ── ollama provider (NDJSON transport) ──────────────────────────────

/// Provider for ollama's `/api/chat`. PURE RAW TEXT — decodes the NDJSON stream
/// into `ProviderEvent::Token` content only. NO native tools array is sent and
/// NO `tool_calls` are read; ai-core parses the `<call name=…>` envelope from text.
struct OllamaProvider {
    endpoint: String,
    model: String,
    temperature: f64,
    /// ollama context window (`num_ctx`). ollama's RUNTIME default is a small
    /// 4096 regardless of what the model supports (gemma4:12b, e.g., supports
    /// 262144) — an agentic loop's context (docs + history) overflows that and
    /// silently TRUNCATES the oldest tokens, dropping the system prompt/task and
    /// leaving the model to flail. Measured: raising it costs no throughput. The
    /// record's `num_ctx` lifts it; `None` = leave ollama's default.
    num_ctx: Option<u64>,
    /// Generation cap (`num_predict`). Unbounded, a weak model can ramble for
    /// thousands of tokens instead of emitting one action, blowing the send
    /// timeout. `None` = leave ollama's default (unbounded). CAUTION on a
    /// THINKING model: the cap is spent on reasoning tokens first, so too small a
    /// value truncates mid-thought and yields an EMPTY completion — pair it with
    /// `think:false`, or leave it unset.
    num_predict: Option<u64>,
    /// Reasoning toggle for THINKING models (`think`). A thinking model spends
    /// hundreds of hidden reasoning tokens before its answer — on gemma4:12b,
    /// ~250 tokens and ~11x the latency for the SAME one-line tool-call. An
    /// agentic backend wants the ACTION, not the rumination, so set `think:false`
    /// on the record. `None` = don't send it (model default) — non-thinking models
    /// reject the field, so it is opt-in.
    think: Option<bool>,
}

#[async_trait]
impl Provider for OllamaProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, ProviderError> {
        // NO guessy default: fail loud before the wire if no model is named.
        if self.model.is_empty() || self.model == DEFAULT_MODEL {
            return Err(ProviderError::raised(
                "ollama_backend: no `model` set — this connector does not \
                 guess one. Name a model on the agent record (model=...).",
            ));
        }
        let mut options = serde_json::Map::new();
        options.insert("temperature".to_string(), json!(self.temperature));
        if let Some(n) = self.num_ctx {
            options.insert("num_ctx".to_string(), json!(n));
        }
        if let Some(n) = self.num_predict {
            options.insert("num_predict".to_string(), json!(n));
        }
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "options": options,
        });
        // `think` is a TOP-LEVEL field (not an option). Only sent when set — a
        // non-thinking model rejects it.
        if let (Some(t), Some(obj)) = (self.think, body.as_object_mut()) {
            obj.insert("think".to_string(), json!(t));
        }
        let client = reqwest::Client::new();
        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::raised(format!("ollama: request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(ProviderError::raised(format!(
                "ollama: HTTP {}",
                resp.status()
            )));
        }

        Ok(consume_ndjson(resp))
    }
}

/// The NDJSON body as a LIVE stream of content tokens.
///
/// PER LINE, NOT PER BODY. This used to drain the whole response into a `Vec`
/// and hand it over only once the stream had ENDED — so nothing reached
/// `text_so_far`, the `token` events or the `streaming` phase until the
/// completion was already finished, and a long turn was indistinguishable from
/// a stalled request. Python's provider is an async generator over the response
/// lines; this yields on the same boundary.
fn consume_ndjson(resp: reqwest::Response) -> ProviderStream {
    let inner = resp.bytes_stream();
    // `pending` holds the events decoded from ONE chunk: a chunk can carry
    // several lines, and each line several tokens.
    let init = (inner, NdjsonState::new(), VecDeque::new(), false);
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
                    // A trailing line with no newline is still a chunk.
                    if !st.buf.is_empty() {
                        if let Ok(parsed) = serde_json::from_slice::<Value>(&st.buf) {
                            decode_chunk_into(&parsed, &mut st.wrap, &mut pending);
                        }
                        st.buf.clear();
                    }
                    // The turn ended while still reasoning (content never
                    // started) — close the span exactly once so the
                    // deliberation is strippable.
                    if let Some(m) = st.wrap.close_marker() {
                        pending.push_back(Ok(ProviderEvent::Token(m.to_string())));
                    }
                    continue;
                };
                match chunk_res {
                    Ok(bytes) => st.buf.extend_from_slice(&bytes),
                    Err(e) => {
                        // A span left open here stays unterminated on purpose:
                        // the stream was cut mid-thought, and `without_thinking`
                        // drops an unterminated tail rather than persisting a
                        // fragment.
                        finished = true;
                        pending.push_back(Err(ProviderError::raised(format!(
                            "ollama: stream error: {e}"
                        ))));
                        continue;
                    }
                }
                while let Some(pos) = st.buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = st.buf.drain(..=pos).collect();
                    let trimmed = &line[..line.len().saturating_sub(1)];
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_slice::<Value>(trimmed) else {
                        continue;
                    };
                    decode_chunk_into(&parsed, &mut st.wrap, &mut pending);
                }
            }
        },
    ))
}

/// Per-line NDJSON decode state — the buffer plus the per-TURN reasoning span.
struct NdjsonState {
    buf: Vec<u8>,
    wrap: ThinkWrap,
}

impl NdjsonState {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            wrap: ThinkWrap::new(),
        }
    }
}

/// Decode ONE ollama NDJSON chunk into content tokens, reading BOTH text
/// channels. `wrap` carries the per-TURN reasoning span across chunks.
///
/// THINKING MODELS SPLIT THEIR OUTPUT — read every text field, not just one.
/// ollama streams a reasoning model's tokens as `message.thinking` and only its
/// conclusion as `message.content`. Measured on gemma4:12b: 336 chars of
/// thinking to 38 of content. Reading `content` alone silently drops whole
/// turns — observed live on 2026-08-02, where gemma spent all 578 generated
/// tokens reasoning, we saw an EMPTY completion, and the chain ended at pass one
/// with no tool call and no error. ollama's own log showed the request answered
/// normally, so it looked like the server had wedged.
///
/// Same defect and same fix as NIM's `reasoning_content`. A raw text protocol
/// reads the text the model produced; it does not let the provider decide which
/// of its fields counts as output. (Non-string / explicit `null` normalizes to
/// empty — protocol shape, not bug masking.) The reasoning is WRAPPED once, so
/// ai-core can parse it but not persist it (`fantastic_ai_core::think`): opened
/// on the first thinking chunk, closed when content starts or the stream ends —
/// not per chunk, or the text would be a confetti of tags.
fn decode_chunk_into(
    parsed: &Value,
    wrap: &mut ThinkWrap,
    out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
) {
    let Some(msg) = parsed.get("message") else {
        return;
    };
    let think = msg.get("thinking").and_then(Value::as_str).unwrap_or("");
    if !think.is_empty() {
        if let Some(m) = wrap.open_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        out.push_back(Ok(ProviderEvent::Token(think.to_string())));
    }
    // RAW: text only. ai-core parses any `<call name=…>` envelope out of it.
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
    if !content.is_empty() {
        if let Some(m) = wrap.close_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        out.push_back(Ok(ProviderEvent::Token(content.to_string())));
    }
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
            "reflect" => reflect_reply(agent_id, kernel)?,
            // BOOT WITH A STORE RUNS `context_recall` — python `_make_boot`: an
            // agent hydrated with a history rebuilds its view before its first
            // turn, so a rebooted process sees the same head the last one saw.
            "boot" => {
                // A BOOT IS A NEW INCARNATION (python `_make_boot`): a reply
                // answering a call the PREVIOUS one made is recorded as a row
                // and wakes no turn.
                state::bump_incarnation(agent_id);
                if fantastic_ai_core::history::load_history(
                    agent_id,
                    kernel,
                    helpers::DEFAULT_CLIENT_ID,
                    false,
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
            // A LIFECYCLE NO-OP, as in python's `ollama_backend` (`return None`):
            // process state drops in `on_delete`, never behind a verb.
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
            // A new thread on the same store: the view starts here, nothing is deleted.
            "context_clear" => retrieval::context_clear(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            // python's VERBS map has no `shutdown` — it is an unknown type like
            // any other, and the value is rendered with python's `repr`.
            _ => json!({
                "error": format!(
                    "ollama_backend: unknown type {}",
                    py_repr_value(&payload.get("type").cloned().unwrap_or(Value::Null))
                )
            }),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        state::drop_state(agent_id);
        Ok(())
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
}

/// The provider this agent is configured for. Built per call (it is cheap and the
/// record can change between turns), and shared by `send` and the verbs that need
/// a completion of their own — `context_recall` asks the model which rows to keep
/// in full.
fn build_provider(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
    let meta = helpers::agent_meta(agent_id, kernel);
    Arc::new(OllamaProvider {
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
        temperature: meta
            .get("temperature")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TEMPERATURE),
        num_ctx: meta.get("num_ctx").and_then(Value::as_u64),
        num_predict: meta.get("num_predict").and_then(Value::as_u64),
        think: meta.get("think").and_then(Value::as_bool),
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

/// `reflect` — python `_make_reflect` + `ollama_backend/tools.py`'s
/// `_reflect_extra`: the shared base body, then the sampling knobs. The rust-only
/// `file_bridge_id` key is gone; a model that read it learned a binding python's
/// ollama agent does not report.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Result<Value, BundleError> {
    let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE, DEFAULT_MODEL, DEFAULT_ENDPOINT);
    let meta = helpers::agent_meta(agent_id, kernel);
    out.insert(
        "temperature".to_string(),
        match meta.get("temperature").filter(|v| !v.is_null()) {
            Some(v) => json!(py_float(v)?),
            None => json!(DEFAULT_TEMPERATURE),
        },
    );
    out.insert(
        "num_ctx".to_string(),
        match meta.get("num_ctx").filter(|v| py_truthy(v)) {
            Some(v) => json!(py_int(v)?),
            None => Value::Null,
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
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests;
