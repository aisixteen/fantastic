//! `llama-server` (llama.cpp) LLM bundle.
//!
//! Mirrors python's `llamacpp_backend`. llama-server speaks the
//! OpenAI-compatible surface at `<endpoint>/chat/completions` and streams RAW
//! TEXT via SSE, so the per-delta decode is the SHARED
//! [`fantastic_ai_core::think::decode_openai_delta`] — the same one the NIM
//! backend uses.
//!
//! NO NATIVE TOOL-CALLING: the provider streams text and ai-core parses the
//! `<tool_call>` envelope out of it.
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
use fantastic_ai_core::provider::{Provider, ProviderEvent, ProviderStream};
use fantastic_ai_core::think::{decode_openai_delta, ThinkWrap};
use fantastic_ai_core::{agent_loop::BackendConfig, events::CallerRoute, helpers, state, verbs};
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "llamacpp_backend.tools";

pub const README: &str = include_str!("readme.md");

/// Where a local llama-server listens by default.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8080/v1";

pub const DEFAULT_CLIENT_ID: &str = fantastic_ai_core::DEFAULT_CLIENT_ID;
pub const SEND_TIMEOUT_SECS: u64 = fantastic_ai_core::SEND_TIMEOUT_SECS;

const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::PerClientInbox,
    name: "llamacpp_backend",
};

// ── provider ────────────────────────────────────────────────────────

struct LlamaCppProvider {
    endpoint: String,
    /// What the RECORD asked for, if anything. Usually `None` — see the module
    /// note on why no default is invented here.
    requested_model: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    /// `None` means DO NOT SEND the kwarg: a model without a thinking gate
    /// rejects an unknown one, so this is three-valued on purpose.
    think: Option<bool>,
}

#[async_trait]
impl Provider for LlamaCppProvider {
    fn model(&self) -> String {
        // Reporting a guess here would be a lie that `reflect` then repeats.
        self.requested_model
            .clone()
            .unwrap_or_else(|| "(llama-server default)".to_string())
    }

    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String> {
        // RAW: no native `tools`/`tool_choice` — ai-core parses the `<tool_call>`
        // envelope out of the streamed text.
        let mut body = serde_json::Map::new();
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));
        if let Some(m) = &self.requested_model {
            body.insert("model".into(), json!(m));
        }
        if let Some(t) = self.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(n) = self.max_tokens {
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
        let resp = req.send().await.map_err(|e| format!("http: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("send: provider HTTP {}", status.as_u16()));
        }
        let events = consume_sse(resp).await?;
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

/// Drain the SSE body, split into `data:` lines, emit content tokens in order.
/// PURE RAW TEXT — no native `tool_calls` are read.
async fn consume_sse(
    resp: reqwest::Response,
) -> Result<Vec<Result<ProviderEvent, String>>, String> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<Result<ProviderEvent, String>> = Vec::new();
    let mut done_marker_seen = false;
    // ONE wrapper for the whole pass — the reasoning span is per TURN.
    let mut wrap = ThinkWrap::new();

    while let Some(chunk_res) = stream.next().await {
        let bytes = match chunk_res {
            Ok(b) => b,
            Err(e) => return Err(format!("stream: {e}")),
        };
        buf.extend_from_slice(&bytes);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
            let line_owned = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            let line = line_owned.as_str();
            if line.is_empty() || !line.starts_with("data:") {
                continue;
            }
            let data = line[5..].trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                done_marker_seen = true;
                break;
            }
            let obj: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(choices) = obj.get("choices").and_then(Value::as_array) else {
                continue;
            };
            let Some(first) = choices.first() else {
                continue;
            };
            let delta = first.get("delta").cloned().unwrap_or(Value::Null);
            for piece in decode_openai_delta(&delta, &mut wrap) {
                out.push(Ok(ProviderEvent::Token(piece)));
            }
        }
        if done_marker_seen {
            break;
        }
    }

    // The turn ended while still reasoning (content never started) — close the
    // span exactly once so the deliberation is strippable.
    if let Some(m) = wrap.close_marker() {
        out.push(Ok(ProviderEvent::Token(m.to_string())));
    }
    Ok(out)
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

async fn send_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if helpers::file_bridge_id(agent_id, kernel).is_none() {
        return json!({"error": "llamacpp_backend: file_bridge_id required"});
    }
    let client_id = helpers::safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let _ = client_id;
    let provider: Arc<dyn Provider> = Arc::new(LlamaCppProvider {
        endpoint: helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT),
        requested_model: helpers::meta_string(agent_id, kernel, "model"),
        api_key: helpers::meta_string(agent_id, kernel, "api_key"),
        max_tokens: meta_u64(agent_id, kernel, "num_predict"),
        temperature: helpers::agent_meta(agent_id, kernel)
            .get("temperature")
            .and_then(Value::as_f64),
        think: meta_bool(agent_id, kernel, "think"),
    });
    verbs::send(provider, agent_id, payload, kernel, CFG).await
}

async fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let endpoint = helpers::meta_string_or(agent_id, kernel, "endpoint", DEFAULT_ENDPOINT);
    let meta = helpers::agent_meta(agent_id, kernel);
    let context_window = fantastic_ai_core::context::resolve_context_window(&meta);
    let context_strategy = meta
        .get("context_strategy")
        .and_then(Value::as_str)
        .unwrap_or("compact")
        .to_string();
    json!({
        "id": agent_id.as_str(),
        "sentence": "llama.cpp-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
        "endpoint": endpoint,
        "model": helpers::meta_string(agent_id, kernel, "model"),
        "file_bridge_id": helpers::meta_string(agent_id, kernel, "file_bridge_id"),
        "think": meta.get("think").cloned().unwrap_or(Value::Null),
        "generating": state::is_generating(agent_id),
        "context_window": context_window,
        "context_strategy": context_strategy,
        "verbs": {
            "reflect": "Identity + endpoint + think + generating + file_bridge_id binding. No args.",
            "boot": "No-op. Returns null.",
            "send": "args: text:str (req), client_id:str? (default 'cli'), system_prompt:str?, send_to:str?. Streams tokens to ONLY the caller. Failfast if file_bridge_id unset.",
            "history": "args: client_id:str? (default 'cli'). Returns {messages, client_id}.",
            "recall": "args: client_id:str?, query:str?, limit:int?, before:int?. Pages turns back from the durable store.",
            "context_status": "No args. Context-budget posture + last compaction + derived reaction.",
            "status": "args: client_id?. In-flight phase + this client's pending queue.",
        },
        "note": "The model and the context window belong to the server (-hf/-m, -c N), not to this record: llama-server serves ONE model and ignores the request's `model` field.",
    })
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
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel).await,
            "boot" | "shutdown" => Value::Null,
            "send" => send_reply(agent_id, payload, kernel).await,
            "history" => verbs::history(agent_id, payload, kernel, "llamacpp_backend").await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            "author" => fantastic_ai_core::author::author(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            other => json!({"error": format!("llamacpp_backend: unknown type {other:?}")}),
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
