//! The `stop` call (#766, 1.8; spec 2.7): the harness stops a turn after
//! `1 + CALL_RETRIES` malformed attempts, a model may write `stop` itself, and both end
//! the chain through the same give-up.

use super::*;
use crate::model_format::hermes::Hermes;
use crate::model_format::Renderer;

const DONE: &str = r#"<call name="done"></call>"#;
/// A foreign call tag: the `default` reader fails every attempt at it.
const FOREIGN: &str = "Sending.\n<invoke name=\"send\">";
/// The model id of a `hermes` chat.
const QWEN: &str = "qwen3-8b";

/// The harness's reason for `n` malformed attempts, in the `default` wording.
fn harness_reason() -> (String, usize) {
    let n = 1 + crate::constants::CALL_RETRIES;
    (
        crate::model_format::fill(
            crate::model_format::default::STOP_REASON,
            &[("n", &n.to_string())],
        ),
        n,
    )
}

/// The `stop` call with `reason`, in the internal form.
fn stop_call(reason: &str) -> String {
    let mut args = Map::new();
    args.insert("reason".to_string(), json!(reason));
    crate::tool_parse::render_call("stop", &args)
}

async fn set_model(kernel: &Arc<Kernel>, agent: &AgentId, model: &str) {
    let r = fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "model": model}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
}

/// The assistant rows of `agent`'s store.
async fn assistant_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["role"] == json!("assistant"))
        .collect()
}

/// What one chain ended with: its last call-ending status, the asker's request that
/// carried the inquiry's close, the passes it took, its last stored row and its `stop`
/// trace rows.
struct Outcome {
    status: Value,
    closed: String,
    passes: usize,
    row: String,
    stops: Vec<Value>,
}

/// The last status of `rx` that ends a call.
fn last_end(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Value {
    let mut ends = Vec::new();
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status")
            && ["done", "failed"]
                .contains(&p["phase"].as_str().unwrap_or_default())
        {
            ends.push(p);
        }
    }
    ends.last().cloned().expect("an ending status")
}

/// The status of a chain the model stopped with `why`: `failed`, `gave_up`, the reason
/// as `error`, the key set of the give-up status.
fn assert_model_stop(st: &Value, why: &str) {
    assert_eq!(st["phase"], json!("failed"), "{st}");
    assert_eq!(st["detail"]["reason"], json!("gave_up"), "{st}");
    assert_eq!(st["detail"]["error"], json!(why), "{st}");
    let keys: Vec<&String> = st["detail"].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["reason", "error", "queue_depth"], "{st}");
}

/// `a` answers a peer's inquiry with `passes`; what it ended with.
async fn run_chain(tag: &str, model: Option<&str>, passes: Vec<Vec<ScriptEvent>>) -> Outcome {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, tag).await;
    if let Some(m) = model {
        set_model(&kernel, &a, m).await;
    }
    let b = add_peer(&kernel, &tmp, &a).await;
    // b asks a and is done for now; it then gets the inquiry's close, then a's hand-off
    let ask = call_envelope(a.as_str(), json!({"type": "inquiry", "text": "go"}), None);
    let fb = script(
        &b,
        vec![
            pass(ask),
            pass(DONE),
            pass(DONE),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let s = script(&a, passes, Duration::ZERO);
    if let Some(m) = model {
        *s.model.lock().unwrap() = m.to_string();
    }
    let mut rx = rebind_backend_inbox(&kernel, &a);
    let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            b.clone(),
            json!({"type": "send", "text": "ask", "client_id": "probe"}),
        )]))
        .await;
    assert!(
        until_passes(&fb, 3).await,
        "{tag}: the asker never heard back"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut ends = Vec::new();
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status")
            && ["done", "failed"]
                .contains(&p["phase"].as_str().unwrap_or_default())
        {
            ends.push(p);
        }
    }
    let status = ends.last().cloned().expect("an ending status");
    let rows = assistant_rows(&kernel, &a).await;
    let all = fantastic_kernel::test_support::as_agent(
        a.clone(),
        crate::memory::store_call(&a, &kernel, json!({"type": "read", "kinds": ["trace"]})),
    )
    .await
    .expect("store read");
    let stops: Vec<Value> = all["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["event"] == json!("stop"))
        .collect();
    Outcome {
        status,
        closed: fb.request(2),
        passes: s.passes_served(),
        row: rows
            .last()
            .and_then(|r| r["content"].as_str())
            .unwrap_or_default()
            .to_string(),
        stops,
    }
}

