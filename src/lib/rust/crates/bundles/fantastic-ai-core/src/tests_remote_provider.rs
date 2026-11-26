//! The split: a chat completes through a provider agent it knows only by id.
use super::*;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use std::time::Duration;

/// A provider agent: `complete` streams the words of its record's `say`.
struct Echo;

struct Words(Vec<String>, crate::provider::ProviderReport);

#[async_trait]
impl Provider for Words {
    async fn chat(&self, _m: &[Value], _x: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let toks: Vec<Result<ProviderEvent, ProviderError>> =
            self.0.iter().map(|w| Ok(ProviderEvent::Token(w.clone()))).collect();
        Ok(Box::pin(futures_util::stream::iter(toks)))
    }
    fn model(&self) -> String {
        "echo-1".into()
    }
    fn report(&self) -> crate::provider::ProviderReport {
        self.1.clone()
    }
}

mod provider_verbs {
    use crate::declared::{Complete, Models, SetDefaultModel};
    fantastic_kernel::verbs! {
        "complete" => Complete,
        "models" => Models,
        "set_default_model" => SetDefaultModel,
    }
}

#[async_trait]
impl Handler for Echo {
    type Call = provider_verbs::Call;
    fn name(&self) -> &str {
        "echo"
    }
    fn man(&self) -> &'static str {
        "A provider stand-in that streams a fixed answer."
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
        // the record's `cache_read` / `cache_write` are what the endpoint reports
        let build = |m: Map<String, Value>| -> Arc<dyn Provider> {
            let report = crate::provider::ProviderReport {
                prompt_tokens: Some(100),
                completion_tokens: Some(5),
                cache_read: m.get("cache_read").and_then(Value::as_i64),
                cache_write: m.get("cache_write").and_then(Value::as_i64),
                stop_reason: None,
            };
            Arc::new(Words(vec!["hel".into(), "lo".into()], report))
        };
        use provider_verbs::Call;
        Ok(Some(match call {
            Call::Reflect(_) => json!({"id": id.as_str()}),
            Call::Complete(a) => crate::remote_provider::complete_verb(id, &a, kernel, build).await,
            Call::Models(_) => crate::remote_provider::models_verb(id, kernel, NO_LIVE).await,
            Call::SetDefaultModel(a) => crate::remote_provider::set_default_verb(id, &a, kernel).await,
        }))
    }
}

fn agent(k: &Arc<Kernel>, root: &Arc<fantastic_kernel::Agent>, id: &str, meta: Value, dir: &std::path::Path) {
    let a = fantastic_kernel::Agent::new(
        AgentId::from(id),
        Some("echo.tools".into()),
        Some(root.id.clone()),
        meta.as_object().cloned().unwrap_or_default(),
        dir.join(id),
        false,
    );
    fantastic_kernel::test_support::register(k, a);
}

#[tokio::test]
async fn a_chat_completes_through_its_provider_agent_by_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    k.bundles.register("echo.tools", Echo);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    agent(&k, &root, "p", json!({}), tmp.path());
    agent(&k, &root, "chat", json!({"can": {"p": {"call": ["complete", "watch"]}}}), tmp.path());
    let rp = RemoteProvider::new(&k, &AgentId::from("chat"), "p");
    let text: Vec<String> = fantastic_kernel::test_support::as_agent(AgentId::from("chat").clone(), async {
        let s = rp.chat(&[json!({"role": "user", "content": "hi"})], None).await.unwrap();
        s.map(|e| match e.unwrap() {
            ProviderEvent::Token(t) => t,
        })
        .collect()
        .await
    })
    .await;
    assert_eq!(text.concat(), "hello", "the provider's tokens, in order");
    assert_eq!(rp.model(), "echo-1", "the model the provider reported");
}

/// A provider stand-in whose reported model names the knobs its pass was built with.
struct Knobbed;

#[async_trait]
impl Handler for Knobbed {
    type Call = provider_verbs::Call;
    fn name(&self) -> &str {
        "knobbed"
    }
    fn man(&self) -> &'static str {
        "A provider stand-in that reports the knobs of its pass."
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
        struct Named(String);
        #[async_trait]
        impl Provider for Named {
            async fn chat(&self, _m: &[Value], _x: Option<i64>) -> Result<ProviderStream, ProviderError> {
                Ok(Box::pin(futures_util::stream::iter(vec![Ok(ProviderEvent::Token("ok".into()))])))
            }
            fn model(&self) -> String {
                self.0.clone()
            }
        }
        let build = |m: Map<String, Value>| -> Arc<dyn Provider> {
            let knob = |k: &str| m.get(k).map_or_else(|| "-".to_string(), ToString::to_string);
            Arc::new(Named(format!("t={} m={} stop={}", knob("temperature"), knob("max_tokens"), knob("stop"))))
        };
        use provider_verbs::Call;
        Ok(Some(match call {
            Call::Complete(a) => crate::remote_provider::complete_verb(id, &a, kernel, build).await,
            _ => json!({"id": id.as_str()}),
        }))
    }
}

