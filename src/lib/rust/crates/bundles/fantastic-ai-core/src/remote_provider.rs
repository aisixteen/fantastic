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

/// THE NEWEST `setting` ROW NAMED `name` in the provider's store, as a STEP (#795): one
/// `read` to the store; `next` gets its value (`None`: no store, no such row, a refusal).
pub async fn setting<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, name: &str, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Option<Value>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(store) = store_of(agent_id, kernel) else {
        return next(rec, None).await;
    };
    let item = Item::new(
        store,
        json!({"type": "read", "kinds": [SETTING], "match": {"name": name}, "order": "desc", "limit": 1}),
    );
    crate::steps::call(kernel, item, rec, move |rec, out| async move {
        let v = out.result.get("rows").and_then(Value::as_array).and_then(|r| r.first()).and_then(|r| r.get("value")).cloned();
        next(rec, v).await;
    })
    .await;
}

/// KEEP `value` as the provider's `name` setting (a `setting` row in its store), as a
/// STEP (#795): `next` gets `Err` naming why it was not kept.
async fn keep_setting<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, name: &str, value: Value, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Result<(), String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(store) = store_of(agent_id, kernel) else {
        return next(rec, Err("the provider names no memory_store_id to keep its settings in".to_string())).await;
    };
    let item = Item::new(
        store,
        json!({"type": "append", "rows": [{"kind": SETTING, "name": name, "value": value}]}),
    );
    crate::steps::call(kernel, item, rec, move |rec, out| async move {
        let r = match out.result.get("error") {
            None if out.status == Status::Completed => Ok(()),
            None => Err(format!("the store did not answer: {}", out.status.as_str())),
            Some(e) => Err(e.as_str().unwrap_or("the store refused").to_string()),
        };
        next(rec, r).await;
    })
    .await;
}

/// A PROVIDER'S DEFAULT MODEL, as a STEP: the newest `default_model` setting in its store.
pub async fn default_model<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Option<String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    setting(agent_id, kernel, DEFAULT_FIELD, rec, move |rec, v| {
        next(rec, v.as_ref().and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from))
    })
    .await;
}

/// A PROVIDER'S KEPT USAGE (total, by_model, by_agent), as a STEP: the newest `usage`
/// setting in its store. None: it has no store, or has kept none yet.
pub async fn kept_usage<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Option<Value>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    setting(agent_id, kernel, USAGE_FIELD, rec, move |rec, v| next(rec, v.filter(Value::is_object))).await;
}

/// KEEP `usage` as the provider's usage (a `usage` setting row in its store), as a
/// STEP. A provider with no store keeps none: `Ok`. `Err` names why a store refused.
pub async fn keep_usage<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, usage: &Value, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Result<(), String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if store_of(agent_id, kernel).is_none() {
        return next(rec, Ok(())).await;
    }
    keep_setting(agent_id, kernel, USAGE_FIELD, usage.clone(), rec, next).await;
}

/// For a provider that cannot ask its API for its models: `handle(…, NO_LIVE, …)`.
pub const NO_LIVE: Option<LiveModels> = None;

/// A provider's live model listing, as [`handle`] takes it: given the key read for the
/// record's endpoint, the names the API serves.
pub type LiveModels = fn(Arc<dyn crate::credentials::KeySource>) -> std::future::Ready<Result<Vec<String>, String>>;

/// HOW A PROVIDER READS ITS KEY before a request: the name its refusals carry, and
/// whether a record without `credentials` is refused rather than keyless. `None` in
/// [`handle`]: a provider that takes no key.
#[derive(Debug, Clone, Copy)]
pub struct Keys {
    /// The backend's name, the prefix of its refusals.
    pub backend: &'static str,
    /// A record without `credentials` is refused.
    pub required: bool,
}

/// THE KEY OF A REQUEST TO THE RECORD'S `endpoint`, as a STEP: `next` gets it
/// ([`crate::credentials::KeyReader::key`]); a provider that takes none gets no key.
pub async fn key_of<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, keys: Option<Keys>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Arc<dyn crate::credentials::KeySource>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let endpoint = crate::helpers::agent_meta(agent_id, kernel)
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match keys {
        Some(k) => {
            crate::credentials::key_reader(agent_id, kernel, k.backend, k.required)
                .key(&endpoint, rec, next)
                .await;
        }
        None => {
            let none: Arc<dyn crate::credentials::KeySource> = Arc::new(crate::credentials::ReadKey::new(&endpoint, Ok(None)));
            next(rec, none).await;
        }
    }
}

