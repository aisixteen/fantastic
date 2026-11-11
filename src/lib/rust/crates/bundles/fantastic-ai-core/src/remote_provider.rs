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

/// The name of a provider's default-model setting (in its memory store).
pub const DEFAULT_FIELD: &str = "default_model";

/// The name of a provider's usage setting (in its memory store).
const USAGE_FIELD: &str = "usage";

/// A PROVIDER'S SETTINGS live in its memory store (`memory_store_id`), as `setting` rows
/// under its own key — `{kind: "setting", name, value}`, the newest of a name wins. Its
/// record holds the connection only.
const SETTING: &str = "setting";

/// The store a provider keeps its settings in.
fn store_of(agent_id: &AgentId, kernel: &Kernel) -> Option<AgentId> {
    crate::helpers::agent_meta(agent_id, kernel)
        .get("memory_store_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(AgentId::from)
}

/// The newest `setting` row named `name` in the provider's store: its value.
pub async fn setting_of(agent_id: &AgentId, kernel: &Arc<Kernel>, name: &str) -> Option<Value> {
    let store = store_of(agent_id, kernel)?;
    let r = kernel
        .send(vec![Item::new(
            store,
            json!({"type": "read", "kinds": [SETTING], "match": {"name": name}, "order": "desc", "limit": 1}),
        )])
        .await;
    r.first()?.result.get("rows")?.as_array()?.first()?.get("value").cloned()
}

/// Keep `value` as the provider's `name` setting (a `setting` row in its store). `Err` names why.
async fn set_setting(agent_id: &AgentId, kernel: &Arc<Kernel>, name: &str, value: Value) -> Result<(), String> {
    let store = store_of(agent_id, kernel).ok_or("the provider names no memory_store_id to keep its settings in")?;
    let r = kernel
        .send(vec![Item::new(
            store,
            json!({"type": "append", "rows": [{"kind": SETTING, "name": name, "value": value}]}),
        )])
        .await;
    match r.first().map(|o| &o.result) {
        Some(res) if res.get("error").is_none() => Ok(()),
        Some(res) => Err(res["error"].as_str().unwrap_or("the store refused").to_string()),
        None => Err("the store did not answer".to_string()),
    }
}

/// A provider's default model: the newest `default_model` setting in its store.
pub async fn default_of(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Option<String> {
    setting_of(agent_id, kernel, DEFAULT_FIELD)
        .await?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Keep `model` as the provider's default (a `setting` row in its store). `Err` names why.
async fn set_default(agent_id: &AgentId, kernel: &Arc<Kernel>, model: &str) -> Result<(), String> {
    set_setting(agent_id, kernel, DEFAULT_FIELD, json!(model)).await
}

/// A provider's kept usage (total, by_model, by_agent): the newest `usage` setting in its
/// store. None: it has no store, or has kept none yet.
pub async fn usage_of(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Option<Value> {
    setting_of(agent_id, kernel, USAGE_FIELD).await.filter(Value::is_object)
}

/// Keep `usage` as the provider's usage (a `usage` setting row in its store). A provider
/// with no store keeps none: `Ok`. `Err` names why a store refused.
pub async fn set_usage(agent_id: &AgentId, kernel: &Arc<Kernel>, usage: &Value) -> Result<(), String> {
    if store_of(agent_id, kernel).is_none() {
        return Ok(());
    }
    set_setting(agent_id, kernel, USAGE_FIELD, usage.clone()).await
}

/// The `models` verb's doc.
pub const MODELS_DOC: &str = "No args. The models this provider serves and its default: {models:[name, …], default}. \
The list is the record's `models`; a provider that can ask its API lists those when the record names none.";

/// The `set_default_model` verb's doc.
pub const SET_DEFAULT_DOC: &str = "args: model:str (req). Sets this provider's default model (one of `models` when the list \
is known, else refused); a chat that names no `model` completes on it. Returns {default}.";

/// For a provider that cannot ask its API for its models: `provider_verbs(…, NO_LIVE)`.
pub const NO_LIVE: Option<fn() -> std::future::Ready<Result<Vec<String>, String>>> = None;

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

/// THE PROVIDER VERBS every backend serves the same way: `complete` (on the call's
/// `model`, else the record's `default_model`), `models` (the record's list, or the
/// API's through `live` when the record names none, and the default) and
/// `set_default_model`. `build` makes the backend's provider from a record (the pass's
/// `model` laid over the agent's). `None`: not a provider verb.
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
            // THE MODEL OF A PASS: the user's (the chat's `model`, sent with the call), else the
            // provider's `default_model`; neither, the server's own (a vLLM or ollama server reads
            // the one it serves), which becomes the default. Nothing anywhere: refused by name.
            let chosen = payload
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or(default_of(agent_id, kernel).await);
            let mut over = meta.clone();
            over.remove("model");
            if let Some(m) = &chosen {
                over.insert("model".into(), json!(m));
            }
            let mut r = complete(agent_id, payload, kernel, build(over)).await;
            if chosen.is_none() {
                match r.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    // the server named it: it is the default from now on
                    Some(m) => {
                        let m = m.to_string();
                        let _ = set_default(agent_id, kernel, &m).await;
                    }
                    None if r.get("kind").and_then(Value::as_str) == Some("ValueError") => {
                        r["error"] = json!(format!(
                            "complete: no model — the chat names none, this provider has no {DEFAULT_FIELD}, \
                             and its server names none ({})",
                            r["error"].as_str().unwrap_or("")
                        ));
                    }
                    None => {}
                }
            }
            Some(r)
        }
        "models" => {
            let mut names = listed_models(&meta);
            if names.is_empty() {
                if let Some(l) = live {
                    match l().await {
                        Ok(n) => names = n,
                        Err(e) => return Some(json!({"error": e, "reason": "unhandled", "default": default_of(agent_id, kernel).await})),
                    }
                }
            }
            Some(json!({"models": names, "default": default_of(agent_id, kernel).await}))
        }
        "set_default_model" => match payload.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            None => Some(json!({"error": "set_default_model: model required — a model name", "reason": "bad_arg"})),
            Some(m) => {
                let names = listed_models(&meta);
                if !names.is_empty() && !names.iter().any(|n| n == m) {
                    return Some(json!({"error": format!("set_default_model: `{m}` is not one of this provider's models {names:?}"),
                                       "reason": "bad_arg"}));
                }
                match set_default(agent_id, kernel, m).await {
                    Ok(()) => Some(json!({"default": m})),
                    Err(e) => Some(json!({"error": format!("set_default_model: {e}"), "reason": "unhandled"})),
                }
            }
        },
        _ => None,
    }
}