/// ONE PROVIDER, MANY CHATS (#632): each chat's request carries its own knobs, laid over the
/// provider's record for that pass only; a chat that names none sends none, and the
/// provider's record never stands in for one.
#[tokio::test]
async fn two_chats_on_one_provider_each_send_their_own_knobs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    k.bundles.register("knobbed.tools", Knobbed);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    let made = |id: &str, handler: &str, meta: Value| {
        fantastic_kernel::test_support::register(&k, fantastic_kernel::Agent::new(
            AgentId::from(id),
            Some(handler.into()),
            Some(root.id.clone()),
            meta.as_object().cloned().unwrap_or_default(),
            tmp.path().join(id),
            false,
        ));
    };
    made("p", "knobbed.tools", json!({"temperature": 9.9}));
    let line = json!({"p": {"call": ["complete", "watch"]}});
    made("a", "echo.tools", json!({"can": line, "temperature": 0.1, "max_tokens": 64}));
    made("b", "echo.tools", json!({"can": line, "temperature": 0.7, "stop": ["END"]}));
    made("c", "echo.tools", json!({"can": line}));
    let mut seen = Vec::new();
    for chat in ["a", "b", "c"] {
        let rp = RemoteProvider::new(&k, &AgentId::from(chat), "p");
        fantastic_kernel::test_support::as_agent(AgentId::from(chat).clone(), async {
            let s = rp.chat(&[json!({"role": "user", "content": "hi"})], None).await.unwrap();
            let _: Vec<_> = s.collect().await;
        })
        .await;
        seen.push(rp.model());
    }
    assert_eq!(seen, ["t=0.1 m=64 stop=-", "t=0.7 m=- stop=[\"END\"]", "t=- m=- stop=-"]);
}

#[test]
fn a_chats_budget_not_below_its_max_tokens_is_refused_on_the_merged_record() {
    let o = |v: Value| v.as_object().cloned().unwrap();
    let why = crate::record::knobs_refusal(&o(json!({"thinking_token_budget": 8000})), &o(json!({"max_tokens": 8000})));
    assert!(why.is_some_and(|w| w.contains("not below max_tokens=8000")));
    assert!(crate::record::knobs_refusal(&o(json!({"max_tokens": 9000})), &o(json!({"thinking_token_budget": 8000}))).is_none());
    // the knobs' shapes are the chat record's: an effort vLLM refuses is refused at the write
    let r = fantastic_kernel::verbs::check_record(
        "update_agent",
        "ai_chat",
        &o(json!({"reasoning_effort": "high"})),
        &fantastic_kernel::declared::record::RECORD,
        &crate::chat_bundle::RECORD,
    );
    assert!(r.is_err_and(|e| e.to_string().contains("reasoning_effort")));
}

#[tokio::test]
async fn a_chat_without_the_grant_is_refused_by_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    k.bundles.register("echo.tools", Echo);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    agent(&k, &root, "p", json!({}), tmp.path());
    agent(&k, &root, "chat", json!({}), tmp.path());
    let rp = RemoteProvider::new(&k, &AgentId::from("chat"), "p");
    let r = fantastic_kernel::test_support::as_agent(AgentId::from("chat").clone(), async {
        match rp.chat(&[json!({"role": "user", "content": "hi"})], None).await {
            Err(e) => e.message,
            Ok(_) => String::new(),
        }
    })
    .await;
    assert!(r.contains("cannot watch provider `p`"), "{r}");
}

/// A kernel with a `ram_store` and a provider `p` (`with_store`: granted it).
async fn kernel_with_provider(tmp: &tempfile::TempDir, with_store: bool) -> Arc<Kernel> {
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    k.bundles.register("echo.tools", Echo);
    k.bundles.register(fantastic_ram_store::HANDLER_MODULE, fantastic_ram_store::RamStoreBundle::default());
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    let meta = if with_store {
        Value::Object(fantastic_ram_store::test_support::memory_meta(&k, "p", json!({})).await)
    } else {
        json!({})
    };
    agent(&k, &root, "p", meta, tmp.path());
    k
}

