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
//! The chat's own line names the provider: `can: {<provider>: {call: [complete,
//! watch, capabilities]}}` (`capabilities`: what its models can do, see
//! [`crate::images`]); the provider keeps no list of callers.
//!
//! ONE PROVIDER, MANY CHATS (#632): the provider's record is its IDENTITY (endpoint,
//! key, model, window, KV policy, timeouts); the REQUEST KNOBS are each chat's own
//! ([`crate::record::RequestKnobs`]) and ride every `complete` it sends (`knobs`),
//! laid over the provider's record for that pass only. Every request reads both
//! records as they are then, so a change to either applies from the next request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::refusal::{
    refusal, with, MISSING_ARG, NOT_FOUND, STORE_REFUSED, TRANSPORT_ERROR,
};
use fantastic_kernel::{AgentId, Item, Kernel, Status};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use crate::declared::{Complete, SetDefaultModel};
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, Unconfigured};
use crate::think::{ThinkMeter, THINKING_CUT, THINK_CLOSE};

/// The event type a provider's tokens carry on its stream.
pub const TOKEN_EVENT: &str = "provider_token";

/// The record field naming a chat's provider.
pub const PROVIDER_FIELD: &str = "provider_id";

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

/// For a provider that cannot ask its API for its models: `provider_verbs(…, NO_LIVE)`.
pub const NO_LIVE: Option<LiveModels> = None;

/// A provider's live model listing, as [`handle`] takes it.
pub type LiveModels = fn() -> std::future::Ready<Result<Vec<String>, String>>;

/// The names a provider's record lists in `models`.
#[must_use]
pub fn listed_models(meta: &Map<String, Value>) -> Vec<String> {
    meta.get("models")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// `complete`: one pass on the call's `model`, else the record's `default_model`, with the
/// call's `knobs`. `build` makes the backend's provider from a record: the agent's, with the
/// pass's `model` and the calling chat's knobs laid over it.
pub async fn complete_verb<B>(
    agent_id: &AgentId,
    args: &Complete,
    kernel: &Arc<Kernel>,
    build: B,
) -> Value
where
    B: Fn(Map<String, Value>) -> Arc<dyn Provider>,
{
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    // THE MODEL OF A PASS: the user's (the chat's `model`, sent with the call), else the
    // provider's `default_model`; neither, the server's own (a vLLM or ollama server reads
    // the one it serves), which becomes the default. Nothing anywhere: refused by name.
    let chosen = args
        .model
        .clone()
        .filter(|s| !s.is_empty())
        .or(default_of(agent_id, kernel).await);
    let mut over = meta.clone();
    over.remove("model");
    if let Some(m) = &chosen {
        over.insert("model".into(), json!(m));
    }
    // the request knobs are the calling chat's alone: never the provider's record
    for k in crate::record::KNOBS {
        over.remove(k);
    }
    if let Some(Value::Object(knobs)) = args.knobs.as_ref().and_then(|k| serde_json::to_value(k).ok()) {
        over.extend(knobs);
    }
    let mut r = complete(agent_id, args, kernel, build(over)).await;
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
    r
}

/// `models`: the record's list, or the API's through `live` when the record names
/// none, and the default.
pub async fn models_verb<L, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, live: Option<L>) -> Value
where
    L: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
{
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let mut names = listed_models(&meta);
    if names.is_empty() {
        if let Some(l) = live {
            match l().await {
                Ok(n) => names = n,
                Err(e) => {
                    let mut r = refusal(TRANSPORT_ERROR, e);
                    with(&mut r, "default", json!(default_of(agent_id, kernel).await));
                    return r;
                }
            }
        }
    }
    json!({"models": names, "default": default_of(agent_id, kernel).await})
}

/// `set_default_model`: keep `model` as the default, one of the record's `models`
/// when that list is known.
pub async fn set_default_verb(agent_id: &AgentId, args: &SetDefaultModel, kernel: &Arc<Kernel>) -> Value {
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let m = args.model.as_str();
    let names = listed_models(&meta);
    if !names.is_empty() && !names.iter().any(|n| n == m) {
        let mut r = refusal(
            NOT_FOUND,
            format!("set_default_model: `{m}` is not one of this provider's models {names:?}"),
        );
        with(&mut r, "arg", json!("model"));
        return r;
    }
    match set_default(agent_id, kernel, m).await {
        Ok(()) => json!({"default": m}),
        Err(e) => refusal(STORE_REFUSED, format!("set_default_model: {e}")),
    }
}