// T9
/// The harness stops the turn: status `failed`, `reason: gave_up`, `error` the reason,
/// the key set of the give-up status; the asker's inquiry is closed with it; the chain
/// ends with nothing more asked and nothing routed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_stop_fails_turn() {
    let (reason, n) = harness_reason();
    let out = run_chain("st_t9", None, (0..12).map(|_| pass(FOREIGN)).collect()).await;
    assert_eq!(out.passes, n, "no nudge, no reroll");
    let st = &out.status;
    assert_eq!(st["phase"], json!("failed"), "{st}");
    assert_eq!(st["detail"]["reason"], json!("gave_up"), "{st}");
    assert_eq!(st["detail"]["error"], json!(reason), "{st}");
    let keys: Vec<&String> = st["detail"].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["reason", "error", "queue_depth"], "{st}");
    assert!(out.closed.contains(&reason), "{}", out.closed);
}

// T17
/// The stored row of a harness-stopped turn is exactly the `stop` call with the reason;
/// the reason is `stop_reason.txt` filled with the attempt count and holds no byte of any
/// attempt; the parser reads it back; the next pass shows it in its type's own form.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_row_is_a_stop_call() {
    let (reason, _) = harness_reason();
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "st_t17").await;
    let s = script(
        &a,
        vec![
            pass(FOREIGN),
            pass(FOREIGN),
            pass(FOREIGN),
            pass(DONE),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await;
    let rows = assistant_rows(&kernel, &a).await;
    let content = rows[0]["content"].as_str().expect("the stop row");
    assert_eq!(content, stop_call(&reason));
    assert!(
        !reason.contains("invoke") && !reason.contains("Sending"),
        "{reason}"
    );
    let calls = crate::tool_parse::extract_tool_calls(content);
    assert_eq!(calls.len(), 1, "{content}");
    assert_eq!(calls[0].name, "stop");
    assert_eq!(calls[0].arguments["reason"], json!(reason));

    // D: the next request shows it as stored
    let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "again", "client_id": "probe"}),
        )]))
        .await;
    let seen = s.request_messages(3);
    let shown = seen
        .iter()
        .filter(|m| m["role"] == json!("assistant"))
        .filter_map(|m| m["content"].as_str())
        .map(|c| c.to_string())
        .collect::<Vec<_>>();
    assert!(shown.contains(&stop_call(&reason)), "{shown:?}");

    // H: the same row in the hermes form
    set_model(&kernel, &a, QWEN).await;
    *s.model.lock().unwrap() = QWEN.to_string();
    let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "once more", "client_id": "probe"}),
        )]))
        .await;
    let seen = s.request_messages(4);
    let shown = seen
        .iter()
        .filter(|m| m["role"] == json!("assistant"))
        .filter_map(|m| m["content"].as_str())
        .map(|c| c.to_string())
        .collect::<Vec<_>>();
    assert!(
        shown.contains(&Hermes.render(&stop_call(&reason))),
        "{shown:?}"
    );
}

// T18
/// A stopped turn, then the next send: a normal turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_turn_after_stop() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "st_t18").await;
    let s = script(
        &a,
        vec![
            pass(FOREIGN),
            pass(FOREIGN),
            pass(FOREIGN),
            pass("All good.\n<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _first = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(json!(crate::tests::last_answer(&kernel, &a).await)
        .as_str()
        .is_some_and(|r| r.starts_with("<call name=\"stop\">")));
    let mut rx = rebind_backend_inbox(&kernel, &a);
    let second = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "next", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(
        json!(crate::tests::last_answer(&kernel, &a).await),
        json!("All good.\n<call name=\"done\"></call>"),
        "{second}"
    );
    assert_eq!(s.passes_served(), 4);
    let evs = drain(&mut rx).await;
    assert!(
        evs.iter()
            .any(|v| v["type"] == json!("status") && v["phase"] == json!("done")),
        "{evs:#?}"
    );
}

// T10
/// The no-call give-up (#795) is the harness stop: answers with no call, `1 +
/// CALL_RETRIES` attempts, then the give-up status with the harness's reason, byte for
/// byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn give_up_status_unchanged() {
    let (reason, n) = harness_reason();
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "st_t10").await;
    let s = script(
        &a,
        (0..8).map(|_| pass("just prose")).collect(),
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &a);
    let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "tab1"}),
        )]))
        .await;
    assert!(until_passes(&s, n).await, "the pass stalled before its last attempt");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.passes_served(), n, "nothing is asked after the stop");
    let st = last_end(&mut rx);
    assert_eq!(st["phase"], json!("failed"), "{st}");
    let want = format!(
        r#"{{"reason":"gave_up","error":{},"queue_depth":{}}}"#,
        json!(reason),
        st["detail"]["queue_depth"]
    );
    assert_eq!(st["detail"].to_string(), want);
    assert_eq!(
        reason,
        "3 attempts in a row wrote malformed calls or no call, so the harness stopped the turn."
    );
}

