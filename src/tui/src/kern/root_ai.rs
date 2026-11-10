//! THE ROOT AI: `fantastic [canvas] ai BACKEND …` (`recipe.rs`). ONE chat agent, `ai`
//! under `user`, on the project's memory store: the terminal UI opens it; on a
//! canvas, a chat cell shows it wherever one is placed (no main chat).
//!
//! - `ai` is a chat on a PROVIDER (`ai_provider`, the backend). On the SAME backend the provider
//!   is RE-POINTED (its endpoint, its own fields) and
//!   what the server decides is CLEARED, so the backend reads it from the server
//!   before its first turn (and writes it back);
//! - on a DIFFERENT backend it is remade: deleted and created again under the
//!   same id on the same store (the dialog's rows are keyed by the id, so the
//!   conversation carries over; its own grants are written again);
//! - none yet: made.
//!
//! Nothing here is guessed: the port and model are the caller's; the host is `127.0.0.1`, the Responses endpoint OpenAI's and the
//! Anthropic one its Messages API, unless named.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The root AI's id.
pub const ROOT_AI: &str = "ai";
/// `ai responses` without `--endpoint`.
pub const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
/// `ai anthropic` without `--endpoint` (the Messages API).
pub const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const VIEW: &str = "view_root";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Vllm { host: String, port: u16, kv_overflow: String },
    Ollama { host: String, port: u16 },
    Responses { endpoint: String, model: String },
    Anthropic { endpoint: String, model: String },
}

impl Backend {
    pub fn handler(&self) -> &'static str {
        match self {
            Backend::Vllm { .. } => "vllm_backend.tools",
            Backend::Ollama { .. } => "ollama_backend.tools",
            Backend::Responses { .. } => "responses_backend.tools",
            Backend::Anthropic { .. } => "anthropic_backend.tools",
        }
    }

    /// The cargo feature that links it.
    fn feature(&self) -> &'static str {
        match self {
            Backend::Vllm { .. } => "vllm",
            Backend::Ollama { .. } => "ollama",
            Backend::Responses { .. } => "responses",
            Backend::Anthropic { .. } => "anthropic",
        }
    }

    pub fn endpoint(&self) -> String {
        match self {
            Backend::Vllm { host, port, .. } => format!("http://{host}:{port}/v1"),
            Backend::Ollama { host, port } => format!("http://{host}:{port}"),
            Backend::Responses { endpoint, .. } | Backend::Anthropic { endpoint, .. } => endpoint.clone(),
        }
    }

    /// The record fields the caller names.
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("endpoint".into(), json!(self.endpoint()));
        match self {
            Backend::Vllm { kv_overflow, .. } => {
                m.insert("kv_overflow".into(), json!(kv_overflow));
            }
            Backend::Ollama { .. } => {}
            Backend::Responses { model, .. } | Backend::Anthropic { model, .. } => {
                m.insert("model".into(), json!(model));
            }
        }
        m
    }

    /// What the server decides — cleared on a re-point, so the new server is read.
    fn from_server(&self) -> &'static [&'static str] {
        match self {
            Backend::Vllm { .. } => &["model", "context_window", "max_tokens", "kv_slots", "metrics_url"],
            // only the model: ollama's loaded window is not in its API
            Backend::Ollama { .. } => &["model"],
            Backend::Responses { .. } => &[],
            Backend::Anthropic { .. } => &["context_window", "max_tokens"],
        }
    }
}

/// The chat cell's own grant onto the AI it shows — `ai_view.ts`'s `grants`.
fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

fn handler_of(h: &Host, id: &str) -> Option<String> {
    h.kernel.agents.get(&fantastic_kernel::AgentId::from(id))?.value().record().handler_module
}

/// The agent `id`'s record, as a `create_agent` to put it back.
fn record_of(h: &Host, id: &str) -> Option<Map<String, Value>> {
    let rec = h.kernel.agents.get(&fantastic_kernel::AgentId::from(id)).and_then(|a| serde_json::to_value(a.value().record()).ok())?;
    let mut rec = rec.as_object().cloned()?;
    rec.insert("type".into(), json!("create_agent"));
    Some(rec)
}