/// THE PROVIDER SIDE: run one pass of `provider` for `args` and stream its
/// tokens on `agent_id`'s own stream, tagged with the caller's `rid`.
pub async fn complete(agent_id: &AgentId, args: &Complete, kernel: &Arc<Kernel>, provider: Arc<dyn Provider>) -> Value {
    let rid = args.rid.clone();
    // the schema lets a call carry `image` instead of `messages`; such a call's bytes are
    // taken at the binary entry, so a pass is never asked for without them
    let Some(messages) = args.messages.as_ref() else {
        let mut r = refusal(MISSING_ARG, "complete: messages required");
        with(&mut r, "arg", json!("messages"));
        with(&mut r, "rid", json!(rid));
        return r;
    };
    let messages: Vec<Value> = messages.iter().cloned().map(Value::Object).collect();
    let max_tokens = args.max_tokens;
    if let Err(e) = provider.ready() {
        return failed(&rid, &e);
    }
    provider.clear_prompt_tokens();
    provider.clear_stop_reason();
    let mut stream = match provider.chat(&messages, max_tokens).await {
        Ok(s) => s,
        Err(e) => return failed(&rid, &e),
    };
    let mut meter = harness_meter(args, provider.as_ref());
    let mut cut = false;
    // THE WHOLE ANSWER rides the reply (#795): the chat parses it, never the tokens,
    // which feed only the live view
    let mut whole = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(text)) => {
                let over = meter.as_mut().is_some_and(|m| m.feed(&text) && m.is_open());
                whole.push_str(&text);
                kernel
                    .emit_quiet(agent_id, json!({"type": TOKEN_EVENT, "rid": rid, "text": text}))
                    .await;
                if over {
                    // the thinking ends here: its span is closed and the request dropped
                    whole.push_str(THINK_CLOSE);
                    kernel
                        .emit_quiet(agent_id, json!({"type": TOKEN_EVENT, "rid": rid, "text": THINK_CLOSE}))
                        .await;
                    cut = true;
                    break;
                }
            }
            Err(e) => return failed(&rid, &e),
        }
    }
    drop(stream);
    let r = provider.report();
    let stop_reason = if cut { Some(THINKING_CUT.to_string()) } else { r.stop_reason };
    let mut reply = json!({"rid": rid, "model": provider.model(), "prompt_tokens": r.prompt_tokens,
           "completion_tokens": r.completion_tokens, "stop_reason": stop_reason, "text": whole});
    // the cache counts, only when the endpoint has them
    for (key, n) in [("cache_read", r.cache_read), ("cache_write", r.cache_write)] {
        if let Some(n) = n {
            reply[key] = json!(n);
        }
    }
    reply
}

/// THE HARNESS THINKING CAP (#782): a meter for a chat's TURN (no `max_tokens` of its
/// own: a side call is bounded by that) whose knobs carry a `thinking_token_budget` the
/// provider's server cannot enforce. Past the cap, [`complete`] closes the thinking span,
/// drops the request and reports `stop_reason` [`THINKING_CUT`]; the chat re-asks once
/// for the answer (`agent_loop::attempts`).
fn harness_meter(args: &Complete, provider: &dyn Provider) -> Option<ThinkMeter> {
    if args.max_tokens.is_some() || provider.enforces_thinking_budget() {
        return None;
    }
    let budget = args.knobs.as_ref()?.thinking_token_budget?;
    Some(ThinkMeter::new(budget))
}

fn failed(rid: &str, e: &ProviderError) -> Value {
    let mut out = json!({"rid": rid, "error": e.message, "kind": e.kind});
    // the class rides the one machine field: the link, or a failure no retry mends
    if let Some(reason) = e.reason() {
        out["reason"] = json!(reason);
    }
    out
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
    /// the chat's request knobs, sent with every pass
    knobs: Map<String, Value>,
}

