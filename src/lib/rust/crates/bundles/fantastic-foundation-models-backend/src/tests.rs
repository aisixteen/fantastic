//! foundation_models_backend against a fake engine (no model, no macOS 26): the
//! record it takes, what it refuses at the write, what reflect shows, how ai-core's
//! messages become one FMCore request, whole turns through a kernel, and
//! cancellation. The FMCore texts are pinned here AND checked against the swift
//! `FMCore.swift` in this crate ([`the_pinned_texts_are_fmcores_words`]).

use super::*;
use crate::engine::{FmAvailability, FmReason, FM_CANCELLED};
use crate::tests_fake::{FakeEngine, Step};
use fantastic_kernel::{Agent, Item};
use std::time::Duration;
use tempfile::TempDir;

const OFF_DEVICE_REFUSAL: &str = "foundation_models_backend: `{key}` has no meaning here — the model is Apple's on-device system model (no endpoint, no model choice, no credentials)";
const MISSING_STORE: &str = "foundation_models_backend: required on the record and missing: memory_store_id — nothing is guessed. memory_store_id = the memory store agent that keeps this agent's conversation (e.g. a ram_store)";
const TOO_BIG_4096: &str = "context_window must be a positive int no larger than the on-device model's window (4096 tokens)";
const NOT_A_USER_TURN: &str = "RuntimeError: foundation_models_backend: the last message is not a user turn (role {role}) — the on-device session answers a prompt";
const TEMPERATURE_HINT: &str = "sampling temperature (>= 0); unset, the on-device model's own";
const MAX_TOKENS_HINT: &str = "a turn's output budget (tokens); unset, the on-device model's own";

fn bundle(engine: FakeEngine) -> FoundationModelsBackendBundle {
    FoundationModelsBackendBundle::new(Arc::new(engine))
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("an object")
}

fn content(role: &str, text: &str) -> Value {
    json!({"role": role, "content": text})
}

fn raised(e: &ProviderError) -> String {
    format!("{}: {}", e.kind, e.message)
}

/// A kernel with the FM bundle on `engine`, a ram_store `memory`, and an FM
/// agent that may call its store and the cli sink.
///
/// EACH FIXTURE ITS OWN AGENT ID: ai-core keeps per-agent state in a
/// process-global table keyed by id (the FIFO lock, the in-flight entry, the
/// calibration), and the tests run in parallel in one process.
struct Fx {
    k: Arc<Kernel>,
    id: String,
    created: Value,
    _tmp: TempDir,
}

impl Fx {
    async fn call(&self, payload: Value) -> Value {
        call(&self.k, &self.id, payload).await
    }

    async fn send(&self, text: &str, extra: Value) -> Value {
        let mut payload = json!({"type": "send", "text": text});
        for (key, v) in obj(extra) {
            payload[key] = v;
        }
        self.call(payload).await
    }
}

async fn kernel(engine: &Arc<FakeEngine>, meta: Value) -> Fx {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = format!(
        "fm_{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let tmp = TempDir::new().unwrap();
    let mut k = Kernel::new();
    k.bundles.register(
        HANDLER_MODULE,
        FoundationModelsBackendBundle::new(Arc::clone(engine) as Arc<dyn FmEngine>),
    );
    k.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    let k = Arc::new(k);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    k.register(Arc::clone(&root));
    k.set_root(root);
    let memory = fantastic_ram_store::test_support::memory_meta(&k, &id, json!({"cli": "*"})).await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel_state",
        "handler_module": HANDLER_MODULE,
        "id": id,
    });
    for (key, v) in memory.into_iter().chain(obj(meta)) {
        create[key] = v;
    }
    let created = call(&k, "kernel_state", create).await;
    Fx {
        k,
        id,
        created,
        _tmp: tmp,
    }
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

// ── the record schema ───────────────────────────────────────────────