#[tokio::test]
async fn a_providers_usage_is_kept_in_its_store_and_the_newest_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel_with_provider(&tmp, true).await;
    let p = AgentId::from("p");
    fantastic_kernel::test_support::as_agent(p.clone(), async {
        assert_eq!(usage_of(&p, &k).await, None, "nothing kept yet");
        let first = json!({"total": {"requests": 1}, "by_model": {}, "by_agent": {}});
        set_usage(&p, &k, &first).await.unwrap();
        assert_eq!(usage_of(&p, &k).await, Some(first));
        let second = json!({"total": {"requests": 2}, "by_model": {}, "by_agent": {}});
        set_usage(&p, &k, &second).await.unwrap();
        assert_eq!(usage_of(&p, &k).await, Some(second), "the newest row wins");
    })
    .await;
}

#[tokio::test]
async fn a_provider_without_a_store_keeps_no_usage_and_does_not_fail() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel_with_provider(&tmp, false).await;
    let p = AgentId::from("p");
    assert_eq!(usage_of(&p, &k).await, None);
    set_usage(&p, &k, &json!({"total": {}})).await.unwrap();
    assert_eq!(usage_of(&p, &k).await, None);
}

#[tokio::test]
async fn a_remote_providers_cache_counts_reach_the_chats_report_and_absent_ones_stay_absent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    k.bundles.register("echo.tools", Echo);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    agent(
        &k,
        &root,
        "p",
        json!({"cache_read": 600, "cache_write": 300}),
        tmp.path(),
    );
    agent(&k, &root, "q", json!({}), tmp.path());
    agent(
        &k,
        &root,
        "chat",
        json!({"can": {"p": {"call": ["complete", "watch"]}, "q": {"call": ["complete", "watch"]}}}),
        tmp.path(),
    );
    for (provider, cache) in [("p", (Some(600), Some(300))), ("q", (None, None))] {
        let rp = RemoteProvider::new(&k, &AgentId::from("chat"), provider);
        let _: Vec<_> =
            fantastic_kernel::test_support::as_agent(AgentId::from("chat").clone(), async {
                let s = rp
                    .chat(&[json!({"role": "user", "content": "hi"})], None)
                    .await
                    .unwrap();
                s.collect().await
            })
            .await;
        let r = rp.report();
        assert_eq!((r.cache_read, r.cache_write), cache, "{provider}");
        assert_eq!(
            (r.prompt_tokens, r.completion_tokens),
            (Some(100), Some(5)),
            "{provider}"
        );
    }
}

/// A FAILED PASS KEEPS ITS CLASS ACROSS `complete`: the provider's reply names it in
/// `reason`, the chat's side reads it back — a refused key stays permanent, the link
/// stays transient, a turn's own failure stays a turn's.
#[test]
fn a_failed_pass_keeps_its_class_across_complete() {
    use crate::provider::ErrorClass;
    use fantastic_kernel::refusal::{DISABLED, UNAUTHORIZED};
    for e in [
        ProviderError::raised("vllm: answered HTTP 401").for_status(401),
        ProviderError::raised("vllm: answered HTTP 404").for_status(404),
        ProviderError::raised("vllm: answered HTTP 503").for_status(503),
        ProviderError::raised("vllm: answered HTTP 400").for_status(400),
        ProviderError::permanent("fm: unavailable", DISABLED),
        crate::credentials::CredentialError("creds/vllm: no agent".into()).into(),
    ] {
        let reply = failed("c~1", &e);
        let back = settle(&reply, &Mutex::default(), &Mutex::default()).expect_err("a failed pass");
        assert_eq!(back.class, e.class, "{reply}");
        assert_eq!(back.message, e.message);
    }
    assert_eq!(
        failed("c~1", &ProviderError::raised("x").for_status(401))["reason"],
        json!(UNAUTHORIZED)
    );
    assert_eq!(
        settle(&json!({"error": "x"}), &Mutex::default(), &Mutex::default())
            .expect_err("failed")
            .class,
        ErrorClass::Turn
    );
}

/// A live view that takes its time over each token: under it the reader's drop-oldest
/// inbox overflows.
struct SlowLive(std::sync::Mutex<String>);

#[async_trait]
impl crate::provider::Live for SlowLive {
    async fn token(&self, t: &str) {
        tokio::time::sleep(Duration::from_millis(2)).await;
        self.0.lock().unwrap().push_str(t);
    }
}

/// A provider agent that streams `n` tokens of its record's `say`, then its `tail`.
struct Many;