impl RemoteProvider {
    /// The provider `provider` (a local id or `<guid>|<id>`) of the chat `chat`.
    #[must_use]
    pub fn new(kernel: &Arc<Kernel>, chat: &AgentId, provider: &str) -> Self {
        let meta = crate::helpers::agent_meta(chat, kernel);
        let knobs = crate::record::knobs_of(&meta);
        let chat_model = meta
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let model = chat_model.clone().unwrap_or_default();
        Self {
            chat_model,
            knobs,
            kernel: Arc::clone(kernel),
            chat: chat.clone(),
            provider: AgentId::from(provider),
            report: Arc::default(),
            model: Arc::new(Mutex::new(model)),
        }
    }
}

impl RemoteProvider {
    /// THE IMAGES OF A REQUEST, on the binary path: each image part of `messages` is pulled
    /// from the chat's store (`get_bytes`) and sent to the provider as ONE `complete` item
    /// of this `rid` (`image: {ref, media_type}`, the bytes as the body at `data`), before
    /// the `complete` that carries the messages. A request with no image sends nothing.
    async fn send_images(&self, messages: &[Value], rid: &str) -> Result<(), ProviderError> {
        for part in messages.iter().filter_map(crate::images::parts_of).flatten() {
            let Some(image_ref) = part.get("ref").and_then(Value::as_str) else {
                continue;
            };
            let (media, bytes) = crate::memory::get_bytes(&self.chat, &self.kernel, image_ref)
                .await
                .map_err(|e| ProviderError::raised(e.0))?;
            let header = json!({"type": "complete", "rid": rid, "image": {"ref": image_ref, "media_type": media}});
            let out = self
                .kernel
                .send(vec![Item::new(self.provider.clone(), header).with_binary_key("data", bytes)])
                .await
                .remove(0);
            if out.status != Status::Completed || out.result.get("error").is_some() {
                return Err(ProviderError::raised(format!(
                    "ai_chat: provider `{}` took no image `{image_ref}`: {}",
                    self.provider.as_str(),
                    out.result.get("error").and_then(Value::as_str).unwrap_or("it refused")
                )));
            }
        }
        Ok(())
    }
}