/// A provider answers no chat: `send`, `history`, `fork` … belong to an AI chat
/// (`ai_chat.tools`) that names it (`provider_id`). `reflect` and `boot` are its own.
#[must_use]
pub fn is_chat_verb(verb: &str) -> bool {
    !matches!(verb, "reflect" | "boot" | "complete" | "models" | "set_default_model")
        && crate::verbs::verb_docs().get(verb).is_some()
}

/// The refusal a provider answers a chat verb with.
#[must_use]
pub fn chat_refusal(name: &str, verb: &str) -> Value {
    let mut r = fantastic_kernel::refusal::refusal(
        fantastic_kernel::refusal::UNHANDLED,
        format!(
            "{name}: `{verb}` is a chat's verb, and this agent is a provider — it holds no conversation. \
             Chat with an `ai_chat.tools` agent whose {PROVIDER_FIELD} names this one; it completes here (`complete`)."
        ),
    );
    fantastic_kernel::refusal::with(&mut r, "verb", json!(verb));
    r
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
    let mut reply = json!({"rid": rid, "model": provider.model(), "prompt_tokens": r.prompt_tokens,
           "completion_tokens": r.completion_tokens, "stop_reason": r.stop_reason});
    // the cache counts, only when the endpoint has them
    for (key, n) in [("cache_read", r.cache_read), ("cache_write", r.cache_write)] {
        if let Some(n) = n {
            reply[key] = json!(n);
        }
    }
    reply
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
    r.cache_read = result.get("cache_read").and_then(Value::as_i64);
    r.cache_write = result.get("cache_write").and_then(Value::as_i64);
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
        // The call runs INSIDE the chat's turn (no spawn): it is the chat's send, judged
        // by its grants, and a cancel of the turn cancels the provider's pass with it.
        let call = {
            let kernel = Arc::clone(&self.kernel);
            let provider = self.provider.clone();
            Box::pin(async move { kernel.send(vec![Item::new(provider, Value::Object(payload))]).await })
        };
        let guard = Unwatch { kernel: Arc::clone(&self.kernel), provider: self.provider.clone(), reader, done: false };
        let st = Pass { call: Some(call), inbox, rid, guard, queued: std::collections::VecDeque::new(),
                        report: Arc::clone(&self.report), model: Arc::clone(&self.model), ended: false };
        let stream = futures_util::stream::unfold(st, |mut st| async move {
            loop {
                if let Some(ev) = st.queued.pop_front() {
                    return Some((ev, st));
                }
                if st.ended {
                    return None;
                }
                let Some(call) = st.call.as_mut() else {
                    st.ended = true;
                    continue;
                };
                tokio::select! {
                    out = call => {
                        st.call = None;
                        // tokens emitted before the reply are already queued: drain them
                        while let Some(it) = st.inbox.try_recv() {
                            if let Some(t) = token_of(&it.payload, &st.rid) {
                                st.queued.push_back(Ok(ProviderEvent::Token(t)));
                            }
                        }
                        st.guard.release().await;
                        let settled = match out.into_iter().next() {
                            // the provider's own answer, a failed pass included: its error as it wrote it
                            Some(o) if o.status == Status::Completed || o.result.get("rid").is_some() => {
                                settle(&o.result, &st.report, &st.model)
                            }
                            Some(o) => Err(ProviderError::raised(format!(
                                "ai_chat: complete on `{}` ended {:?}: {}",
                                st.guard.provider.as_str(),
                                o.status,
                                o.result
                            ))),
                            None => Err(ProviderError::raised("ai_chat: complete returned nothing")),
                        };
                        if let Err(e) = settled {
                            st.queued.push_back(Err(e));
                        }
                        st.ended = true;
                    }
                    item = st.inbox.recv() => {
                        if let Some(t) = item.and_then(|it| token_of(&it.payload, &st.rid)) {
                            return Some((Ok(ProviderEvent::Token(t)), st));
                        }
                    }
                }
            }
        });
        Ok(Box::pin(stream))
    }

    fn model(&self) -> String {
        self.model.lock().expect("model poisoned").clone()
    }

    fn report(&self) -> ProviderReport {
        self.report.lock().expect("report poisoned").clone()
    }

    fn clear_prompt_tokens(&self) {
        let mut r = self.report.lock().expect("report poisoned");
        r.prompt_tokens = None;
        r.cache_read = None;
        r.cache_write = None;
    }

    fn clear_stop_reason(&self) {
        self.report.lock().expect("report poisoned").stop_reason = None;
    }
}