#[test]
fn the_schema_is_two_knobs_then_the_ai_fields_with_the_models_window() {
    let schema = bundle(FakeEngine::new().context_size(4096))
        .record_schema()
        .unwrap();
    let fields = schema["fields"].as_array().unwrap();
    let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "temperature",
            "max_tokens",
            "memory_store_id",
            "context_window",
            "output_reserve",
            "context_strategy",
            "recent_n",
            "caching",
            "cache_primer_until",
            "transport_retries",
            "misfire_limit",
            "system_prompt",
            "file_bridge_id",
            "system_prompt_file",
            "pre_system_prompt",
            "post_system_prompt"
        ]
    );
    // the window is the MODEL'S, read from the engine — not the kernel's 128k
    assert_eq!(fields[3]["default"], json!(4096));
    // the knobs have no default of ours: unset is the model's own
    assert!(fields[0].get("default").is_none() && fields[1].get("default").is_none());
    let required: Vec<&str> = fields
        .iter()
        .filter(|f| f["required"] == json!(true))
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(required, ["memory_store_id"]);
    // the exact field bytes swift publishes
    assert_eq!(
        fields[0].to_string(),
        json!({"name": "temperature", "type": "float", "required": false, "hint": TEMPERATURE_HINT})
            .to_string()
    );
    assert_eq!(
        fields[1].to_string(),
        json!({"name": "max_tokens", "type": "int", "required": false, "hint": MAX_TOKENS_HINT})
            .to_string()
    );
    // another engine, another window
    let big = bundle(FakeEngine::new().context_size(8192))
        .record_schema()
        .unwrap();
    assert_eq!(big["fields"][3]["default"], json!(8192));
    // every other field is ai-core's, byte for byte
    let ai = schema::schema_with_window(vec![], 4096);
    assert_eq!(
        Value::Array(fields[2..].to_vec()).to_string(),
        ai["fields"].to_string()
    );
}

#[tokio::test]
async fn the_catalog_carries_the_schema_with_no_agent_existing() {
    let engine = Arc::new(FakeEngine::new().context_size(4096));
    let mut k = Kernel::new();
    k.bundles.register(
        HANDLER_MODULE,
        FoundationModelsBackendBundle::new(Arc::clone(&engine) as Arc<dyn FmEngine>),
    );
    let k = Arc::new(k);
    let tmp = TempDir::new().unwrap();
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    k.register(Arc::clone(&root));
    k.set_root(root);
    let r = call(
        &k,
        "kernel_state",
        json!({"type": "reflect", "bundles": "all"}),
    )
    .await;
    let bundles = r["bundles"].as_array().cloned().unwrap_or_default();
    let fm = bundles
        .iter()
        .find(|b| b["name"] == json!(NAME))
        .unwrap_or_else(|| panic!("no {NAME} in {r}"));
    assert_eq!(fm["schema"]["fields"][3]["default"], json!(4096), "{fm}");
}

// ── validate_meta ───────────────────────────────────────────────────

#[test]
fn an_off_device_field_is_refused_at_the_write() {
    let b = bundle(FakeEngine::new());
    for key in ["endpoint", "model", "credentials"] {
        assert_eq!(
            b.validate_meta(
                &obj(json!({key: "x", "memory_store_id": "memory"})),
                &Map::new()
            ),
            Some(OFF_DEVICE_REFUSAL.replace("{key}", key))
        );
    }
    // a null clears nothing that was never there — not a refusal
    assert_eq!(
        b.validate_meta(
            &obj(json!({"model": null, "memory_store_id": "memory"})),
            &Map::new()
        ),
        None
    );
}

#[test]
fn the_knobs_are_refused_outside_their_shape() {
    let b = bundle(FakeEngine::new().context_size(4096));
    let check = |extra: Value| {
        let mut m = obj(json!({"memory_store_id": "memory"}));
        m.extend(obj(extra));
        b.validate_meta(&m, &Map::new())
    };
    let temp = Some("temperature must be a number >= 0".to_string());
    assert_eq!(check(json!({"temperature": -0.1})), temp);
    assert_eq!(check(json!({"temperature": "hot"})), temp);
    assert_eq!(check(json!({"temperature": true})), temp);
    assert_eq!(check(json!({"temperature": 0})), None);
    assert_eq!(check(json!({"temperature": 0.7})), None);
    let max = Some("max_tokens must be a positive int".to_string());
    assert_eq!(check(json!({"max_tokens": 0})), max);
    assert_eq!(check(json!({"max_tokens": 5.5})), max);
    assert_eq!(check(json!({"max_tokens": 256})), None);
    let too_big = Some(TOO_BIG_4096.to_string());
    assert_eq!(check(json!({"context_window": 8192})), too_big);
    assert_eq!(check(json!({"context_window": 0})), too_big);
    assert_eq!(check(json!({"context_window": 2048})), None);
    assert_eq!(check(json!({"context_window": 4096})), None);
}

