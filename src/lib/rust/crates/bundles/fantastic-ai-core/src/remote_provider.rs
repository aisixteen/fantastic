//! THE SPLIT: an AI CHAT and its PROVIDER are two agents.
//!
//! - A PROVIDER (any `*_backend` bundle) answers `complete`: one stateless pass
//!   over the messages it is handed — no memory, no calls, no conversation. Its
//!   tokens go out as `provider_token` events on its own stream, tagged with the
//!   caller's `rid`; the reply carries the report (model, prompt / completion
//!   tokens, stop reason). See [`complete`].
//! - An AI CHAT (`ai_chat.tools`) holds the conversation: the turn loop, the
//!   `<call>` parse and route, the memory, the folds. It knows its provider only
//!   by id (`provider_id`, a local id or a peer's `<guid>|<id>`), through
//!   [`RemoteProvider`]: it watches the provider, sends `complete`, and reads the
//!   tokens tagged with its `rid`. Nothing in the chat names a provider's kind,
//!   so a provider on another kernel serves it the same way.
//!
//! The chat's grants name the provider: `can_call` `{<provider>: [complete,
//! watch]}`, and the provider's `callable_by` the chat.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::{AgentId, Item, Kernel, Status};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, Unconfigured};

/// The event type a provider's tokens carry on its stream.
pub const TOKEN_EVENT: &str = "provider_token";

/// The record field naming a chat's provider.
pub const PROVIDER_FIELD: &str = "provider_id";

/// The `complete` verb's doc.
pub const COMPLETE_DOC: &str = "args: messages:list (req — role + text content, as ai_core assembles them), \
model:str? (the model of this pass — the calling chat's own; absent: this provider's default_model), \
max_tokens:int? (this call's output bound; absent: the record's), rid:str (req — tags every token event). \
ONE stateless pass: no memory, no calls, no conversation kept. Each content token is emitted on this agent's \
stream as {type:'provider_token', rid, text} (a watcher reads them); the reply is {rid, model, prompt_tokens, \
completion_tokens, stop_reason} or {rid, error, transport}. An AI chat (`ai_chat.tools`) calls it through its \
`provider_id`.";

/// The `models` verb's doc.
pub const MODELS_DOC: &str = "No args. The models this provider serves and its default: {models:[name, …], default}. \
The list is the record's `models` (names); a provider that can ask its API lists those when the record names none.";

/// The `default_model` verb's doc.
pub const DEFAULT_MODEL_DOC: &str = "args: model:str? — absent: GET the default ({default}); given: SET it (one of `models` when \
the list is known, else refused) and answer {default}. A chat that names no `model` completes on the default.";

/// For a provider that cannot ask its API for its models: `provider_verbs(…, NO_LIVE)`.
pub const NO_LIVE: Option<fn() -> std::future::Ready<Result<Vec<String>, String>>> = None;

/// The record field holding a provider's default model.
pub const DEFAULT_FIELD: &str = "default_model";

