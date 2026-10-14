//! The REAL bridge, linked — macOS only.
//!
//! HERMETIC (default suite): every case here is answered by the swift bridge
//! BEFORE any generation starts — a malformed `request_json` (parsed first), an
//! invalid turn or option (FMCore's preflight, after the availability check), the
//! NULL / unknown-handle no-ops. No prompt reaches the model, so the suite runs
//! the same on a machine without Apple Intelligence.
//!
//! LIVE (opt-in): `FM_LIVE=1 cargo test -p fantastic-foundation-models-backend
//! live_on_device_model -- --nocapture` runs tiny real prompts through the
//! on-device model and prints what it measured (`FM_LIVE …` lines).

use crate::engine::{
    FmAvailability, FmEngine, FmEvent, FmReason, FmRequest, FmStream, FmTurn, FM_CANCELLED,
};
use crate::system::SystemEngine;
use std::time::{Duration, Instant};

/// Read a stream to its end: (texts, the `Done`).
async fn collect(mut s: FmStream) -> (Vec<String>, Option<String>, i64) {
    let mut texts = Vec::new();
    let read = async {
        while let Some(ev) = s.next().await {
            match ev {
                FmEvent::Text(t) => texts.push(t),
                FmEvent::Done {
                    error,
                    prompt_tokens,
                } => return (error, prompt_tokens),
                FmEvent::Broken(e) => return (Some(format!("BROKEN: {e}")), -1),
            }
        }
        (Some("the stream closed with no Done".to_string()), -1)
    };
    let (error, pt) = tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .expect("the bridge must call on_done");
    (texts, error, pt)
}

/// The bridge's answer to one raw `request_json` that never reaches the model.
async fn refusal(json: &str) -> String {
    let (texts, error, pt) = collect(SystemEngine::new().stream_json(json)).await;
    assert!(texts.is_empty(), "{json}: {texts:?}");
    assert_eq!(pt, -1, "{json}: no prompt was counted");
    error.unwrap_or_else(|| panic!("{json}: must be refused"))
}

#[test]
fn availability_and_the_window_come_from_the_bridge() {
    let e = SystemEngine::new();
    match e.availability() {
        FmAvailability::Available => {}
        FmAvailability::Unavailable(r) => assert!(
            [
                FmReason::DEVICE_NOT_ELIGIBLE,
                FmReason::APPLE_INTELLIGENCE_NOT_ENABLED,
                FmReason::MODEL_NOT_READY,
                "platform not macOS 26+ — this is ",
                "unavailable for a reason this build does not know: ",
            ]
            .iter()
            .any(|p| r.starts_with(p)),
            "an unknown reason sentence: {r}"
        ),
    }
    let n = e.context_size();
    assert!(n >= 1, "{n}");
    assert_eq!(e.context_size(), n, "read once");
}

#[tokio::test]
async fn a_malformed_request_json_is_refused_in_fmcores_words() {
    for (json, want) in [
        (
            r#"{"turns":[],"prompt":"p"}"#,
            "request_json: `instructions` is missing",
        ),
        (
            r#"{"instructions":"","turns":[{"role":"user"}],"prompt":"p"}"#,
            "request_json: `turns[0].content` is missing",
        ),
        (
            r#"{"instructions":"","turns":[],"prompt":5}"#,
            "request_json: `prompt` is not a string",
        ),
        (
            r#"{"instructions":"","turns":[],"prompt":"p","max_tokens":"9"}"#,
            "request_json: `max_tokens` is not an int",
        ),
        (
            r#"{"instructions":"","turns":"x","prompt":"p"}"#,
            "request_json: `turns` is not a list",
        ),
        (
            r#"{"instructions":null,"turns":[],"prompt":"p"}"#,
            "request_json: `instructions` is null, expected a string",
        ),
        ("[1]", "request_json: the document is not an object"),
    ] {
        assert_eq!(refusal(json).await, want, "{json}");
    }
    assert!(refusal("not json").await.starts_with("request_json: "));
    assert_eq!(
        refusal("a\0b").await,
        "request_json carries a NUL byte — the C ABI takes a NUL-terminated string"
    );
}