#[test]
fn a_record_without_a_store_is_refused_naming_it() {
    let b = bundle(FakeEngine::new());
    assert_eq!(
        b.validate_meta(&Map::new(), &Map::new()),
        Some(MISSING_STORE.to_string())
    );
}

#[tokio::test]
async fn create_agent_refuses_an_off_device_field() {
    let engine = Arc::new(FakeEngine::new());
    let fx = kernel(&engine, json!({"model": "gpt"})).await;
    assert!(
        fx.created["error"]
            .as_str()
            .is_some_and(|e| e.contains("`model` has no meaning here")),
        "{}",
        fx.created
    );
}

// ── reflect ─────────────────────────────────────────────────────────

#[tokio::test]
async fn reflect_shows_availability_the_window_and_the_knobs() {
    let engine = Arc::new(FakeEngine::new().context_size(4096));
    let fx = kernel(&engine, json!({"temperature": 0.3})).await;
    assert_eq!(fx.created["id"], json!(fx.id), "{}", fx.created);
    let r = fx.call(json!({"type": "reflect"})).await;
    assert_eq!(r["sentence"], json!(SENTENCE));
    assert!(
        r["model"].is_null() && r["endpoint"].is_null(),
        "nothing off-device on the record"
    );
    assert_eq!(
        r["context_window"],
        json!(4096),
        "the model's window, not 128k"
    );
    assert_eq!(r["available"], json!(true));
    assert!(r["unavailable_reason"].is_null());
    assert_eq!(r["context_size"], json!(4096));
    assert_eq!(r["temperature"], json!(0.3));
    assert!(r["max_tokens"].is_null());
    // the extras ride AFTER the shared base, in this order
    // (the substrate's own keys — grants, tree — follow the bundle's reply)
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    let i = keys
        .iter()
        .position(|k| *k == "available")
        .expect("available");
    assert_eq!(
        keys[i..i + 5],
        [
            "available",
            "unavailable_reason",
            "context_size",
            "temperature",
            "max_tokens"
        ]
    );
    assert!(keys.iter().position(|k| *k == "generating").unwrap() < i);
    assert!(!keys.contains(&"concurrency") && !keys.contains(&"verbs"));

    engine.set_availability(FmAvailability::Unavailable(
        FmReason::MODEL_NOT_READY.to_string(),
    ));
    let r = fx.call(json!({"type": "reflect"})).await;
    assert_eq!(r["available"], json!(false));
    assert_eq!(r["unavailable_reason"], json!(FmReason::MODEL_NOT_READY));
}

#[tokio::test]
async fn a_smaller_record_window_wins() {
    let engine = Arc::new(FakeEngine::new().context_size(4096));
    let fx = kernel(&engine, json!({"context_window": 3000})).await;
    let r = fx.call(json!({"type": "reflect"})).await;
    assert_eq!(r["context_window"], json!(3000));
    let s = fx.call(json!({"type": "context_status"})).await;
    assert_eq!(s["context_window"], json!(3000), "{s}");
    assert_eq!(s["budget"], json!(3000 - 1024), "{s}");
}

#[tokio::test]
async fn context_status_budgets_against_the_models_window() {
    let engine = Arc::new(FakeEngine::new().context_size(4096));
    let fx = kernel(&engine, json!({})).await;
    let s = fx.call(json!({"type": "context_status"})).await;
    assert_eq!(s["context_window"], json!(4096), "{s}");
    assert_eq!(s["budget"], json!(4096 - 1024), "{s}");
}

// ── messages -> one request ─────────────────────────────────────────

#[test]
fn system_text_becomes_instructions_and_the_last_user_turn_the_prompt() {
    let r = FoundationModelsProvider::request(
        &[
            content("system", "you are fm"),
            content("user", "hi"),
            content("assistant", "hello"),
            content("system", "late note"),
            content("user", "again"),
        ],
        Some(0.2),
        Some(64),
    )
    .unwrap();
    assert_eq!(
        r,
        FmRequest {
            instructions: "you are fm\n\nlate note".to_string(),
            turns: vec![
                FmTurn {
                    role: "user".to_string(),
                    content: "hi".to_string()
                },
                FmTurn {
                    role: "assistant".to_string(),
                    content: "hello".to_string()
                },
            ],
            prompt: "again".to_string(),
            temperature: Some(0.2),
            max_tokens: Some(64),
        }
    );
}