type Call = std::pin::Pin<Box<dyn std::future::Future<Output = Vec<fantastic_kernel::Outcome>> + Send>>;

/// One pass in flight: the `complete` call, the reader's inbox, what is still to yield.
struct Pass {
    call: Option<Call>,
    inbox: fantastic_kernel::inbox::InboxReceiver,
    rid: String,
    guard: Unwatch,
    queued: std::collections::VecDeque<Result<ProviderEvent, ProviderError>>,
    report: Arc<Mutex<ProviderReport>>,
    model: Arc<Mutex<String>>,
    ended: bool,
}

/// The reader's watch of the provider: released when the pass ends, or — the turn
/// dropped or cancelled mid-pass — when the pass is dropped.
struct Unwatch {
    kernel: Arc<Kernel>,
    provider: AgentId,
    reader: AgentId,
    done: bool,
}

impl Unwatch {
    async fn release(&mut self) {
        if !self.done {
            self.done = true;
            self.kernel.unwatch(&self.provider, &self.reader).await;
            self.kernel.remove_inbox(&self.reader);
        }
    }
}

impl Drop for Unwatch {
    fn drop(&mut self) {
        if !self.done {
            let (k, p, r) = (Arc::clone(&self.kernel), self.provider.clone(), self.reader.clone());
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                h.spawn(async move {
                    k.unwatch(&p, &r).await;
                    k.remove_inbox(&r);
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "tests_remote_provider.rs"]
mod tests_remote_provider;

/// TESTS AND TOOLS: an `ai_chat` agent completing through `provider` — `<provider>_ai`,
/// under the same parent, on the provider's memory store (its `memory_store_id`), with
/// both ends of the edge granted (the chat may `complete` and `watch` the provider, and
/// call what the provider may; the provider and the store admit it). The chat holds the
/// conversation; the provider keeps answering `complete`. Returns the chat's id. A
/// provider's own tests drive a whole turn through it.
pub async fn pair_chat(kernel: &Arc<Kernel>, provider: &AgentId) -> AgentId {
    let chat = AgentId::from(format!("{}_ai", provider.as_str()));
    if kernel.agents.contains_key(&chat) {
        return chat;
    }
    let pm = crate::helpers::agent_meta(provider, kernel);
    let parent = kernel
        .agents
        .get(provider)
        .and_then(|a| a.parent_id.clone())
        .map_or_else(|| "kernel".to_string(), |p| p.as_str().to_string());
    let mut can_call = pm.get("can_call").and_then(Value::as_object).cloned().unwrap_or_default();
    can_call.insert(provider.as_str().to_string(), json!(["complete", "watch", "reflect"]));
    let mut create = json!({"type": "create_agent", "parent_id": parent, "handler_module": crate::chat_bundle::HANDLER_MODULE,
                            "id": chat.as_str(), PROVIDER_FIELD: provider.as_str(), "can_call": can_call,
                            "callable_by": pm.get("callable_by").cloned().unwrap_or(json!({"$self": "*"}))});
    // a provider record that names a `model` (a test's): the chat runs it, as a user's choice
    if let Some(m) = pm.get("model").filter(|m| m.as_str().is_some_and(|s| !s.is_empty())) {
        create["model"] = m.clone();
    }
    for k in ["memory_store_id", "file_bridge_id", "context_window", "context_strategy", "caching"] {
        if let Some(v) = pm.get(k) {
            create[k] = v.clone();
        }
    }
    if create.get("context_window").is_none() {
        // the model's own window, as the provider reports it
        let r = kernel.send(vec![Item::new(provider.clone(), json!({"type": "reflect"}))]).await;
        if let Some(w) = r.first().and_then(|o| o.result.get("context_window")).filter(|w| w.is_number()) {
            create["context_window"] = w.clone();
        }
    }
    let root = kernel.root().map_or_else(|| AgentId::from("kernel"), |r| r.id.clone());
    let _ = kernel.send(vec![Item::new(root, create)]).await;
    // the other ends: the provider admits the chat, and so does everything that admits the provider
    let me = chat.as_str().to_string();
    let _ = kernel.edit_callable_by(provider, |m| {
        m.insert(me.clone(), json!(["complete", "watch", "reflect"]));
        true
    });
    let all: Vec<Arc<fantastic_kernel::Agent>> = kernel.agents.iter().map(|e| Arc::clone(e.value())).collect();
    for a in all {
        let admits = a
            .meta
            .read()
            .ok()
            .and_then(|m| m.get("callable_by").and_then(Value::as_object).map(|c| c.get(provider.as_str()).cloned()))
            .flatten();
        if let Some(verbs) = admits {
            let me = me.clone();
            let _ = kernel.edit_callable_by(&a.id, move |m| {
                m.insert(me, verbs);
                true
            });
        }
    }
    chat
}