/// The names a provider's record lists in `models`.
#[must_use]
pub fn listed_models(meta: &Map<String, Value>) -> Vec<String> {
    meta.get("models")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// What a backend reads from its API before a pass's request, given the key and the
/// pass's `complete` (its model chosen: the call's, else the default): the `complete`
/// to run and the backend's own record fields the read found (written back as a step
/// before the pass, #795), or the verb's refusal. Its own I/O, in the verb's task.
pub type Prepare = Box<
    dyn FnOnce(Arc<dyn crate::credentials::KeySource>, Complete, Option<String>) -> futures_util::future::BoxFuture<'static, Result<Prepared, Value>>
        + Send,
>;

/// What a [`Prepare`] read: the `complete` to run, and the record fields to write back
/// first (empty: nothing to write).
pub struct Prepared {
    /// The `complete` to run.
    pub args: Complete,
    /// The backend's own record fields the read found.
    pub write: Map<String, Value>,
}

/// `complete`: one pass on the call's `model`, else the record's `default_model`, with the
/// call's `knobs` — as STEPS (#795): the default read, the key read, then the pass, its
/// own I/O, in a task under this execution (its `cancel` and deadline end the stream);
/// a model the server named becomes the default (a step), then `settle` settles it.
/// `prepare`: what the backend reads from its API first. `build` makes the backend's
/// provider from a record (the agent's, with the pass's `model` and the calling chat's
/// knobs laid over it) and the key read.
pub async fn complete_verb<B>(
    agent_id: &AgentId,
    args: Complete,
    kernel: &Arc<Kernel>,
    keys: Option<Keys>,
    prepare: Option<Prepare>,
    build: B,
    settle: crate::steps::Settle,
) where
    B: FnOnce(Map<String, Value>, Arc<dyn crate::credentials::KeySource>) -> Arc<dyn Provider> + Send + 'static,
{
    // THE MODEL OF A PASS: the user's (the chat's `model`, sent with the call), else the
    // provider's `default_model`; neither, the server's own (a vLLM or ollama server reads
    // the one it serves), which becomes the default. Nothing anywhere: refused by name.
    let pass = PassCall { me: agent_id.clone(), kernel: Arc::clone(kernel), keys, prepare, build, settle };
    match args.model.clone().filter(|s| !s.is_empty()) {
        Some(m) => pass.keyed(args, Some(m)).await,
        None => {
            default_model(agent_id, kernel, (pass, args), |(pass, args), chosen| pass.keyed(args, chosen)).await;
        }
    }
}

/// One `complete` between its steps.
struct PassCall<B> {
    me: AgentId,
    kernel: Arc<Kernel>,
    keys: Option<Keys>,
    prepare: Option<Prepare>,
    build: B,
    settle: crate::steps::Settle,
}

impl<B> PassCall<B>
where
    B: FnOnce(Map<String, Value>, Arc<dyn crate::credentials::KeySource>) -> Arc<dyn Provider> + Send + 'static,
{
    /// The model is chosen: the key is read (a step), then the pass runs.
    async fn keyed(self, args: Complete, chosen: Option<String>) {
        let (me, k, keys) = (self.me.clone(), Arc::clone(&self.kernel), self.keys);
        key_of(&me, &k, keys, (self, args, chosen), |(pass, args, chosen), key| async move {
            pass.run(args, chosen, key);
        })
        .await;
    }

    /// THE PASS, the provider's own I/O, in a task under this execution.
    fn run(self, args: Complete, chosen: Option<String>, key: Arc<dyn crate::credentials::KeySource>) {
        let Self { me, kernel, keys: _, prepare, build, settle } = self;
        let forker = settle.forker();
        let owner = me.clone();
        let again = forker.clone();
        forker.run(&owner, "complete", async move {
            let (args, write) = match prepare {
                Some(p) => match p(Arc::clone(&key), args, chosen.clone()).await {
                    Ok(p) => (p.args, p.write),
                    Err(refused) => return settle.reply(refused),
                },
                None => (args, Map::new()),
            };
            if write.is_empty() {
                return Self::stream(me, kernel, build, settle, args, chosen, key).await;
            }
            // what the read found is the backend's own record first (a step); the pass
            // runs once it is written, in a task under this execution again
            let (m, k) = (me.clone(), Arc::clone(&kernel));
            kernel
                .write_own(&me, write, move |written| async move {
                    if fantastic_kernel::is_refused(&written) {
                        return settle.reply(written);
                    }
                    let owner = m.clone();
                    again.run(&owner, "complete", Self::stream(m, k, build, settle, args, chosen, key));
                })
                .await;
        });
    }

    /// The pass's request and stream, once the backend's record holds what it needs.
    async fn stream(
        me: AgentId,
        kernel: Arc<Kernel>,
        build: B,
        settle: crate::steps::Settle,
        args: Complete,
        chosen: Option<String>,
        key: Arc<dyn crate::credentials::KeySource>,
    ) {
        {
            let mut over = crate::helpers::agent_meta(&me, &kernel);
            over.remove("model");
            if let Some(m) = &chosen {
                over.insert("model".into(), json!(m));
            }
            // the request knobs are the calling chat's alone: never the provider's record
            for kn in crate::record::KNOBS {
                over.remove(kn);
            }
            if let Some(Value::Object(knobs)) = args.knobs.as_ref().and_then(|kn| serde_json::to_value(kn).ok()) {
                over.extend(knobs);
            }
            let mut r = complete(&me, &args, &kernel, build(over, key)).await;
            if chosen.is_some() {
                return settle.reply(r);
            }
            match r.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                // the server named it: it is the default from now on (a step)
                Some(m) => {
                    let m = m.to_string();
                    keep_setting(&me, &kernel, DEFAULT_FIELD, json!(m), (r, settle), |(r, settle), _| async move {
                        settle.reply(r);
                    })
                    .await;
                }
                None => {
                    if r.get("kind").and_then(Value::as_str) == Some("ValueError") {
                        r["error"] = json!(format!(
                            "complete: no model — the chat names none, this provider has no {DEFAULT_FIELD}, \
                             and its server names none ({})",
                            r["error"].as_str().unwrap_or("")
                        ));
                    }
                    settle.reply(r);
                }
            }
        }
    }
}