#[test]
fn a_list_not_ending_in_a_user_turn_is_refused() {
    let refusal =
        |m: &[Value]| raised(&FoundationModelsProvider::request(m, None, None).unwrap_err());
    assert_eq!(
        refusal(&[content("user", "a"), content("assistant", "b")]),
        NOT_A_USER_TURN.replace("{role}", "assistant")
    );
    assert_eq!(
        refusal(&[content("system", "s")]),
        NOT_A_USER_TURN.replace("{role}", "none")
    );
}

#[test]
fn the_request_json_is_the_headers_shape() {
    let r = FmRequest {
        instructions: "be \"terse\"".to_string(),
        turns: vec![FmTurn {
            role: "user".to_string(),
            content: "é\n".to_string(),
        }],
        prompt: "p".to_string(),
        temperature: Some(0.5),
        max_tokens: None,
    };
    assert_eq!(
        r.to_json(),
        r#"{"instructions":"be \"terse\"","turns":[{"role":"user","content":"é\n"}],"prompt":"p","temperature":0.5,"max_tokens":null}"#
    );
}

async fn drain(stream: ProviderStream) -> (String, Option<ProviderError>) {
    use futures_util::StreamExt;
    let mut stream = stream;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => text.push_str(&t),
            Err(e) => return (text, Some(e)),
        }
    }
    (text, None)
}

fn provider(engine: &Arc<FakeEngine>, max_tokens: Option<i64>) -> FoundationModelsProvider {
    FoundationModelsProvider {
        engine: Arc::clone(engine) as Arc<dyn FmEngine>,
        temperature: None,
        max_tokens,
        refusal: None,
        report: ReportCell::default(),
    }
}

#[tokio::test]
async fn a_side_calls_bound_overrides_the_records_and_the_prompt_count_is_reported() {
    let engine = Arc::new(FakeEngine::new().script(vec![vec![
        Step::Text("sum"),
        Step::Text("mary"),
        Step::PromptTokens(321),
    ]]));
    let p = provider(&engine, Some(900));
    let (text, err) = drain(p.chat(&[content("user", "x")], Some(50)).await.unwrap()).await;
    assert!(err.is_none());
    assert_eq!(text, "summary");
    assert_eq!(engine.requests()[0].max_tokens, Some(50));
    assert_eq!(p.report().prompt_tokens, Some(321));
    assert_eq!(
        p.report().stop_reason,
        None,
        "FM reports no stop reason; none is claimed"
    );
    // the record's bound when no side bound is given
    let _ = drain(p.chat(&[content("user", "x")], None).await.unwrap()).await;
    assert_eq!(engine.requests()[1].max_tokens, Some(900));
}

#[tokio::test]
async fn a_generation_failure_is_raised_under_the_bundles_name() {
    let engine = Arc::new(
        FakeEngine::new()
            .script(vec![vec![Step::Text("par")]])
            .failure("the on-device model failed: The session's transcript exceeded the model's context size."),
    );
    let p = provider(&engine, None);
    let (text, err) = drain(p.chat(&[content("user", "x")], None).await.unwrap()).await;
    assert_eq!(
        raised(&err.expect("must fail")),
        "RuntimeError: foundation_models_backend: the on-device model failed: The session's \
         transcript exceeded the model's context size."
    );
    assert_eq!(text, "par");
    assert_eq!(
        p.report().prompt_tokens,
        None,
        "a failed pass reports no count"
    );
}

// ── the stream's lifetime ───────────────────────────────────────────