/// The names a provider's record lists in `models` (strings, or `{id|name}` objects).
#[must_use]
pub fn listed_models(meta: &Map<String, Value>) -> Vec<String> {
    meta.get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    m.as_str()
                        .map(String::from)
                        .or_else(|| m.get("id").or_else(|| m.get("name")).and_then(Value::as_str).map(String::from))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A provider's default: `default_model`, else the record's `model`.
#[must_use]
pub fn default_of(meta: &Map<String, Value>) -> Option<String> {
    meta.get(DEFAULT_FIELD)
        .or_else(|| meta.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// THE PROVIDER VERBS every backend serves the same way: `complete`, `models` (from the
/// record; `live` lists the API's when the record names none) and `default_model`.
/// `build` makes the backend's provider from a record (the pass's `model` laid over
/// the agent's). `None`: not a provider verb.
pub async fn provider_verbs<B, L, Fut>(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    build: B,
    live: Option<L>,
) -> Option<Value>
where
    B: Fn(Map<String, Value>) -> Arc<dyn Provider>,
    L: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
{
    let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    match verb {
        "complete" => {
            let model = payload
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| default_of(&meta));
            let Some(model) = model else {
                let rid = payload.get("rid").cloned().unwrap_or(Value::Null);
                return Some(json!({"rid": rid, "error": "complete: no model — the call names none and this provider has no default_model",
                                   "kind": "ValueError", "transport": false}));
            };
            let mut over = meta.clone();
            over.insert("model".into(), json!(model));
            Some(complete(agent_id, payload, kernel, build(over)).await)
        }
        "models" => {
            let mut names = listed_models(&meta);
            if names.is_empty() {
                if let Some(l) = live {
                    match l().await {
                        Ok(n) => names = n,
                        Err(e) => return Some(json!({"error": e, "reason": "unhandled", "default": default_of(&meta)})),
                    }
                }
            }
            Some(json!({"models": names, "default": default_of(&meta)}))
        }
        "default_model" => match payload.get("model") {
            None | Some(Value::Null) => Some(json!({"default": default_of(&meta)})),
            Some(Value::String(m)) if !m.is_empty() => {
                let names = listed_models(&meta);
                if !names.is_empty() && !names.contains(m) {
                    return Some(json!({"error": format!("default_model: `{m}` is not one of this provider's models {names:?}"),
                                       "reason": "bad_arg"}));
                }
                let mut patch = Map::new();
                patch.insert(DEFAULT_FIELD.into(), json!(m));
                kernel.update(agent_id, patch);
                Some(json!({"default": m}))
            }
            Some(_) => Some(json!({"error": "default_model: model must be a model name", "reason": "bad_arg"})),
        },
        _ => None,
    }
}

/// THE PROVIDER SIDE: run one pass of `provider` for `payload` and stream its
/// tokens on `agent_id`'s own stream, tagged with the caller's `rid`.
pub async fn complete(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>, provider: Arc<dyn Provider>) -> Value {
    let rid = payload.get("rid").and_then(Value::as_str).unwrap_or("").to_string();
    if rid.is_empty() {
        return json!({"error": "complete: rid required", "reason": "bad_arg"});
    }
    let Some(messages) = payload.get("messages").and_then(Value::as_array).cloned() else {
        return json!({"rid": rid, "error": "complete: messages must be a list", "reason": "bad_arg"});
    };
    let max_tokens = payload.get("max_tokens").and_then(Value::as_i64);
    if let Err(e) = provider.ready() {
        return failed(&rid, &e);
    }
    provider.clear_prompt_tokens();
    provider.clear_stop_reason();
    let mut stream = match provider.chat(&messages, max_tokens).await {
        Ok(s) => s,
        Err(e) => return failed(&rid, &e),
    };
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(text)) => {
                kernel
                    .emit_quiet(agent_id, json!({"type": TOKEN_EVENT, "rid": rid, "text": text}))
                    .await;
            }
            Err(e) => return failed(&rid, &e),
        }
    }
    let r = provider.report();
    json!({"rid": rid, "model": provider.model(), "prompt_tokens": r.prompt_tokens,
           "completion_tokens": r.completion_tokens, "stop_reason": r.stop_reason})
}

fn failed(rid: &str, e: &ProviderError) -> Value {
    json!({"rid": rid, "error": e.message, "kind": e.kind, "transport": e.transport})
}

/// THE CHAT SIDE: the provider an AI chat's record names (`provider_id`), or a
/// provider that refuses every pass naming the missing field.
#[must_use]
pub fn chat_provider(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    match meta.get(PROVIDER_FIELD).and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(p) => Arc::new(RemoteProvider::new(kernel, agent_id, p)),
        None => Arc::new(Unconfigured(ProviderError::value_error(format!(
            "ai_chat: the record names no {PROVIDER_FIELD} (the provider agent this chat completes through)"
        )))),
    }
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A provider reached through the kernel: `complete` on the agent `provider`.
pub struct RemoteProvider {
    kernel: Arc<Kernel>,
    chat: AgentId,
    provider: AgentId,
    report: Arc<Mutex<ProviderReport>>,
    model: Arc<Mutex<String>>,
    /// the chat's own `model` (absent: the provider's default)
    chat_model: Option<String>,
}

impl RemoteProvider {
    /// The provider `provider` (a local id or `<guid>|<id>`) of the chat `chat`.
    #[must_use]
    pub fn new(kernel: &Arc<Kernel>, chat: &AgentId, provider: &str) -> Self {
        let chat_model = crate::helpers::agent_meta(chat, kernel)
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let model = chat_model.clone().unwrap_or_default();
        Self {
            chat_model,
            kernel: Arc::clone(kernel),
            chat: chat.clone(),
            provider: AgentId::from(provider),
            report: Arc::default(),
            model: Arc::new(Mutex::new(model)),
        }
    }
}