#[tokio::test]
async fn an_invalid_turn_or_option_is_refused_before_the_model() {
    let e = SystemEngine::new();
    let req = |turns: Vec<FmTurn>, temperature: Option<f64>, max_tokens: Option<i64>| FmRequest {
        instructions: String::new(),
        turns,
        prompt: "p".to_string(),
        temperature,
        max_tokens,
    };
    let cases = [
        (
            req(
                vec![FmTurn {
                    role: "tool".to_string(),
                    content: "x".to_string(),
                }],
                None,
                None,
            ),
            "turn 0 has role \"tool\" — a transcript turn is user or assistant (system text goes in instructions)",
        ),
        (
            req(vec![], Some(-0.5), None),
            "temperature must be a number >= 0, got -0.5",
        ),
        (
            req(vec![], None, Some(0)),
            "max_tokens must be a positive int, got 0",
        ),
    ];
    for (r, want) in cases {
        let (texts, error, _) = collect(e.stream(&r)).await;
        assert!(texts.is_empty());
        // FMCore checks availability FIRST: an unavailable machine refuses
        // every request with the reason instead.
        let want = match e.availability() {
            FmAvailability::Available => want.to_string(),
            FmAvailability::Unavailable(reason) => crate::engine::unavailable_message(&reason),
        };
        assert_eq!(error.as_deref(), Some(want.as_str()));
    }
}

#[test]
fn the_null_and_unknown_handle_cases_are_no_ops() {
    use crate::system::bridge_raw as raw;
    raw::free_null();
    raw::cancel_unknown(u64::MAX);
    assert_eq!(raw::stream_without_callbacks(), 0);
}

// ── LIVE, opt-in ────────────────────────────────────────────────────

fn tiny(instructions: &str, turns: &[(&str, &str)], prompt: &str) -> FmRequest {
    FmRequest {
        instructions: instructions.to_string(),
        turns: turns
            .iter()
            .map(|(r, c)| FmTurn {
                role: (*r).to_string(),
                content: (*c).to_string(),
            })
            .collect(),
        prompt: prompt.to_string(),
        temperature: Some(0.0),
        max_tokens: Some(32),
    }
}

/// The on-device model, for real. Skipped unless `FM_LIVE=1`.
#[tokio::test]
async fn live_on_device_model() {
    if std::env::var("FM_LIVE").as_deref() != Ok("1") {
        eprintln!("live_on_device_model: skipped (set FM_LIVE=1 to run it)");
        return;
    }
    let e = SystemEngine::new();
    let a = e.availability();
    eprintln!(
        "FM_LIVE availability={a:?} context_size={}",
        e.context_size()
    );
    let FmAvailability::Available = a else {
        panic!("FM_LIVE=1 but the model is unavailable: {a:?}");
    };

    // 1. a tiny prompt through the C ABI
    let t = Instant::now();
    let (texts, error, pt) = collect(e.stream(&tiny(
        "Answer with one word.",
        &[],
        "Reply with the word pong.",
    )))
    .await;
    eprintln!(
        "FM_LIVE tiny: text={:?} deltas={} error={error:?} prompt_tokens={pt} {:.2}s",
        texts.concat(),
        texts.len(),
        t.elapsed().as_secs_f64()
    );
    assert_eq!(error, None);
    assert!(!texts.concat().trim().is_empty());

    // 2. the transcript is replayed: the model reads the earlier turns
    let t = Instant::now();
    let (texts, error, pt) = collect(e.stream(&tiny(
        "Answer with one word.",
        &[("user", "My name is Zed."), ("assistant", "Noted.")],
        "What is my name?",
    )))
    .await;
    eprintln!(
        "FM_LIVE transcript: text={:?} error={error:?} prompt_tokens={pt} {:.2}s",
        texts.concat(),
        t.elapsed().as_secs_f64()
    );
    assert_eq!(error, None);
    assert!(texts.concat().contains("Zed"), "{texts:?}");

    // 3. cancel mid-generation: `cancelled` follows, the count did not finish
    let t = Instant::now();
    let mut s = e.stream(&FmRequest {
        max_tokens: Some(400),
        ..tiny("", &[], "Count from 1 to 200, one number per line.")
    });
    let mut deltas = 0usize;
    let mut bytes = 0usize;
    let mut done = None;
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(60), s.next())
        .await
        .expect("a live stream must progress")
    {
        match ev {
            FmEvent::Text(x) => {
                deltas += 1;
                bytes += x.len();
                if deltas == 3 {
                    s.cancel();
                }
            }
            FmEvent::Done { error, .. } => {
                done = Some(error);
                break;
            }
            FmEvent::Broken(x) => panic!("{x}"),
        }
    }
    eprintln!(
        "FM_LIVE cancel: ended after {deltas} deltas / {bytes} bytes, done={done:?} {:.2}s",
        t.elapsed().as_secs_f64()
    );
    assert_eq!(done, Some(Some(FM_CANCELLED.to_string())));

    // 4. one kernel turn through a foundation_models_backend agent
    let t = Instant::now();
    let (k, reply, status) = live_kernel_turn().await;
    eprintln!(
        "FM_LIVE kernel send: {reply} reported_prompt_tokens={} {:.2}s",
        status["reported_prompt_tokens"],
        t.elapsed().as_secs_f64()
    );
    assert!(reply.get("error").is_none(), "{reply}");
    drop(k);

    // 5. the default system block, against the real window: refused before the
    // model is called
    let (_k, reply) = live_default_block_turn().await;
    eprintln!("FM_LIVE default system block: {reply}");
    assert!(
        reply["error"]
            .as_str()
            .is_some_and(|e| e.contains("context_insufficient")),
        "{reply}"
    );
}

