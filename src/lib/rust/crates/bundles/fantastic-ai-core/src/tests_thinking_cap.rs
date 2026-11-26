//! THE THINKING CAP PER PASS (#782): every chat pass carries a `thinking_token_budget`
//! (the record's, else the default, fitted below `max_tokens`; `null` is none), a server
//! that cannot enforce it is cut by the harness on the stream, and the chat re-asks once
//! for the answer.

use super::*;
use crate::constants::DEFAULT_THINKING_BUDGET;
use crate::think::{ThinkMeter, THINK_CLOSE, THINK_OPEN};
use futures_util::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the thinker answers after its thinking: a call, so the answer stands (#795).
const ANSWER: &str = "the answer <call name=\"done\"></call>";

fn knobs(meta: Value) -> Map<String, Value> {
    crate::record::knobs_of(meta.as_object().unwrap())
}

#[test]
fn a_chat_that_names_no_budget_carries_the_default() {
    assert_eq!(knobs(json!({}))["thinking_token_budget"], json!(DEFAULT_THINKING_BUDGET));
    assert_eq!(DEFAULT_THINKING_BUDGET, 8192);
    assert_eq!(knobs(json!({"max_tokens": 65536}))["thinking_token_budget"], json!(8192));
}

#[test]
fn a_chats_own_budget_overrides_the_default_and_null_is_no_cap() {
    assert_eq!(knobs(json!({"thinking_token_budget": 2000}))["thinking_token_budget"], json!(2000));
    assert_eq!(knobs(json!({"thinking_token_budget": 30000, "max_tokens": 65536}))["thinking_token_budget"], json!(30000));
    let none = knobs(json!({"thinking_token_budget": null, "temperature": 0.3}));
    assert!(none.get("thinking_token_budget").is_none(), "{none:?}");
    assert_eq!(none["temperature"], json!(0.3));
}

/// The default is fitted below a chat's own `max_tokens`, so it can fire; a budget the
/// chat names that is not below its `max_tokens` stays refused at the write.
#[test]
fn max_tokens_stays_above_the_budget() {
    for m in [4000_i64, 8192, 16384, 20000] {
        let b = knobs(json!({"max_tokens": m}))["thinking_token_budget"].as_i64().unwrap();
        assert!(b < m, "max_tokens={m}: budget {b}");
        assert_eq!(b, DEFAULT_THINKING_BUDGET.min(m / 2));
    }
    assert!(knobs(json!({"max_tokens": 1})).get("thinking_token_budget").is_none());
    let o = |v: Value| v.as_object().cloned().unwrap();
    assert!(crate::record::knobs_refusal(&o(json!({"max_tokens": 8192})), &o(json!({"thinking_token_budget": 8192}))).is_some());
    assert!(crate::record::knobs_refusal(&o(json!({"max_tokens": 8192})), &o(json!({}))).is_none());
}

#[test]
fn the_meter_counts_only_thinking_and_reads_a_split_marker() {
    // 2 tokens = 8 characters of thinking
    let mut m = ThinkMeter::new(2);
    assert!(!m.feed("answer text long before any thinking at all"));
    assert!(!m.feed("<thi"));
    assert!(!m.feed("nk>abcd"));
    assert!(m.is_open());
    assert!(!m.feed("efgh"), "exactly at the cap");
    assert!(!m.feed("</th"));
    assert!(!m.feed("ink> the answer goes on and on and on and on"), "the answer is no thinking");
    assert!(!m.is_open());
    assert!(m.feed("<think>more thinking"), "a second span adds to the first");
    let mut m = ThinkMeter::new(1);
    assert!(m.feed(&format!("{THINK_OPEN}é is not a byte, it is one character")));
}

// ── a provider that thinks without bound ──────────────────────────────────────

/// A pass: `<think>`, then `n` chunks of one token of thinking each, then the answer;
/// `pulled` counts the chunks its streams were read for.
struct Thinker {
    enforces: bool,
    answer: String,
    n: usize,
    pulled: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Thinker {
    async fn chat(&self, _m: &[Value], _x: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let mut toks = vec![THINK_OPEN.to_string()];
        toks.extend(std::iter::repeat_n("abcd".to_string(), self.n));
        toks.push(THINK_CLOSE.to_string());
        toks.push(self.answer.clone());
        let pulled = Arc::clone(&self.pulled);
        Ok(Box::pin(futures_util::stream::iter(toks).map(move |t| {
            pulled.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderEvent::Token(t))
        })))
    }
    fn model(&self) -> String {
        "think-1".into()
    }
    fn enforces_thinking_budget(&self) -> bool {
        self.enforces
    }
}

/// The thinker's provider agent: `enforces` and `runaway` (how many passes think past any
/// cap before one answers) come from its record.
struct ThinkerProvider;

/// The passes each thinker agent served, by id.
fn served() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static SERVED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    SERVED.get_or_init(Default::default)
}