/// The reply of a `complete`, read into the report; `Err` for a failed pass.
fn settle(result: &Value, report: &Mutex<ProviderReport>, model: &Mutex<String>) -> Result<(), ProviderError> {
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        let transport = result.get("transport").and_then(Value::as_bool).unwrap_or(false);
        return Err(if transport {
            ProviderError::transport(err)
        } else if result.get("kind").and_then(Value::as_str) == Some("ValueError") {
            ProviderError::value_error(err)
        } else {
            ProviderError::raised(err)
        });
    }
    let mut r = report.lock().expect("report poisoned");
    r.prompt_tokens = result.get("prompt_tokens").and_then(Value::as_i64);
    r.completion_tokens = result.get("completion_tokens").and_then(Value::as_i64);
    r.stop_reason = result.get("stop_reason").and_then(Value::as_str).map(String::from);
    if let Some(m) = result.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        *model.lock().expect("model poisoned") = m.to_string();
    }
    Ok(())
}

fn token_of(item: &Value, rid: &str) -> Option<String> {
    (item.get("type").and_then(Value::as_str) == Some(TOKEN_EVENT)
        && item.get("rid").and_then(Value::as_str) == Some(rid))
    .then(|| item.get("text").and_then(Value::as_str).unwrap_or("").to_string())
}

#[async_trait]
impl Provider for RemoteProvider {
    async fn chat(&self, messages: &[Value], max_tokens: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let rid = format!("{}~{n}", self.chat.as_str());
        // the reader: an inbox of this pass only, watching the provider's stream
        let reader = AgentId::from(format!("_provider_reader_{}_{n}", self.chat.as_str()));
        let mut inbox = self.kernel.ensure_inbox(&reader);
        if let Some(refused) = self.kernel.watch(&self.provider, reader.clone()).await {
            self.kernel.remove_inbox(&reader);
            return Err(ProviderError::raised(format!(
                "ai_chat: cannot watch provider `{}`: {}",
                self.provider.as_str(),
                refused.get("error").and_then(Value::as_str).unwrap_or(&refused.to_string())
            )));
        }
        let mut payload = Map::new();
        payload.insert("type".into(), json!("complete"));
        payload.insert("messages".into(), Value::Array(messages.to_vec()));
        payload.insert("rid".into(), json!(rid));
        if let Some(m) = &self.chat_model {
            payload.insert("model".into(), json!(m));
        }
        if let Some(m) = max_tokens {
            payload.insert("max_tokens".into(), json!(m));
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<ProviderEvent, ProviderError>>();
        let (kernel, chat, provider) = (Arc::clone(&self.kernel), self.chat.clone(), self.provider.clone());
        let (report, model) = (Arc::clone(&self.report), Arc::clone(&self.model));
        // the call runs as the chat (its grants are judged), beside the reader
        tokio::spawn(async move { let me = chat.clone(); fantastic_kernel::send::as_agent(&me, async move {
            let call = kernel.send(vec![Item::new(provider.clone(), Value::Object(payload))]);
            tokio::pin!(call);
            let outcome = loop {
                tokio::select! {
                    out = &mut call => break out,
                    item = inbox.recv() => match item {
                        Some(it) => if let Some(t) = token_of(&it.payload, &rid) {
                            let _ = tx.send(Ok(ProviderEvent::Token(t)));
                        },
                        None => {}
                    },
                }
            };
            // tokens emitted before the reply are already queued: drain them
            while let Some(it) = inbox.try_recv() {
                if let Some(t) = token_of(&it.payload, &rid) {
                    let _ = tx.send(Ok(ProviderEvent::Token(t)));
                }
            }
            kernel.unwatch(&provider, &reader).await;
            kernel.remove_inbox(&reader);
            let settled = match outcome.into_iter().next() {
                Some(o) if o.status == Status::Completed => settle(&o.result, &report, &model),
                Some(o) => Err(ProviderError::raised(format!(
                    "ai_chat: complete on `{}` ended {:?}: {}",
                    provider.as_str(),
                    o.status,
                    o.result
                ))),
                None => Err(ProviderError::raised("ai_chat: complete returned nothing")),
            };
            if let Err(e) = settled {
                let _ = tx.send(Err(e));
            }
        }).await });
        Ok(Box::pin(tokio_stream_of(rx)))
    }

    fn model(&self) -> String {
        self.model.lock().expect("model poisoned").clone()
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
}

/// An unbounded receiver as a stream (no tokio-stream dependency).
fn tokio_stream_of<T: Send + 'static>(
    rx: tokio::sync::mpsc::UnboundedReceiver<T>,
) -> impl futures_util::Stream<Item = T> + Send + 'static {
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
}

#[cfg(test)]
#[path = "tests_remote_provider.rs"]
mod tests_remote_provider;