#[async_trait]
impl Handler for Many {
    type Call = provider_verbs::Call;
    fn name(&self) -> &str {
        "many"
    }
    fn man(&self) -> &'static str {
        "A provider stand-in that streams many tokens."
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
        let build = |_m: Map<String, Value>| -> Arc<dyn Provider> {
            let mut toks: Vec<String> = (0..400).map(|i| format!("w{i} ")).collect();
            toks.push("<call name=\"done\"></call>".into());
            Arc::new(Words(toks, crate::provider::ProviderReport::default()))
        };
        use provider_verbs::Call;
        Ok(Some(match call {
            Call::Complete(a) => crate::remote_provider::complete_verb(id, &a, kernel, build).await,
            _ => json!({"id": id.as_str()}),
        }))
    }
}

/// THE ROUTE READS THE SETTLED TEXT, NOT THE STREAM (#795): the provider's `complete`
/// reply carries the whole answer; a live view so slow that its drop-oldest inbox
/// overflowed shows less, and the answer read from the settle is still whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_reads_the_settled_text_not_the_stream() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    // a tiny inbox: the live view's watch drops the oldest tokens under a slow reader
    k.inbox_bound = 4;
    k.bundles.register("many.tools", Many);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    let a = fantastic_kernel::Agent::new(AgentId::from("p"), Some("many.tools".into()), Some(root.id.clone()), Map::new(), tmp.path().join("p"), false);
    fantastic_kernel::test_support::register(&k, a);
    agent(&k, &root, "chat", json!({"can": {"p": {"call": ["complete", "watch"]}}}), tmp.path());
    let rp = RemoteProvider::new(&k, &AgentId::from("chat"), "p");
    let live = Arc::new(SlowLive(std::sync::Mutex::new(String::new())));
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, ProviderError>>();
    let done: crate::provider::Done = Box::new(move |r| {
        Box::pin(async move {
            let _ = tx.send(r);
        })
    });
    fantastic_kernel::test_support::as_agent(AgentId::from("chat"), async {
        rp.begin(
            vec![json!({"role": "user", "content": "hi"})],
            None,
            (Duration::from_secs(30), 30.0),
            Arc::clone(&live) as Arc<dyn crate::provider::Live>,
            done,
        )
        .await
    })
    .await
    .expect("the request is out");
    let whole = tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .expect("the request settled")
        .expect("done ran")
        .expect("an answer");
    let mut want: String = (0..400).map(|i| format!("w{i} ")).collect();
    want.push_str("<call name=\"done\"></call>");
    assert_eq!(whole, want, "the settle's text is the whole answer");
    let shown = live.0.lock().unwrap().clone();
    assert!(shown.len() < want.len(), "the live view dropped tokens: {} of {}", shown.len(), want.len());
}

/// THE CHAT'S ONE WATCH (#795): a chat watches its provider once, at its first request,
/// and keeps that watch across requests — never one reader per pass. Each request's
/// tokens still reach its own live view, and the watch ends with the chat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chat_keeps_one_watch_on_its_provider_across_requests() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    k.bundles.register("echo.tools", Echo);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    agent(&k, &root, "p1w", json!({}), tmp.path());
    agent(&k, &root, "chat1w", json!({"can": {"p1w": {"call": ["complete", "watch"]}}}), tmp.path());
    let chat = AgentId::from("chat1w");
    let provider = AgentId::from("p1w");
    let mut first: Option<AgentId> = None;
    for round in 0..2 {
        let rp = RemoteProvider::new(&k, &chat, "p1w");
        let live = Arc::new(SlowLive(std::sync::Mutex::new(String::new())));
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, ProviderError>>();
        let done: crate::provider::Done = Box::new(move |r| {
            Box::pin(async move {
                let _ = tx.send(r);
            })
        });
        fantastic_kernel::test_support::as_agent(chat.clone(), async {
            rp.begin(
                vec![json!({"role": "user", "content": "hi"})],
                None,
                (Duration::from_secs(30), 30.0),
                Arc::clone(&live) as Arc<dyn crate::provider::Live>,
                done,
            )
            .await
        })
        .await
        .expect("the request is out");
        let whole = tokio::time::timeout(Duration::from_secs(20), rx)
            .await
            .expect("the request settled")
            .expect("done ran")
            .expect("an answer");
        assert_eq!(whole, "hello", "round {round}");
        assert_eq!(live.0.lock().unwrap().as_str(), "hello", "round {round}: its own tokens");
        assert!(k.has_watchers(&provider).await, "round {round}: the chat still watches");
        let reader = chat_watches().lock().unwrap().get(&chat).map(|w| w.reader.clone());
        assert!(reader.is_some(), "round {round}: the watch is the chat's");
        match &first {
            None => first = reader,
            Some(f) => assert_eq!(reader.as_ref(), Some(f), "one reader across requests"),
        }
    }
    forget_watch(&chat, &k).await;
    assert!(!k.has_watchers(&provider).await, "the watch ends with the chat");
}