/// The reply of a `complete`, read into the report; `Err` for a failed pass.
fn settle(result: &Value, report: &Mutex<ProviderReport>, model: &Mutex<String>) -> Result<(), ProviderError> {
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        let mut e = if result.get("kind").and_then(Value::as_str) == Some("ValueError") {
            ProviderError::value_error(err)
        } else {
            ProviderError::raised(err)
        };
        e.class = crate::provider::reason_class(result.get("reason").and_then(Value::as_str));
        return Err(e);
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

impl RemoteProvider {
    /// The `complete` payload of one request.
    fn payload(&self, messages: &[Value], max_tokens: Option<i64>, rid: &str) -> Map<String, Value> {
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
        if !self.knobs.is_empty() {
            payload.insert("knobs".into(), Value::Object(self.knobs.clone()));
        }
        payload
    }

    /// A reader's inbox watching the provider's stream for one request: its id and
    /// inbox, or why the provider cannot be watched.
    async fn reader(&self, n: u64) -> Result<(AgentId, fantastic_kernel::inbox::InboxReceiver), ProviderError> {
        let reader = AgentId::from(format!("_provider_reader_{}_{n}", self.chat.as_str()));
        let inbox = self.kernel.ensure_inbox(&reader);
        if let Some(refused) = self.kernel.watch(&self.provider, reader.clone()).await {
            self.kernel.remove_inbox(&reader);
            return Err(ProviderError::raised(format!(
                "ai_chat: cannot watch provider `{}`: {}",
                self.provider.as_str(),
                refused.get("error").and_then(Value::as_str).unwrap_or(&refused.to_string())
            )));
        }
        Ok((reader, inbox))
    }

    /// THE CHAT'S REQUEST AS A MESSAGE (#795): the `complete` call goes out through
    /// [`Kernel::call`] and this returns its pid at once. The provider's tokens feed
    /// `live` from a watcher of its stream that holds no lock and wakes nothing; the
    /// settle runs `done` with the WHOLE answer the reply carries (`text`), so a token
    /// the live view dropped never changes what is read. A call refused at launch runs
    /// `done` with the refusal and hands back no pid.
    pub(crate) async fn begin(
        &self,
        messages: Vec<Value>,
        max_tokens: Option<i64>,
        deadline: (std::time::Duration, f64),
        live: Arc<dyn crate::provider::Live>,
        done: crate::provider::Done,
    ) -> Option<fantastic_kernel::TaskId> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let rid = format!("{}~{n}", self.chat.as_str());
        if let Err(e) = self.send_images(&messages, &rid).await {
            done(Err(e)).await;
            return None;
        }
        let (reader, mut inbox) = match self.reader(n).await {
            Ok(r) => r,
            Err(e) => {
                done(Err(e)).await;
                return None;
            }
        };
        let guard = Unwatch { kernel: Arc::clone(&self.kernel), provider: self.provider.clone(), reader, done: false };
        // THE LIVE VIEW: the tokens of this request, until the settle says it ended;
        // what is still queued then is fed first. It carries the chat's identity (no
        // `ps` row): the live view's events are the chat's own, and an emit outside
        // any identity is dropped
        let (end_tx, mut end_rx) = tokio::sync::oneshot::channel::<()>();
        let feed_rid = rid.clone();
        let feeder = fantastic_kernel::spawn_inherit(async move {
            loop {
                tokio::select! {
                    biased;
                    item = inbox.recv() => match item {
                        Some(it) => {
                            if let Some(t) = token_of(&it.payload, &feed_rid) {
                                live.token(&t).await;
                            }
                        }
                        None => break,
                    },
                    _ = &mut end_rx => {
                        while let Some(it) = inbox.try_recv() {
                            if let Some(t) = token_of(&it.payload, &feed_rid) {
                                live.token(&t).await;
                            }
                        }
                        break;
                    }
                }
            }
        });
        let done = Arc::new(Mutex::new(Some(done)));
        let (within, ceiling) = deadline;
        let item = Item::new(self.provider.clone(), Value::Object(self.payload(&messages, max_tokens, &rid)))
            .with_timeout(within.as_secs_f64());
        let (report, model, provider) = (Arc::clone(&self.report), Arc::clone(&self.model), self.provider.clone());
        let then_done = Arc::clone(&done);
        let out = self
            .kernel
            .call(item, move |o| async move {
                let _ = end_tx.send(());
                let _ = feeder.await;
                let mut guard = guard;
                guard.release().await;
                let read = answer_of(&o, &report, &model, &provider, ceiling);
                let done = then_done.lock().expect("done poisoned").take();
                if let Some(done) = done {
                    done(read).await;
                }
            })
            .await;
        if out.status == Status::Accepted {
            return out.pid;
        }
        // REFUSED AT LAUNCH (the gate, the target): the continuation never runs
        let done = done.lock().expect("done poisoned").take();
        if let Some(done) = done {
            done(Err(ProviderError::raised(format!(
                "ai_chat: complete on `{}` was refused: {}",
                self.provider.as_str(),
                fantastic_kernel::refusal::error_text(&out.result).unwrap_or_else(|| out.result.to_string())
            ))))
            .await;
        }
        None
    }
}

/// The settle of a `complete` call: its whole answer (`text`), the report read into the
/// provider's cells; `Err` for a failed, cancelled or timed-out request.
fn answer_of(
    o: &fantastic_kernel::Outcome,
    report: &Mutex<ProviderReport>,
    model: &Mutex<String>,
    provider: &AgentId,
    ceiling: f64,
) -> Result<String, ProviderError> {
    match o.status {
        // the provider's own answer, a failed pass included: its error as it wrote it
        Status::Completed | Status::Errored if o.status == Status::Completed || o.result.get("rid").is_some() => {
            settle(&o.result, report, model)?;
            Ok(o.result.get("text").and_then(Value::as_str).unwrap_or_default().to_string())
        }
        Status::Cancelled => Err(ProviderError::cancelled(format!(
            "ai_chat: complete on `{}` was cancelled",
            provider.as_str()
        ))),
        Status::TimedOut => Err(ProviderError::timed_out(ceiling)),
        _ => Err(ProviderError::raised(format!(
            "ai_chat: complete on `{}` ended {:?}: {}",
            provider.as_str(),
            o.status,
            o.result
        ))),
    }
}

