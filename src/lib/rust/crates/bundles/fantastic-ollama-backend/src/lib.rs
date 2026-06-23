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
//! line-delimited JSON; each line carries `{message:{content?}}`. NO `tools`
//! array is sent and NO `tool_calls` are read — tool-calling is RAW
//! prompt-and-parse owned by ai-core (the model emits a `<tool_call>` envelope
//! in the content text).

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::provider::{Provider, ProviderEvent, ProviderStream};
use fantastic_ai_core::{agent_loop::BackendConfig, events::CallerRoute, helpers, state, verbs};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
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

/// Default model id (overridable via the agent record's `model` field).
pub const DEFAULT_MODEL: &str = "gemma4:e2b";

/// Per-backend config: cli round-trip routing, parallel tool dispatch.
const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::CliRoundTrip,
    parallel_tools: true,
    name: "ollama_backend",
};

// ── ollama provider (NDJSON transport) ──────────────────────────────

/// Provider for ollama's `/api/chat`. PURE RAW TEXT — decodes the NDJSON stream
/// into `ProviderEvent::Token` content only. NO native tools array is sent and
/// NO `tool_calls` are read; ai-core parses the `<tool_call>` envelope from text.
struct OllamaProvider {
    endpoint: String,
    model: String,
}

#[async_trait]
impl Provider for OllamaProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
        });
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
        while let Some(chunk) = byte_stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
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
                decode_chunk_into(&parsed, &mut out);
            }
        }
        if !buf.is_empty() {
            if let Ok(parsed) = serde_json::from_slice::<Value>(&buf) {
                decode_chunk_into(&parsed, &mut out);
            }
        }
        Ok(Box::pin(futures_util::stream::iter(out)))
    }
}

fn decode_chunk_into(parsed: &Value, out: &mut Vec<Result<ProviderEvent, String>>) {
    let Some(msg) = parsed.get("message") else {
        return;
    };
    // RAW: content text ONLY. ai-core parses any `<tool_call>` envelope out of it.
    if let Some(content) = msg.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            out.push(Ok(ProviderEvent::Token(content.to_string())));
        }
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
            "history" => verbs::history(agent_id, payload, kernel, "ollama_backend").await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "interrupt" => verbs::interrupt(agent_id),
            "refresh_menu" => verbs::refresh_menu(agent_id),
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
}

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if helpers::file_bridge_id(agent_id, kernel).is_none() {
        return json!({"error": "ollama_backend: file_bridge_id required"});
    }
    let provider: Arc<dyn Provider> = Arc::new(OllamaProvider {
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        model: helpers::meta_string_or(agent_id, kernel, "model", DEFAULT_MODEL),
    });
    verbs::send(provider, agent_id, payload, kernel, CFG).await
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
    json!({
        "id": agent_id.as_str(),
        "sentence": "Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
        "model": model,
        "endpoint": endpoint,
        "file_bridge_id": file_bridge_id,
        "generating": generating,
        "context_window": context_window,
        "context_strategy": context_strategy,
        "verbs": {
            "reflect": "Identity + model + endpoint + generating flag + file_bridge_id binding. No args.",
            "boot": "No-op. Returns null.",
            "shutdown": "Aborts any in-flight send and drops process-memory state. Returns {stopped:bool}.",
            "send": "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller. Per-backend FIFO lock. Returns {response, final, client_id}.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id} — that client's persisted chat.",
            "recall": "args: client_id:str?, query:str?, limit:int?, before:int?. Pages turns back from the durable store (lossless on demand after compaction). Returns {messages, total, truncated, client_id}.",
            "context_status": "No args. Context-budget posture + last compaction + derived reaction. Returns {context_window, output_reserve, budget, strategy, last_projection, last_reaction}.",
            "interrupt": "No args. Cancels any in-flight send. Returns {interrupted:bool}.",
            "refresh_menu": "No args. Drops the cached agent menu. Returns {refreshed:true}.",
            "status": "args: client_id:str?. Returns the in-flight/queue snapshot (text redacted for other clients).",
        },
        "emits": {
            "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...}} — phase transitions.",
            "token": "{type:'token', text, source, client_id} — one per streaming chunk.",
            "say": "{type:'say', text:'[tool target → reply]', source, client_id} — one per tool-call summary.",
            "done": "{type:'done', source, client_id} — final event after streaming completes (or interrupt).",
            "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the Context Protocol push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized}. too_small: detail={context_window, system_tokens, hint} (model NOT called — a failfast). Pull counterpart: the context_status verb.",
        },
        "concurrency": "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait + receive a queued status event. reflect/history/interrupt/status skip the lock.",
    })
}

#[cfg(test)]
mod tests;