// T24 (D in W13, H in W7)
/// A model writes `stop` with a reason: the provider is called once, the row is stored
/// as written, and the chain ends through the give-up with the model's reason; the
/// inquiry closed with it; nothing more asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_stop_fails_chain() {
    let why = "the file it names does not exist";
    // D: the internal form, as written
    let out = run_chain("st_t24d", None, vec![pass(stop_call(why)), pass(DONE)]).await;
    assert_eq!(out.passes, 1, "no retry, no nudge");
    assert_eq!(out.row, stop_call(why));
    assert_model_stop(&out.status, why);
    assert!(out.closed.contains(why), "{}", out.closed);

    // H
    let out = run_chain(
        "st_t24h",
        Some(QWEN),
        vec![pass(Hermes.render(&stop_call(why))), pass(DONE)],
    )
    .await;
    assert_eq!(out.passes, 1, "no retry, no nudge");
    assert_eq!(out.row, stop_call(why));
    let st = &out.status;
    assert_eq!(st["phase"], json!("failed"), "{st}");
    assert_eq!(st["detail"]["reason"], json!("gave_up"), "{st}");
    assert_eq!(st["detail"]["error"], json!(why), "{st}");
    assert!(out.closed.contains(why), "{}", out.closed);
}

// T25
/// `stop` beside `done`: `stop` wins, and the chain ends as in T24.
/// Beside a `send`, the `send` is dispatched, as it is beside `done` (section 5, item 5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_beside_other_calls() {
    let why = "the task names a repository that is not there";
    for (tag, other) in [("st_t25d", DONE)] {
        for (i, answer) in [
            format!("{}\n{other}", stop_call(why)),
            format!("{other}\n{}", stop_call(why)),
        ]
        .into_iter()
        .enumerate()
        {
            let tag = format!("{tag}{i}");
            let out = run_chain(&tag, None, vec![pass(answer.clone()), pass(DONE)]).await;
            assert_eq!(out.passes, 1, "{answer}");
            assert_eq!(out.row, answer);
            assert_model_stop(&out.status, why);
            assert!(out.closed.contains(why), "{answer}: {}", out.closed);
        }
    }

    // beside a `send`: the `send` reaches its target, and the chain ends failed
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "st_t25s").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let fb = script(&b, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let side = call_envelope(
        b.as_str(),
        json!({"type": "send", "text": "beside the stop"}),
        Some("null"),
    );
    let s = script(
        &a,
        vec![pass(format!("{side}\n{}", stop_call(why))), pass(DONE)],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &a);
    let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await;
    assert!(
        until_passes(&fb, 1).await,
        "the send beside the stop never ran"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        fb.request(0).contains("beside the stop"),
        "{}",
        fb.request(0)
    );
    assert_eq!(s.passes_served(), 1, "the chain ended");
    assert_model_stop(&last_end(&mut rx), why);
}

// T26
/// The harness's stop and a model's stop with the same reason text end alike: the same
/// `reason`, `error`, inquiry close and stored call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_writers_same_outcome() {
    let (reason, n) = harness_reason();
    let harness = run_chain("st_t26a", None, (0..12).map(|_| pass(FOREIGN)).collect()).await;
    let model = run_chain("st_t26b", None, vec![pass(stop_call(&reason)), pass(DONE)]).await;
    for key in ["reason", "error"] {
        assert_eq!(
            harness.status["detail"][key], model.status["detail"][key],
            "{key}"
        );
    }
    assert_eq!(harness.status["phase"], model.status["phase"]);
    assert_eq!(harness.row, model.row);
    assert!(harness.closed.contains(&reason) && model.closed.contains(&reason));
    // the `stop` trace rows differ by `errors` only: the harness's lists every attempt's
    assert_eq!(harness.stops.len(), 1, "{:?}", harness.stops);
    assert_eq!(model.stops.len(), 1, "{:?}", model.stops);
    assert_eq!(harness.stops[0]["reason"], model.stops[0]["reason"]);
    assert_eq!(harness.stops[0]["errors"].as_array().map(Vec::len), Some(n));
    assert!(model.stops[0].get("errors").is_none(), "{}", model.stops[0]);
}