fn served_by(id: &str) -> usize {
    served().lock().unwrap().get(id).copied().unwrap_or(0)
}

#[async_trait]
impl Handler for ThinkerProvider {
    type Call = provider_verbs::Call;
    fn name(&self) -> &str {
        "thinker"
    }
    fn man(&self) -> &'static str {
        "A provider stand-in that thinks long before it answers."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        provider_verbs::VERBS
    }
    fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        provider_verbs::dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &crate::tests_decl::any::RECORD
    }
    async fn handle(&self, id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let build = |m: Map<String, Value>| -> Arc<dyn Provider> {
            let runaway = m.get("runaway").and_then(Value::as_u64).map_or(0, |n| usize::try_from(n).unwrap());
            let nth = {
                let mut all = served().lock().unwrap();
                let n = all.entry(id.as_str().to_string()).or_default();
                *n += 1;
                *n - 1
            };
            Arc::new(Thinker {
                enforces: m.get("enforces").and_then(Value::as_bool).unwrap_or(false),
                answer: ANSWER.into(),
                n: if nth < runaway { 100 } else { 3 },
                pulled: Arc::default(),
            })
        };
        use provider_verbs::Call;
        Ok(Some(match call {
            Call::Complete(a) => crate::remote_provider::complete_verb(id, &a, kernel, build).await,
            _ => json!({"id": id.as_str()}),
        }))
    }
}

/// THE SCRIPTED STREAM THAT THINKS PAST THE BUDGET, through `complete`: the pass stops
/// being read at the cap, its thinking span is closed and its stop reason says why; a
/// server that enforces the budget itself is read whole; a side call is bounded by its
/// own `max_tokens`.
#[tokio::test]
async fn complete_cuts_a_thinking_past_the_budget() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| k.bundles.register("thinker.tools", ThinkerProvider)).await;
    let run = |args: Value, enforces: bool| {
        let kernel = Arc::clone(&kernel);
        async move {
            let a: crate::declared::Complete = serde_json::from_value(args).unwrap();
            let pulled = Arc::new(AtomicUsize::new(0));
            let p: Arc<dyn Provider> =
                Arc::new(Thinker { enforces, answer: "ok".into(), n: 100, pulled: Arc::clone(&pulled) });
            let r = crate::remote_provider::complete(&AgentId::from("p"), &a, &kernel, p).await;
            (r, pulled.load(Ordering::SeqCst))
        }
    };
    let msgs = json!([{"role": "user", "content": "hi"}]);
    let (r, pulled) = run(json!({"messages": msgs, "rid": "r1", "knobs": {"thinking_token_budget": 10}}), false).await;
    assert_eq!(r["stop_reason"], json!(crate::think::THINKING_CUT), "{r}");
    assert!(pulled <= 13, "the request is dropped at the cap: {pulled} chunks read");
    let (r, pulled) = run(json!({"messages": msgs, "rid": "r2", "knobs": {"thinking_token_budget": 10}}), true).await;
    assert_eq!(r["stop_reason"], Value::Null, "{r}");
    assert_eq!(pulled, 103, "an enforcing server's stream is read whole");
    let (r, pulled) = run(json!({"messages": msgs, "rid": "r3", "max_tokens": 2000, "knobs": {"thinking_token_budget": 10}}), false).await;
    assert_eq!((r["stop_reason"].clone(), pulled), (Value::Null, 103), "a side call: {r}");
    let (r, pulled) = run(json!({"messages": msgs, "rid": "r4"}), false).await;
    assert_eq!((r["stop_reason"].clone(), pulled), (Value::Null, 103), "no cap: {r}");
}