/// Make, re-point or remake the root AI on `b`; says what it did. The root AI is a chat (`ai`)
/// and its provider (the chat's `provider_id`, a backend): a re-point changes the provider's
/// record, a remake deletes and makes both.
pub async fn run(h: &Host, b: &Backend) -> Result<String, String> {
    // a build without the backend would skip the record at the next boot
    if h.kernel.bundles.get(b.handler()).is_none() {
        return Err(format!("this build has no {} backend — build with `--features {}`", b.handler(), b.feature()));
    }
    let had_chat = handler_of(h, ROOT_AI);
    let provider = crate::kern::backend::provider_of(h, ROOT_AI);
    let had = provider.as_deref().and_then(|p| handler_of(h, p));
    let did = if let (Some(p), true) = (&provider, had.as_deref() == Some(b.handler())) {
        let mut up = b.fields();
        for f in b.from_server() {
            up.entry(f.to_string()).or_insert(Value::Null);
        }
        // the model and the window are on the chat too: cleared with the provider's, the new
        // server is read
        let chat_up = crate::kern::backend::chat_part(&up);
        up.insert("type".into(), json!("update_agent"));
        up.insert("id".into(), json!(p));
        let r = h.operator("kernel", Value::Object(up)).await.result;
        if let Some(e) = err_of(&r) {
            return Err(format!("re-pointing `{p}`: {e}"));
        }
        if !chat_up.is_empty() {
            let mut up = chat_up;
            up.insert("type".into(), json!("update_agent"));
            up.insert("id".into(), json!(ROOT_AI));
            let r = h.operator("kernel", Value::Object(up)).await.result;
            if let Some(e) = err_of(&r) {
                return Err(format!("re-pointing `{ROOT_AI}`: {e}"));
            }
        }
        "re-pointed"
    } else {
        // the OLD records (chat and provider), to put back if the new ones are refused (a missing
        // key, a field the new backend requires): a failed switch never loses the AI
        let old_provider = provider.as_deref().and_then(|p| record_of(h, p));
        let old_chat = record_of(h, ROOT_AI);
        if had_chat.is_some() {
            let r = h.operator("kernel", json!({"type": "delete_agent", "id": ROOT_AI})).await.result;
            if let Some(e) = err_of(&r) {
                return Err(format!("removing the old `{ROOT_AI}`: {e}"));
            }
        }
        if let (Some(p), Some(_)) = (&provider, &old_provider) {
            let r = h.operator("kernel", json!({"type": "delete_agent", "id": p})).await.result;
            if let Some(e) = err_of(&r) {
                return Err(format!("removing the old `{p}`: {e}"));
            }
        }
        let mut spec = b.fields();
        spec.insert("handler_module".into(), json!(b.handler()));
        let user = fantastic_kernel::AgentId::from(h.cfg.user.as_str());
        if let Err(e) = crate::kern::backend::BackendBundle::make_ai(h, &user, ROOT_AI, spec, Map::new()).await {
            // whatever the failed make left is removed, then the old pair is put back (provider first)
            for id in [ROOT_AI.to_string(), crate::kern::backend::provider_id(ROOT_AI)] {
                if h.kernel.exists(&id) {
                    let _ = h.operator("kernel", json!({"type": "delete_agent", "id": id})).await;
                }
            }
            if let Some(chat) = old_chat {
                let mut failed = None;
                for rec in old_provider.into_iter().chain([chat]) {
                    let back = h.operator("kernel", Value::Object(rec)).await.result;
                    failed = failed.or_else(|| err_of(&back));
                }
                return Err(match failed {
                    None => format!("{e} — `{ROOT_AI}` is left on {}", had.unwrap_or_else(|| had_chat.unwrap_or_default())),
                    Some(b2) => format!("{e} — and the old `{ROOT_AI}` could not be restored: {b2}"),
                });
            }
            return Err(e);
        }
        if had_chat.is_some() { "remade" } else { "made" }
    };
    Ok(format!("the root AI `{ROOT_AI}` ({did}, {}) speaks to {}", b.handler(), b.endpoint()))
}

async fn page_records(h: &Host) -> Result<Vec<Map<String, Value>>, String> {
    let r = h.operator(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "load_tree"})).await.result;
    if let Some(e) = err_of(&r) {
        return Err(format!("the page's records: {e}"));
    }
    Ok(r.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_object().cloned()).collect())
}

pub(crate) async fn persist(h: &Host, rec: Map<String, Value>) -> Result<(), String> {
    let id = rec.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let r = h.operator(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "persist_record", "record": rec})).await.result;
    err_of(&r).map_or(Ok(()), |e| Err(format!("writing the page record `{id}`: {e}")))
}

/// Whether the page has its view yet (it is made at the page's first load).
pub(crate) async fn has_view(h: &Host) -> Result<bool, String> {
    Ok(page_records(h).await?.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(VIEW)))
}