#[tokio::test]
async fn dropping_a_live_stream_cancels_it_once_and_a_finished_one_not_at_all() {
    let engine = FakeEngine::new().hang();
    let req = FmRequest {
        instructions: String::new(),
        turns: vec![],
        prompt: "p".to_string(),
        temperature: None,
        max_tokens: None,
    };
    let mut s = engine.stream(&req);
    assert_eq!(s.next().await, Some(FmEvent::Text("ok ".to_string())));
    drop(s);
    assert_eq!(engine.cancels(), 1, "a live stream is cancelled on drop");

    let done = FakeEngine::new();
    let mut s = done.stream(&req);
    while let Some(ev) = s.next().await {
        if matches!(ev, FmEvent::Done { .. }) {
            break;
        }
    }
    drop(s);
    assert_eq!(done.cancels(), 0, "a finished stream has nothing to cancel");

    // an explicit cancel keeps the stream readable: `cancelled` follows
    let mut s = engine.stream(&req);
    s.cancel();
    let mut last = None;
    while let Some(ev) = s.next().await {
        last = Some(ev);
    }
    assert_eq!(
        last,
        Some(FmEvent::Done {
            error: Some(FM_CANCELLED.to_string()),
            prompt_tokens: -1
        })
    );
    drop(s);
    assert_eq!(engine.cancels(), 2, "one cancel, not a second on drop");
}

