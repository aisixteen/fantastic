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
//! prompt-and-parse owned by ai-core (the model emits a `<tool_call>` envelope
//! in the content text).

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::provider::{Provider, ProviderEvent, ProviderStream};
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, events::CallerRoute, helpers, state, verbs};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "ollama_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Hard ceiling per `send` (mirrors Python's `SEND_TIMEOUT`).
pub const SEND_TIMEOUT_SECS: u64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

/// Default `client_id` for callers that don't supply one.
pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;

/// Default ollama HTTP endpoint.
pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// NO guessy default (by default, no defaults): never name a network-reaching
/// model. The sentinel exists only so reflect can say what's true; the request
/// path fails loud when the model is unset/sentinel.
pub const DEFAULT_MODEL: &str = "(no default — set the `model` meta)";

/// Default sampling temperature sent to ollama (overridable via the record's
/// `temperature` field). ollama's own default is 0.8 (creative) — wrong for an
/// agentic backend whose output is tool-calls + code. Mirrors the python
/// canonical default (0.3 — precise, with a touch of variety); a record
/// `temperature` overrides it.
pub const DEFAULT_TEMPERATURE: f64 = 0.3;

/// Per-backend config: cli round-trip routing, parallel tool dispatch.
const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::CliRoundTrip,
    name: "ollama_backend",
};

// ── ollama provider (NDJSON transport) ──────────────────────────────

/// Provider for ollama's `/api/chat`. PURE RAW TEXT — decodes the NDJSON stream
/// into `ProviderEvent::Token` content only. NO native tools array is sent and
/// NO `tool_calls` are read; ai-core parses the `<tool_call>` envelope from text.
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

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String> {
        // NO guessy default: fail loud before the wire if no model is named.
        if self.model.is_empty() || self.model == DEFAULT_MODEL {
            return Err("ollama_backend: no `model` set — this connector does not \
                guess one. Name a model on the agent record (model=...)."
                .to_string());
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
            .map_err(|e| format!("ollama: request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("ollama: HTTP {}", resp.status()));
        }

        // Decode the whole NDJSON body into a Vec of events, then return
        // them as a stream. Mirrors the prior Vec-based consumption.
        let mut byte_stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut out: Vec<Result<ProviderEvent, String>> = Vec::new();
        // ONE wrapper for the whole pass — the reasoning span is per TURN.
        let mut wrap = ThinkWrap::new();
        while let Some(chunk) = byte_stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    // A span left open here stays unterminated on purpose: the
                    // stream was cut mid-thought, and `without_thinking` drops
                    // an unterminated tail rather than persisting a fragment.
                    out.push(Err(format!("ollama: stream error: {e}")));
                    return Ok(Box::pin(futures_util::stream::iter(out)));
                }
            };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let trimmed = &line[..line.len().saturating_sub(1)];
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(parsed) = serde_json::from_slice::<Value>(trimmed) else {
                    continue;
                };
                decode_chunk_into(&parsed, &mut wrap, &mut out);
            }
        }
        if !buf.is_empty() {
            if let Ok(parsed) = serde_json::from_slice::<Value>(&buf) {
                decode_chunk_into(&parsed, &mut wrap, &mut out);
            }
        }
        // The turn ended while still reasoning (content never started) — close
        // the span exactly once so the deliberation is strippable.
        if let Some(m) = wrap.close_marker() {
            out.push(Ok(ProviderEvent::Token(m.to_string())));
        }
        Ok(Box::pin(futures_util::stream::iter(out)))
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
    out: &mut Vec<Result<ProviderEvent, String>>,
) {
    let Some(msg) = parsed.get("message") else {
        return;
    };
    let think = msg.get("thinking").and_then(Value::as_str).unwrap_or("");
    if !think.is_empty() {
        if let Some(m) = wrap.open_marker() {
            out.push(Ok(ProviderEvent::Token(m.to_string())));
        }
        out.push(Ok(ProviderEvent::Token(think.to_string())));
    }
    // RAW: text only. ai-core parses any `<tool_call>` envelope out of it.
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
    if !content.is_empty() {
        if let Some(m) = wrap.close_marker() {
            out.push(Ok(ProviderEvent::Token(m.to_string())));
        }
        out.push(Ok(ProviderEvent::Token(content.to_string())));
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
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" => Value::Null,
            "shutdown" => {
                state::drop_state(agent_id);
                Value::Null
            }
            "send" => send_reply(agent_id, payload, kernel).await,
            // ONE ASK VERB, WHOEVER IS ASKED: a person answers `inquiry` from a
            // record, this agent answers it by THINKING, and the asker writes
            // the same call either way.
            "inquiry" => match verbs::inquiry_open(payload) {
                Err(refusal) => refusal,
                Ok(sp) => verbs::inquiry_close(send_reply(agent_id, &sp, kernel).await, payload),
            },
            "list_models" => list_models_reply(agent_id, payload, kernel).await,
            "history" => verbs::history(agent_id, payload, kernel).await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            other => json!({"error": format!("ollama: unknown type {other:?}")}),
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
        verbs::on_cancel(agent_id, task_id.as_str(), kernel, CFG.route).await;
        Ok(())
    }
}

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let meta = helpers::agent_meta(agent_id, kernel);
    let temperature = meta
        .get("temperature")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TEMPERATURE);
    let num_ctx = meta.get("num_ctx").and_then(Value::as_u64);
    let num_predict = meta.get("num_predict").and_then(Value::as_u64);
    let think = meta.get("think").and_then(Value::as_bool);
    let provider: Arc<dyn Provider> = Arc::new(OllamaProvider {
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
        temperature,
        num_ctx,
        num_predict,
        think,
    });
    verbs::send(provider, agent_id, payload, kernel, CFG).await
}