/// A kernel with one system-model agent `id` (ids differ per call: ai-core's
/// per-agent state is process-global).
async fn live_kernel(id: &str) -> (std::sync::Arc<fantastic_kernel::Kernel>, tempfile::TempDir) {
    use fantastic_kernel::{Agent, AgentId, Item, Kernel};
    use serde_json::{json, Map};
    use std::sync::Arc;
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::new();
    k.bundles.register(
        crate::HANDLER_MODULE,
        crate::FoundationModelsBackendBundle::system(),
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
    let memory = fantastic_ram_store::test_support::memory_meta(&k, id, json!({"cli": "*"})).await;
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": crate::HANDLER_MODULE, "id": id, "temperature": 0,
    });
    for (key, v) in memory {
        create[key] = v;
    }
    let created = k
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(created["id"], json!(id), "{created}");
    (k, tmp)
}

async fn live_kernel_turn() -> (
    (std::sync::Arc<fantastic_kernel::Kernel>, tempfile::TempDir),
    serde_json::Value,
    serde_json::Value,
) {
    use fantastic_kernel::{AgentId, Item};
    use serde_json::json;
    let (k, tmp) = live_kernel("fm_live_turn").await;
    let reply = k
        .send(vec![Item::new(
            AgentId::from("fm_live_turn"),
            json!({"type": "send", "text": "Say hello, then end the chain.",
                   "system_prompt": "Be terse. End every turn with <call name=\"done\"></call>."}),
        )])
        .await
        .remove(0)
        .result;
    let status = k
        .send(vec![Item::new(
            AgentId::from("fm_live_turn"),
            json!({"type": "context_status"}),
        )])
        .await
        .remove(0)
        .result;
    ((k, tmp), reply, status)
}

async fn live_default_block_turn() -> (
    (std::sync::Arc<fantastic_kernel::Kernel>, tempfile::TempDir),
    serde_json::Value,
) {
    use fantastic_kernel::{AgentId, Item};
    use serde_json::json;
    let (k, tmp) = live_kernel("fm_live_default").await;
    let reply = k
        .send(vec![Item::new(
            AgentId::from("fm_live_default"),
            json!({"type": "send", "text": "hi"}),
        )])
        .await
        .remove(0)
        .result;
    ((k, tmp), reply)
}