/// An ai_chat on the thinker agent `id`: `extra` lays over its record. The provider's
/// record carries `provider` (`enforces`, `runaway`).
async fn chat_on_thinker(tmp: &TempDir, id: &str, extra: Value, provider: Value) -> (Arc<Kernel>, AgentId) {
    // a chat id of its own: ai-core keeps per-agent state process-wide
    let chat = format!("chat_{id}");
    let kernel = base_kernel(tmp, |k| {
        k.bundles.register("thinker.tools", ThinkerProvider);
        k.bundles.register(crate::chat_bundle::HANDLER_MODULE, crate::chat_bundle::AiChatBundle);
    })
    .await;
    let mut p = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "thinker.tools", "id": id});
    for (k, v) in provider.as_object().unwrap() {
        p[k] = v.clone();
    }
    let made = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), p)]).await.remove(0).result;
    assert_eq!(made["id"], id, "{made}");
    let memory = fantastic_ram_store::test_support::memory_meta(
        &kernel,
        &chat,
        json!({"cli": {"call": ["*"]}, id: {"call": ["complete", "watch"]}}),
    )
    .await;
    let mut create = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
                            "id": chat, "provider_id": id, "context_window": 32768});
    for (k, v) in memory {
        create[k] = v;
    }
    for (k, v) in extra.as_object().unwrap() {
        create[k] = v.clone();
    }
    let made = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), create)]).await.remove(0).result;
    assert_eq!(made["id"], json!(chat), "{made}");
    (kernel, AgentId::from(chat.as_str()))
}

async fn retries(kernel: &Arc<Kernel>, chat: &AgentId) -> Vec<Value> {
    traces(kernel, chat, "retry").await
}

/// The chat's trace rows of `event`.
async fn traces(kernel: &Arc<Kernel>, chat: &AgentId, event: &str) -> Vec<Value> {
    let chat = chat.clone();
    let all = fantastic_kernel::test_support::as_agent(
        chat.clone(),
        crate::memory::store_call(&chat, kernel, json!({"type": "read", "kinds": ["trace"]})),
    )
    .await
    .expect("store read");
    all["rows"].as_array().cloned().unwrap_or_default().into_iter().filter(|r| r["event"] == json!(event)).collect()
}

/// THE HARNESS CUT, end to end: the chat's pass thinks past its cap on a server that
/// cannot enforce it; the harness ends the thinking, the chat re-asks once with the
/// "answer now" note, logs a `retry`, and the second pass's answer is the turn's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_thinking_is_re_asked_once_and_answers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat) = chat_on_thinker(&tmp, "thinker_once", json!({"thinking_token_budget": 10}), json!({"runaway": 1})).await;
    let r = kernel
        .operator_send(vec![Item::new(chat.clone(), json!({"type": "send", "text": "hi"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["response"], json!(ANSWER), "{r}");
    let retries = retries(&kernel, &chat).await;
    assert_eq!(retries.len(), 1, "{retries:?}");
    let note = crate::model_format::NOTICE_THINKING_CUT.replace("{budget}", "10");
    assert_eq!(retries[0]["error"], json!(note), "{retries:?}");
    assert_eq!(retries[0]["attempt"], json!(1));
}

/// A second cut in the same turn stops it: the re-ask is once, never the call retries'.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_cut_stops_the_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat) = chat_on_thinker(&tmp, "thinker_twice", json!({"thinking_token_budget": 10}), json!({"runaway": 99})).await;
    let r = kernel
        .operator_send(vec![Item::new(chat.clone(), json!({"type": "send", "text": "hi"}))])
        .await
        .remove(0)
        .result;
    assert_ne!(r["response"], json!(ANSWER), "{r}");
    assert_eq!(served_by("thinker_twice"), 2, "two passes, then the stop: {r}");
    assert_eq!(retries(&kernel, &chat).await.len(), 2);
    let stops = traces(&kernel, &chat, "stop").await;
    let why = crate::model_format::STOP_REASON_THINKING.replace("{budget}", "10");
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0]["reason"], json!(why), "{stops:?}");
}

/// A server that enforces the budget is never cut: the pass streams whole and answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enforcing_server_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat) = chat_on_thinker(&tmp, "thinker_enforced", json!({"thinking_token_budget": 10}), json!({"runaway": 1, "enforces": true})).await;
    let r = kernel
        .operator_send(vec![Item::new(chat.clone(), json!({"type": "send", "text": "hi"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["response"], json!(ANSWER), "{r}");
    assert!(retries(&kernel, &chat).await.is_empty());
}