/// `list_models` — model autocomplete + reachability probe. Resolves the endpoint
/// from `payload["endpoint"]` FIRST (so a caller can probe an ARBITRARY endpoint
/// with no configured record), else the record's `endpoint` meta, else
/// [`DEFAULT_ENDPOINT`]. Does `GET {endpoint}/api/tags`, parses ollama's
/// `{"models":[{"name":…}, …]}` shape, and returns `{"models":[name, …]}` in
/// order. ANY failure (connection refused, DNS, timeout, non-2xx, bad JSON)
/// returns `{"error": "<human-readable message including the endpoint>"}` — this
/// doubles as the TUI's reachability probe, so the message must name the URL.
async fn list_models_reply(agent_id: &AgentId, payload: &Value, kernel: &Kernel) -> Value {
    // Endpoint precedence: explicit payload override → record meta → DEFAULT.
    let endpoint = payload
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT));
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    // Reuse reqwest (the same client type used for /api/chat) — no new http dep.
    let client = reqwest::Client::new();
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return json!({"error": format!("ollama: can't reach {url} ({e})")}),
    };
    if !resp.status().is_success() {
        return json!({"error": format!("ollama: {url} returned HTTP {}", resp.status())});
    }
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return json!({"error": format!("ollama: bad response from {url} ({e})")}),
    };
    json!({ "models": parse_tag_names(&body) })
}

/// Extract model names from ollama's `/api/tags` body
/// (`{"models":[{"name":"gemma3:12b", …}, …]}`) → `["gemma3:12b", …]`, in order.
/// Missing / oddly-shaped entries are skipped (never a panic).
fn parse_tag_names(body: &Value) -> Vec<String> {
    body.get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let model = helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL);
    let endpoint = helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT);
    let file_bridge_id = helpers::meta_string(agent_id, kernel, "file_bridge_id");
    let generating = state::is_generating(agent_id);
    let meta = helpers::agent_meta(agent_id, kernel);
    let context_window = fantastic_ai_core::context::resolve_context_window(&meta);
    let context_strategy = meta
        .get("context_strategy")
        .and_then(Value::as_str)
        .unwrap_or("compact")
        .to_string();
    let temperature = meta
        .get("temperature")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TEMPERATURE);
    let num_ctx = meta.get("num_ctx").and_then(Value::as_u64);
    let num_predict = meta.get("num_predict").and_then(Value::as_u64);
    let think = meta.get("think").and_then(Value::as_bool);
    json!({
        "id": agent_id.as_str(),
        "sentence": "Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "temperature": temperature,
        "num_ctx": num_ctx,
        "num_predict": num_predict,
        "think": think,
        "file_bridge_id": file_bridge_id,
        "generating": generating,
        "context_window": context_window,
        "context_strategy": context_strategy,
        "verbs": {
            "reflect": "Identity + model + endpoint + generating flag + file_bridge_id binding. No args.",
            "boot": "No-op. Returns null.",
            "shutdown": "Drops process-memory state. Returns null.",
            "send": "args: text:str (req), client_id:str? (default 'cli'), system_prompt:str? (REPLACES the default system block), send_to:str? (hands this pass's TEXT to that agent id once, no propagation). Streams tokens to ONLY the caller. Per-backend FIFO lock. ONE provider pass; the model routes its own work with `reply_to` inside its <tool_call>s (default: back to itself). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Returns {response, final, client_id}.",
            "list_models": "args: endpoint:str? (default the record's endpoint, else http://localhost:11434). GET {endpoint}/api/tags. Returns {models:[name,...]} in order on success, or {error:'<msg naming the endpoint>'} on any failure — doubles as the reachability probe.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id} — that client's persisted chat.",
            "recall": "args: client_id:str?, query:str?, limit:int?, before:int?. Pages turns back from the durable store (lossless on demand after compaction). Returns {messages, total, truncated, client_id}.",
            "context_status": "No args. Context-budget posture + last compaction + derived reaction. Returns {context_window, output_reserve, budget, strategy, last_projection, last_reaction}.",
            "status": "args: client_id:str?. Returns the in-flight/queue snapshot (text redacted for other clients).",
        },
        "cancel": fantastic_ai_core::CANCEL_DOC,
        "emits": {
            "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...}} — phase transitions.",
            "token": "{type:'token', text, source, client_id} — one per streaming chunk.",
            "say": "{type:'say', text:'[tool target → reply]', source, client_id} — one per tool-call summary.",
            "done": "{type:'done', source, client_id} — final event after streaming completes (or a cancel).",
            "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the Context Protocol push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized}. too_small: detail={context_window, system_tokens, hint} (model NOT called — a failfast). Pull counterpart: the context_status verb.",
        },
        "concurrency": "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait + receive a queued status event. reflect/history/status skip the lock.",
    })
}

#[cfg(test)]
mod tests;