#[async_trait]
impl Provider for RemoteProvider {
    fn remote(&self) -> Option<&RemoteProvider> {
        Some(self)
    }

    /// A HELPER'S REQUEST (a fold summary, the owed recall): read as a stream inside
    /// the pass that asks it. The chat's own requests go out by [`RemoteProvider::begin`].
    async fn chat(&self, messages: &[Value], max_tokens: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let rid = format!("{}~{n}", self.chat.as_str());
        self.send_images(messages, &rid).await?;
        // the reader: an inbox of this request only, watching the provider's stream
        let (reader, inbox) = self.reader(n).await?;
        let payload = self.payload(messages, max_tokens, &rid);
        // The helper's call runs inside the pass that asks it: it is the chat's send,
        // judged by its grants.
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

/// TESTS: an `ai_chat` agent completing through `provider` — `<provider>_ai`,
/// under the same parent, on the provider's memory store (its `memory_store_id`),
/// made by the operator. Its `can` is one line, `{<provider>: {call: [complete, watch,
/// capabilities]}}`: the provider's own lines (its key line among them) are never copied
/// into the chat; the chat's other lines come from its bundle's defaults. The chat
/// holds the conversation; the provider keeps answering `complete`. Returns the
/// chat's id. A provider's own tests drive a whole turn through it. Test support
/// only (feature `test-support`): nothing shipped creates as the operator here.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub async fn pair_chat(kernel: &Arc<Kernel>, provider: &AgentId) -> AgentId {
    let chat = AgentId::from(format!("{}_ai", provider.as_str()));
    if kernel.exists(chat.as_str()) {
        return chat;
    }
    let pm = crate::helpers::agent_meta(provider, kernel);
    let parent = kernel.resolve_target(provider.as_str())
        .and_then(|a| a.parent_id.clone())
        .map_or_else(|| "kernel".to_string(), |p| p.as_str().to_string());
    let can = json!({provider.as_str(): {"call": ["complete", "watch", "capabilities"]}});
    let mut create = json!({"type": "create_agent", "parent_id": parent, "handler_module": crate::chat_bundle::HANDLER_MODULE,
                            "id": chat.as_str(), PROVIDER_FIELD: provider.as_str(), "can": can});
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
        let r = kernel
            .operator_send(vec![Item::new(provider.clone(), json!({"type": "reflect"}))])
            .await;
        if let Some(w) = r.first().and_then(|o| o.result.get("context_window")).filter(|w| w.is_number()) {
            create["context_window"] = w.clone();
        }
    }
    let root = kernel.root().map_or_else(|| AgentId::from("kernel"), |r| r.id.clone());
    let _ = kernel.operator_send(vec![Item::new(root, create)]).await;
    chat
}

/// EVERY VERB OF A PROVIDER BACKEND, answered from its typed call: `boot` is the
/// shared incarnation reset, `reflect` is the backend's own (handed in), and the
/// rest are the provider verbs above. `build` makes the backend's provider from a
/// record; `live` lists the models the API serves when the record names none.
pub async fn handle<B, L, Fut, R>(
    agent_id: &AgentId,
    call: crate::declared::provider::Call,
    kernel: &Arc<Kernel>,
    build: B,
    live: Option<L>,
    reflect: R,
) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError>
where
    B: Fn(Map<String, Value>) -> Arc<dyn Provider>,
    L: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
    R: FnOnce() -> Value,
{
    use crate::declared::provider::Call;
    let reply = match call {
        Call::Reflect(_) => reflect(),
        Call::Boot(_) => crate::verbs::boot(agent_id, kernel).await,
        Call::Complete(a) => complete_verb(agent_id, &a, kernel, build).await,
        Call::Models(_) => models_verb(agent_id, kernel, live).await,
        Call::SetDefaultModel(a) => set_default_verb(agent_id, &a, kernel).await,
    };
    Ok((!reply.is_null()).then_some(reply))
}