/// `models`: the record's list, or the API's through `live` (given the key read for the
/// record's endpoint, its own I/O) when the record names none, and the default — as
/// STEPS (#795): the key and the default are reads; `settle` settles it.
pub async fn models_verb<L, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, keys: Option<Keys>, live: Option<L>, settle: crate::steps::Settle)
where
    L: FnOnce(Arc<dyn crate::credentials::KeySource>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Vec<String>, String>> + Send + 'static,
{
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let names = listed_models(&meta);
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    // the default, then the answer
    let answer = move |settle: crate::steps::Settle, listed: Result<Vec<String>, String>| async move {
        default_model(&me, &k, settle, move |settle, default| async move {
            settle.reply(match listed {
                Ok(names) => json!({"models": names, "default": default}),
                Err(e) => {
                    let mut r = refusal(TRANSPORT_ERROR, e);
                    with(&mut r, "default", json!(default));
                    r
                }
            });
        })
        .await;
    };
    let Some(live) = live.filter(|_| names.is_empty()) else {
        return answer(settle, Ok(names)).await;
    };
    let owner = agent_id.clone();
    key_of(agent_id, kernel, keys, settle, move |settle, key| async move {
        let forker = settle.forker();
        forker.run(&owner, "models", async move {
            let listed = live(key).await;
            answer(settle, listed).await;
        });
    })
    .await;
}

/// `set_default_model`: keep `model` as the default, one of the record's `models`
/// when that list is known — the write a step (#795); `settle` settles it.
pub async fn set_default_verb(agent_id: &AgentId, args: &SetDefaultModel, kernel: &Arc<Kernel>, settle: crate::steps::Settle) {
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let m = args.model.clone();
    let names = listed_models(&meta);
    if !names.is_empty() && !names.contains(&m) {
        let mut r = refusal(
            NOT_FOUND,
            format!("set_default_model: `{m}` is not one of this provider's models {names:?}"),
        );
        with(&mut r, "arg", json!("model"));
        return settle.reply(r);
    }
    keep_setting(agent_id, kernel, DEFAULT_FIELD, json!(m), settle, move |settle, r| async move {
        settle.reply(match r {
            Ok(()) => json!({"default": m}),
            Err(e) => refusal(STORE_REFUSED, format!("set_default_model: {e}")),
        });
    })
    .await;
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

/// THE IMAGES OF A REQUEST, on the binary path, as STEPS (#795): each image part of
/// `messages` is pulled from the chat's store (`get_bytes`, a step) and sent to the
/// provider as ONE `complete` item of this `rid` (`image: {ref, media_type}`, the bytes as
/// the body at `data`, a step), before the `complete` that carries the messages; `next`
/// runs once every one was taken, or with why one was not. A request with no image runs
/// `next` at once.
fn send_images<R, F, Fut>(
    kernel: Arc<Kernel>,
    chat: AgentId,
    provider: AgentId,
    mut refs: std::collections::VecDeque<String>,
    rid: String,
    rec: R,
    next: F,
) -> futures_util::future::BoxFuture<'static, ()>
where
    R: Send + 'static,
    F: FnOnce(R, Result<(), ProviderError>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    Box::pin(async move {
        let Some(image_ref) = refs.pop_front() else {
            return next(rec, Ok(())).await;
        };
        let (k, me) = (Arc::clone(&kernel), chat.clone());
        let ask = json!({"type": "get_bytes", "ref": image_ref});
        crate::steps::store(&me, &k, ask, None, (rec, next), move |(rec, next), got| async move {
            let (head, body) = match got {
                Ok(r) => r,
                Err(e) => return next(rec, Err(ProviderError::raised(e.0))).await,
            };
            let media = head.get("media_type").and_then(Value::as_str).unwrap_or("").to_string();
            let Some(bytes) = body else {
                let e = format!("{chat}: memory store returned no bytes for image `{image_ref}`");
                return next(rec, Err(ProviderError::raised(e))).await;
            };
            let header = json!({"type": "complete", "rid": rid, "image": {"ref": image_ref, "media_type": media}});
            let item = Item::new(provider.clone(), header).with_binary_key("data", bytes);
            let k = Arc::clone(&kernel);
            crate::steps::call(&k, item, (rec, next), move |(rec, next), out| async move {
                if out.status != Status::Completed || out.result.get("error").is_some() {
                    let e = format!(
                        "ai_chat: provider `{}` took no image `{image_ref}`: {}",
                        provider.as_str(),
                        out.result.get("error").and_then(Value::as_str).unwrap_or("it refused")
                    );
                    return next(rec, Err(ProviderError::raised(e))).await;
                }
                send_images(kernel, chat, provider, refs, rid, rec, next).await;
            })
            .await;
        })
        .await;
    })
}

/// The image refs of `messages`, in order.
fn image_refs(messages: &[Value]) -> std::collections::VecDeque<String> {
    messages
        .iter()
        .filter_map(crate::images::parts_of)
        .flatten()
        .filter_map(|part| part.get("ref").and_then(Value::as_str).map(String::from))
        .collect()
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

/// What the chat's watch feeder is told: a request's live view starts, or the request
/// settled (its queued tokens are fed, then its view leaves; the ack says so).
enum Ctl {
    Open(String, Arc<dyn crate::provider::Live>),
    Close(String, tokio::sync::oneshot::Sender<()>),
}

/// A chat's one watch on its provider.
struct ChatWatch {
    provider: AgentId,
    reader: AgentId,
    ctl: tokio::sync::mpsc::UnboundedSender<Ctl>,
}

impl ChatWatch {
    /// The watch ends: unwatched, its inbox gone, its feeder stops.
    async fn release(self, kernel: &Kernel) {
        kernel.unwatch(&self.provider, &self.reader).await;
        kernel.remove_inbox(&self.reader);
    }
}

/// Every chat's watch on its provider, by chat.
fn chat_watches() -> &'static Mutex<std::collections::HashMap<AgentId, ChatWatch>> {
    static W: std::sync::OnceLock<Mutex<std::collections::HashMap<AgentId, ChatWatch>>> =
        std::sync::OnceLock::new();
    W.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// The chat is gone: its watch on its provider ends ([`crate::verbs::on_delete`]).
pub(crate) async fn forget_watch(chat: &AgentId, kernel: &Kernel) {
    let w = chat_watches().lock().expect("watches poisoned").remove(chat);
    if let Some(w) = w {
        w.release(kernel).await;
    }
}

/// One item of the chat's watch: a token goes to the live view its `rid` names.
async fn feed(lives: &std::collections::HashMap<String, Arc<dyn crate::provider::Live>>, payload: &Value) {
    let Some(rid) = payload.get("rid").and_then(Value::as_str) else { return };
    let Some(live) = lives.get(rid) else { return };
    if let Some(t) = token_of(payload, rid) {
        live.token(&t).await;
    }
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

    /// THE CHAT'S WATCH on its provider (#795): ONE reader per chat, made at its first
    /// request and again only when the chat's provider changed — never one per pass.
    /// Its feeder hands each `provider_token` to the live view of the request its `rid`
    /// names; it holds no lock and wakes nothing. The feeder's control channel.
    async fn chat_watch(&self) -> Result<tokio::sync::mpsc::UnboundedSender<Ctl>, ProviderError> {
        if let Some(w) = chat_watches()
            .lock()
            .expect("watches poisoned")
            .get(&self.chat)
            .filter(|w| w.provider == self.provider && !w.ctl.is_closed())
        {
            return Ok(w.ctl.clone());
        }
        let old = chat_watches().lock().expect("watches poisoned").remove(&self.chat);
        if let Some(old) = old {
            old.release(&self.kernel).await;
        }
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let (reader, mut inbox) = self.reader(n).await?;
        let (ctl, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ctl>();
        // It carries the chat's identity (no `ps` row): the live view's events are the
        // chat's own, and an emit outside any identity is dropped.
        fantastic_kernel::spawn_inherit(async move {
            let mut lives: std::collections::HashMap<String, Arc<dyn crate::provider::Live>> =
                std::collections::HashMap::new();
            loop {
                tokio::select! {
                    biased;
                    c = rx.recv() => match c {
                        Some(Ctl::Open(rid, live)) => {
                            lives.insert(rid, live);
                        }
                        Some(Ctl::Close(rid, ack)) => {
                            while let Some(it) = inbox.try_recv() {
                                feed(&lives, &it.payload).await;
                            }
                            lives.remove(&rid);
                            let _ = ack.send(());
                        }
                        None => break,
                    },
                    item = inbox.recv() => match item {
                        Some(it) => feed(&lives, &it.payload).await,
                        None => break,
                    },
                }
            }
        });
        chat_watches().lock().expect("watches poisoned").insert(
            self.chat.clone(),
            ChatWatch {
                provider: self.provider.clone(),
                reader,
                ctl: ctl.clone(),
            },
        );
        Ok(ctl)
    }

    /// THE CHAT'S REQUEST AS A MESSAGE (#795): the request's images go first (steps,
    /// [`send_images`]), then the `complete` call goes out through [`Kernel::call`], and
    /// `launched` gets its pid. The provider's tokens feed `live` from a watcher of its
    /// stream that holds no lock and wakes nothing; the settle runs `done` with the WHOLE
    /// answer the reply carries (`text`), so a token the live view dropped never changes
    /// what is read. A call refused at launch, or an image not taken, runs `done` with
    /// why, and `launched` never runs.
    pub(crate) async fn begin(
        &self,
        messages: Vec<Value>,
        max_tokens: Option<i64>,
        deadline: (std::time::Duration, f64),
        live: Arc<dyn crate::provider::Live>,
        done: crate::provider::Done,
        launched: crate::provider::Launched,
    ) {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let rid = format!("{}~{n}", self.chat.as_str());
        let refs = image_refs(&messages);
        let me = self.clone_handle();
        send_images(
            Arc::clone(&self.kernel),
            self.chat.clone(),
            self.provider.clone(),
            refs,
            rid.clone(),
            (me, messages, max_tokens, deadline, live, done, launched, rid),
            |(me, messages, max_tokens, deadline, live, done, launched, rid), sent| async move {
                match sent {
                    Ok(()) => me.request(messages, max_tokens, deadline, live, done, launched, rid).await,
                    Err(e) => done(Err(e)).await,
                }
            },
        )
        .await;
    }

    /// The same provider handle (its report and model cells shared).
    fn clone_handle(&self) -> Self {
        Self {
            kernel: Arc::clone(&self.kernel),
            chat: self.chat.clone(),
            provider: self.provider.clone(),
            report: Arc::clone(&self.report),
            model: Arc::clone(&self.model),
            chat_model: self.chat_model.clone(),
            knobs: self.knobs.clone(),
        }
    }

    /// The `complete` of a request whose images were taken.
    #[allow(clippy::too_many_arguments)]
    async fn request(
        &self,
        messages: Vec<Value>,
        max_tokens: Option<i64>,
        deadline: (std::time::Duration, f64),
        live: Arc<dyn crate::provider::Live>,
        done: crate::provider::Done,
        launched: crate::provider::Launched,
        rid: String,
    ) {
        // THE CHAT'S ONE WATCH on its provider: this request's tokens feed `live`
        // until its settle says it ended
        let watch = match self.chat_watch().await {
            Ok(w) => w,
            Err(e) => return done(Err(e)).await,
        };
        let _ = watch.send(Ctl::Open(rid.clone(), live));
        let feed_rid = rid.clone();
        let done = Arc::new(Mutex::new(Some(done)));
        let (within, ceiling) = deadline;
        let item = Item::new(self.provider.clone(), Value::Object(self.payload(&messages, max_tokens, &rid)))
            .with_timeout(within.as_secs_f64());
        let (report, model, provider) = (Arc::clone(&self.report), Arc::clone(&self.model), self.provider.clone());
        let then_done = Arc::clone(&done);
        let out = self
            .kernel
            .call(item, move |o| async move {
                // what is still queued for this request is fed first
                let (tx, rx) = tokio::sync::oneshot::channel();
                if watch.send(Ctl::Close(feed_rid, tx)).is_ok() {
                    let _ = rx.await;
                }
                let read = answer_of(&o, &report, &model, &provider, ceiling);
                let done = then_done.lock().expect("done poisoned").take();
                if let Some(done) = done {
                    done(read).await;
                }
            })
            .await;
        if out.status == Status::Accepted {
            if let Some(pid) = out.pid {
                launched(pid);
            }
            return;
        }
        if let Some(w) = chat_watches().lock().expect("watches poisoned").get(&self.chat) {
            let (tx, _) = tokio::sync::oneshot::channel();
            let _ = w.ctl.send(Ctl::Close(rid.clone(), tx));
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

    /// A provider agent is asked by `complete` calls only ([`RemoteProvider::begin`],
    /// through [`crate::provider::begin`]): no stream is read here.
    async fn chat(&self, _messages: &[Value], _max_tokens: Option<i64>) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::raised(format!(
            "ai_chat: provider `{}` is an agent: its requests are `complete` calls",
            self.provider.as_str()
        )))
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
/// rest are the provider verbs above, each settled by its steps (#795). `keys`: how
/// the backend reads its key (`None`: it takes none). `prepare`: what it reads from
/// its API before a pass. `build` makes the backend's provider from a record and the
/// key read; `live` lists the models the API serves when the record names none.
#[allow(clippy::too_many_arguments)]
pub async fn handle<B, L, Fut, R>(
    agent_id: &AgentId,
    call: crate::declared::provider::Call,
    kernel: &Arc<Kernel>,
    keys: Option<Keys>,
    prepare: Option<Prepare>,
    build: B,
    live: Option<L>,
    reflect: R,
) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError>
where
    B: FnOnce(Map<String, Value>, Arc<dyn crate::credentials::KeySource>) -> Arc<dyn Provider> + Send + 'static,
    L: FnOnce(Arc<dyn crate::credentials::KeySource>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Vec<String>, String>> + Send + 'static,
    R: FnOnce() -> Value,
{
    use crate::declared::provider::Call;
    let reply = match call {
        Call::Reflect(_) => reflect(),
        Call::Boot(_) => crate::verbs::boot(agent_id, kernel).await,
        Call::Complete(a) => {
            complete_verb(agent_id, a, kernel, keys, prepare, build, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        Call::Models(_) => {
            models_verb(agent_id, kernel, keys, live, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        Call::SetDefaultModel(a) => {
            set_default_verb(agent_id, &a, kernel, crate::steps::deferred(kernel)).await;
            Value::Null
        }
    };
    Ok((!reply.is_null()).then_some(reply))
}