#[tokio::test]
async fn a_broken_bridge_delta_is_an_error_and_still_cancels_on_drop() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    let s = FmStream::new(rx, move || {
        n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    tx.send(FmEvent::Broken("not UTF-8".to_string())).unwrap();
    let (_, err) = drain(events(s, ReportCell::default())).await;
    assert_eq!(
        err.map(|e| e.message),
        Some("foundation_models_backend: not UTF-8".to_string())
    );
    assert_eq!(
        n.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the generation is still running — it is cancelled"
    );
}

// ── whole turns through the kernel ──────────────────────────────────

#[tokio::test]
async fn a_send_streams_the_engines_text_and_replays_the_conversation_next_turn() {
    let engine = Arc::new(FakeEngine::new().context_size(32768).script(vec![
        vec![
            Step::Text("first "),
            Step::Text("<call name=\"done\"></call>"),
            Step::PromptTokens(1200),
        ],
        vec![Step::Text("second <call name=\"done\"></call>")],
    ]));
    let fx = kernel(&engine, json!({"temperature": 0.1, "max_tokens": 300})).await;
    assert_eq!(fx.created["id"], json!(fx.id), "{}", fx.created);
    let one = fx.send("hi", json!({})).await;
    assert!(
        one["response"]
            .as_str()
            .is_some_and(|r| r.starts_with("first ")),
        "{one}"
    );
    let req0 = engine.requests()[0].clone();
    assert!(
        !req0.instructions.is_empty(),
        "the system block is the instructions"
    );
    assert!(req0.turns.is_empty());
    assert_eq!(req0.prompt, "hi");
    assert_eq!((req0.temperature, req0.max_tokens), (Some(0.1), Some(300)));
    let status = fx.call(json!({"type": "context_status"})).await;
    assert_eq!(status["reported_prompt_tokens"], json!(1200), "{status}");

    let two = fx.send("again", json!({})).await;
    assert!(
        two["response"]
            .as_str()
            .is_some_and(|r| r.starts_with("second")),
        "{two}"
    );
    let reqs = engine.requests();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    let roles: Vec<&str> = reqs[1].turns.iter().map(|t| t.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant"]);
    assert_eq!(reqs[1].turns[0].content, "hi");
    assert!(reqs[1].turns[1].content.starts_with("first "));
    assert_eq!(reqs[1].prompt, "again");
}

#[tokio::test]
async fn a_lean_system_prompt_turn_runs_in_the_real_on_device_window() {
    // 4096 tokens, the window the SDK reports on macOS 27.0: a caller's own
    // short system block is the instructions, verbatim, and the turn runs.
    let engine = Arc::new(FakeEngine::new().context_size(4096));
    let fx = kernel(&engine, json!({})).await;
    let r = fx.send("hi", json!({"system_prompt": "Be terse."})).await;
    assert!(
        r["response"].as_str().is_some_and(|t| t.starts_with("ok ")),
        "{r}"
    );
    let instructions: Vec<String> = engine
        .requests()
        .into_iter()
        .map(|r| r.instructions)
        .collect();
    assert_eq!(instructions, ["Be terse."]);
}

#[tokio::test]
async fn the_default_system_block_fits_the_on_device_window() {
    // With the verb docs out of the reflects and the howto tight, the kernel's
    // default system block fits the 4096-token on-device window — the turn runs
    // with no `system_prompt`.
    let engine = Arc::new(FakeEngine::new().context_size(4096));
    let fx = kernel(&engine, json!({})).await;
    let r = fx.send("hi", json!({})).await;
    assert!(
        r["response"].as_str().is_some_and(|t| t.starts_with("ok ")),
        "{r}"
    );
    assert!(!engine.requests().is_empty(), "the model is called");
}

#[tokio::test]
async fn a_send_on_an_unavailable_model_fails_loud_naming_the_reason() {
    let engine = Arc::new(FakeEngine::new().unavailable(FmReason::APPLE_INTELLIGENCE_NOT_ENABLED));
    let fx = kernel(&engine, json!({})).await;
    assert_eq!(
        fx.created["id"],
        json!(fx.id),
        "creating the agent needs no model"
    );
    let r = fx.send("hi", json!({})).await;
    assert_eq!(
        r["error"],
        json!(format!(
            "RuntimeError: foundation_models_backend: the on-device model is unavailable: {}",
            FmReason::APPLE_INTELLIGENCE_NOT_ENABLED
        )),
        "{r}"
    );
    assert!(
        engine.requests().is_empty(),
        "nothing reached the engine — and nothing else either"
    );
}

#[tokio::test]
async fn unavailability_is_named_before_the_window_is_judged() {
    // The real window (4096) AND the default system block, which cannot fit: the
    // caller still reads WHY the model cannot answer, not `context_insufficient`
    // — the availability refusal comes first in the turn (swift `makeProvider`,
    // python `_get_provider`).
    let engine = Arc::new(
        FakeEngine::new()
            .context_size(4096)
            .unavailable(FmReason::DEVICE_NOT_ELIGIBLE),
    );
    let fx = kernel(&engine, json!({})).await;
    let r = fx.send("hi", json!({})).await;
    assert_eq!(
        r["error"],
        json!(format!(
            "RuntimeError: foundation_models_backend: the on-device model is unavailable: {}",
            FmReason::DEVICE_NOT_ELIGIBLE
        )),
        "{r}"
    );
    assert!(engine.requests().is_empty());
}

#[tokio::test]
async fn a_kernel_cancel_of_the_send_cancels_the_on_device_stream() {
    let engine = Arc::new(FakeEngine::new().hang());
    let fx = Arc::new(kernel(&engine, json!({})).await);
    let fx2 = Arc::clone(&fx);
    let join =
        tokio::spawn(async move { fx2.send("hi", json!({"system_prompt": "Be terse."})).await });
    // wait for the generation to be in flight
    let mut task_id = None;
    for _ in 0..200 {
        let table = call(&fx.k, "kernel", json!({"type": "ps"})).await;
        task_id = table["tasks"].as_array().and_then(|ts| {
            ts.iter()
                .find(|t| t["agent"] == json!(fx.id) && t["verb"] == json!("send"))
                .and_then(|t| t["id"].as_i64())
        });
        if task_id.is_some() && !engine.requests().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let task_id = task_id.expect("the in-flight send must be in `ps`");
    let r = call(
        &fx.k,
        "kernel",
        json!({"type": "cancel", "task_id": task_id}),
    )
    .await;
    assert_eq!(r["cancelled"], json!(true), "{r}");
    let reply = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("the send must end once cancelled")
        .unwrap();
    assert_eq!(reply, json!({"cancelled": true}));
    assert_eq!(engine.cancels(), 1, "the on-device stream was cancelled");
    let st = fx.call(json!({"type": "status"})).await;
    assert_eq!(st["generating"], json!(false));
}

#[tokio::test]
async fn an_unknown_type_names_the_bundle() {
    let engine = Arc::new(FakeEngine::new());
    let fx = kernel(&engine, json!({})).await;
    let r = fx.call(json!({"type": "nope"})).await;
    assert_eq!(
        r["error"],
        json!("foundation_models_backend: unknown type 'nope'")
    );
}

// ── the readme, and the texts FMCore publishes ──────────────────────

fn swift_file(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("swift")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A swift source with its split string literals joined — `"a " + "b"` (across
/// lines too) reads `"a b"` — so a sentence can be found whole.
fn joined(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            // `"` ws* `+` ws* `"` → nothing
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'+' {
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < b.len() && b[k] == b'"' {
                    i = k + 1;
                    continue;
                }
            }
        }
        // copy one UTF-8 character
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[test]
fn the_readme_is_the_bundles_one_file() {
    assert_eq!(bundle(FakeEngine::new()).readme(), Some(README));
    assert!(README.starts_with("# foundation_models_backend — "));
}

/// The FMCore texts this crate spells itself, found verbatim in `FMCore.swift`
/// — so a rewording on either side fails here, not in production.
#[test]
fn the_pinned_texts_are_fmcores_words() {
    let core_src = joined(&swift_file("Sources/FantasticFMCore/FMCore.swift"));
    let has = |src: &str, what: &str, text: &str| {
        assert!(src.contains(text), "swift {what} does not say: {text}");
    };
    // FMCore's reason sentences and refusals — the ones this crate spells itself
    for reason in [
        FmReason::DEVICE_NOT_ELIGIBLE,
        FmReason::APPLE_INTELLIGENCE_NOT_ENABLED,
        FmReason::MODEL_NOT_READY,
    ] {
        has(&core_src, "FMCore", &format!("\"{reason}\""));
    }
    has(&core_src, "FMCore", &FmReason::platform("\\(what)"));
    has(&core_src, "FMCore", &unavailable_message("\\(reason)"));
    has(&core_src, "FMCore", &format!("\"{FM_CANCELLED}\""));
    has(
        &core_src,
        "FMCore",
        &format!(
            "FM_DOCUMENTED_CONTEXT_SIZE = {}",
            crate::engine::FM_DOCUMENTED_CONTEXT_SIZE
        ),
    );
}

#[test]
fn the_platform_refusal_is_fmcores_sentence() {
    assert_eq!(
        FmReason::platform("Linux"),
        "platform not macOS 26+ — this is Linux"
    );
    assert_eq!(
        FmAvailability::from_bridge_json(r#"{"available":true,"reason":null}"#),
        FmAvailability::Available
    );
    assert_eq!(
        FmAvailability::from_bridge_json(r#"{"available":false,"reason":"model not ready — x"}"#),
        FmAvailability::Unavailable("model not ready — x".to_string())
    );
    assert_eq!(
        FmAvailability::from_bridge_json("nope"),
        FmAvailability::Unavailable(
            "fm_availability answered something that is not {available, reason}: nope".to_string()
        ),
        "a broken bridge is never read as available"
    );
}

/// The engine every non-macOS build runs: it refuses, naming the platform, and
/// starts nothing.
#[tokio::test]
async fn off_macos_the_engine_refuses_naming_the_platform() {
    #[cfg(not(target_os = "macos"))]
    use crate::system::SystemEngine as Refusing;
    #[cfg(target_os = "macos")]
    use crate::system::{platform_name, ElsewhereEngine as Refusing};
    #[cfg(target_os = "macos")]
    {
        assert_eq!(platform_name("linux"), "Linux");
        assert_eq!(platform_name("ios"), "iOS");
        assert_eq!(platform_name("visionos"), "visionOS");
        assert_eq!(platform_name("freebsd"), "freebsd");
    }
    let e = Refusing::new();
    let reason = e
        .availability()
        .reason()
        .map(str::to_string)
        .expect("refused");
    assert!(
        reason.starts_with("platform not macOS 26+ — this is "),
        "{reason}"
    );
    assert_eq!(e.context_size(), crate::engine::FM_DOCUMENTED_CONTEXT_SIZE);
    let req = FmRequest {
        instructions: String::new(),
        turns: vec![],
        prompt: "p".to_string(),
        temperature: None,
        max_tokens: None,
    };
    let mut s = e.stream(&req);
    assert_eq!(
        s.next().await,
        Some(FmEvent::Done {
            error: Some(unavailable_message(&reason)),
            prompt_tokens: -1
        })
    );
    // and a bundle over it fails a send loud, before anything is assembled
    let p = FoundationModelsBackendBundle::new(Arc::new(Refusing::new()));
    let refusal = p
        .engine
        .availability()
        .reason()
        .map(|r| format!("{NAME}: {}", unavailable_message(r)));
    assert_eq!(
        refusal,
        Some(format!(
            "foundation_models_backend: the on-device model is unavailable: {reason}"
        ))
    );
}
