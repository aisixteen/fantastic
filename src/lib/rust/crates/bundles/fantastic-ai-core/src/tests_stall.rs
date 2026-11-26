//! A turn that ends in a failure says so in the history (#177): a failed pass
//! stores an ERROR ROW, and the FIFO lock it held is released, so the queue moves
//! on.

use super::*;
use fantastic_kernel::inbox::InboxReceiver;
use fantastic_kernel::test_support::as_agent;

/// The rows the agent's own store holds, in order.
async fn stored_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

fn error_rows(rows: &[Value]) -> Vec<&Value> {
    rows.iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("error"))
        .collect()
}

/// A pass that FAILS (a stalled stream, say) ends its turn with an ERROR ROW: the
/// incoming message and a row naming the failure. The FIFO lock is released, so the
/// next message runs and reads the row (#533, #795).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_pass_stores_an_error_row_and_the_queue_moves_on() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_err_row").await;
    let reason = "mock_backend: stream_stalled — https://api.example/v1/messages sent no \
                  bytes for 0.2s; this turn is NOT complete";
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail(reason.into())],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let first = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "one", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(first["raised"], json!(true), "{first}");
    let rows = stored_rows(&kernel, &backend).await;
    let errors = error_rows(&rows);
    assert_eq!(errors.len(), 1, "{rows:#?}");
    let content = errors[0]["content"].as_str().unwrap_or_default();
    assert!(content.contains("stream_stalled"), "{content}");
    assert!(
        content.contains("nothing this turn meant to send"),
        "{content}"
    );
    assert!(
        rows.iter().any(|r| r["content"] == json!("one")),
        "{rows:#?}"
    );
    // the lock is free: the next message runs its own pass, and it reads the error row
    let second = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(second["response"], json!(r#"<call name="done"></call>"#), "{second}");
    assert_eq!(s.passes_served(), 2);
    assert!(s.request(1).contains("stream_stalled"), "{}", s.request(1));
}

/// Every status event the agent's inbox has taken in, as it arrives. Polls until
/// `done` says it, or the bound runs out — the bound is the failure, not a wait.
async fn events_until(
    rx: &mut InboxReceiver,
    seen: &mut Vec<Value>,
    mut done: impl FnMut(&[Value]) -> bool,
) {
    for _ in 0..500 {
        while let Some(v) = rx.try_recv() {
            seen.push(v.payload);
        }
        if done(seen) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the expected status never arrived: {seen:#?}");
}

fn status_of<'a>(events: &'a [Value], client: &str, phase: &str) -> Option<&'a Value> {
    events.iter().find(|e| {
        e.get("type").and_then(Value::as_str) == Some("status")
            && e.get("phase").and_then(Value::as_str) == Some(phase)
            && e.get("client_id").and_then(Value::as_str) == Some(client)
    })
}

const ENDPOINT: &str = "https://api.example/v1/messages";

/// THE READ CLOCK ENDS A STALLED TURN (#177): a pass whose stream goes silent fails
/// once its window passes, naming the stall. While that turn holds the FIFO lock,
/// a send that arrives is `queued`; it runs the moment the window ends the first
/// turn. The first turn's `status` is `done` with `reason='error'`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_send_waits_behind_a_stalled_turn_and_runs_when_the_window_ends() {
    // the send ceiling is process-wide: hold the lock a ceiling test takes, so this
    // turn runs under the default ceiling
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_stall_q").await;
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Stall {
                idle: Duration::from_millis(500),
                endpoint: ENDPOINT.into(),
            }],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let mut seen: Vec<Value> = Vec::new();
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let first = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "probe_a"}),
        )])
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the stalled pass never started");
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let second = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "probe_b"}),
        )])
        .await
        .remove(0)
        .result
    });
    // queued behind the stalled turn: no second pass has started, the first is still out
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe_b", "queued").is_some()
    })
    .await;
    assert_eq!(s.passes_served(), 1, "the queued send waited for the lock");
    assert!(
        !first.is_finished(),
        "the stalled turn still holds the lock"
    );
    // the window passes: the first turn fails visibly, and the queued send runs
    let first_out = first.await.expect("first send");
    assert_eq!(first_out["raised"], json!(true), "{first_out}");
    let second_out = second.await.expect("second send");
    assert!(second_out.get("raised").is_none(), "{second_out}");
    // ONE CHAIN PER AGENT (#227): the second send is held; the wake turn that
    // answers it runs once the stalled chain has ended.
    assert_eq!(s.passes_served(), 2, "the queued send ran");
    // status: the first call FAILED with reason='error', naming the stall
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe_a", "failed").is_some()
    })
    .await;
    let done_a = status_of(&seen, "probe_a", "failed").expect("failed");
    assert_eq!(done_a["detail"]["reason"], json!("error"), "{done_a}");
    let why = done_a["detail"]["error"].as_str().unwrap_or_default();
    assert!(why.contains("stream_stalled"), "{why}");
    assert!(why.contains(ENDPOINT), "{why}");
    // and the stored error row is what the next turn reads
    let rows = conversation(&kernel, &backend).await;
    let errors = error_rows(&rows);
    assert_eq!(errors.len(), 1, "{rows:#?}");
    assert!(
        errors[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("stream_stalled"),
        "{rows:#?}"
    );
    assert!(
        s.request(1).to_string().contains("[turn failed]"),
        "{}",
        s.request(1)
    );
}

// ─── a failed turn settles its sender and tells its watchers (#496) ─────────

/// A send whose turn outlives the ceiling settles with the kernel's refusal shape,
/// `reason: timeout` (a sender branching on `reason` settles on it), carrying the
/// same text the error row holds. The agent emits `done {reason: timeout}`, and the
/// turn is NOT retried: one pass served, however long the caller waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_turn_settles_its_sender_with_reason_timeout_and_is_not_retried() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.1");
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_to_reason").await;
    let s = script(
        &backend,
        vec![pass("never arrives"), pass("never arrives")],
        Duration::from_millis(800),
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let mut seen: Vec<Value> = Vec::new();
    let out = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["reason"], json!("timeout"), "{out}");
    assert_eq!(out["client_id"], json!("probe"), "{out}");
    let error = out["error"].as_str().unwrap_or_default();
    assert!(error.starts_with("send: timeout after 0.1s"), "{out}");
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe", "failed").is_some()
    })
    .await;
    let done = status_of(&seen, "probe", "failed").expect("failed");
    assert_eq!(done["detail"]["reason"], json!("timeout"), "{done}");
    let rows = stored_rows(&kernel, &backend).await;
    assert_eq!(error_rows(&rows).len(), 1, "{rows:#?}");
    // nothing retries it: the agent is idle and no second pass starts
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 1, "the failed turn was run again");
}

/// A provider failure that is not a timeout also emits `done {reason: error}` and
/// leaves the sender with an error reply (a raised failure, as ever), once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_provider_turn_settles_its_sender_and_emits_done_error_once() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_fail_done").await;
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail("boom".into())],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let mut seen: Vec<Value> = Vec::new();
    let out = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["raised"], json!(true), "{out}");
    assert!(out["error"].as_str().unwrap_or_default().contains("boom"));
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe", "failed").is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Some(v) = rx.try_recv() {
        seen.push(v.payload);
    }
    let dones: Vec<&Value> = seen
        .iter()
        .filter(|e| {
            e["type"] == json!("status")
                && e["phase"] == json!("failed")
                && e["client_id"] == json!("probe")
                && e["detail"]["reason"] == json!("error")
        })
        .collect();
    assert_eq!(dones.len(), 1, "{seen:#?}");
    assert_eq!(dones[0]["detail"]["reason"], json!("error"), "{dones:#?}");
    // nothing re-runs the failed turn (#795)
    assert_eq!(s.passes_served(), 1, "the failed turn was run again");
}

/// A message held behind a chain is answered by the pass that reads it. When that
/// pass times out, the held sender settles with the same `reason: timeout` reply
/// (under its own `client_id`), not a bare error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_sender_settles_with_reason_timeout_when_the_turn_that_reads_it_times_out() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.5");
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_held_to").await;
    let s = script(
        &backend,
        vec![pass("never arrives")],
        Duration::from_millis(1500),
    );
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let first = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "o"}),
        )])
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let second = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result
    });
    until_held(&backend, 1).await;
    let one = first.await.expect("first");
    assert_eq!(one["reason"], json!("timeout"), "{one}");
    let two = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the held sender was left waiting")
        .expect("second");
    assert_eq!(two["reason"], json!("timeout"), "{two}");
    assert_eq!(two["client_id"], json!("t"), "{two}");
}

/// A `send` call carrying its own `<timeout>`: the parent's wait outlasts the child's
/// send ceiling, so the child's refusal (not the parent's own deadline) is what the
/// parent is handed.
fn send_with_deadline(target: &str, text: &str, secs: u64) -> String {
    let mut args = Map::new();
    args.insert("target_id".into(), json!(target));
    args.insert("timeout".into(), json!(secs));
    args.insert("payload".into(), json!({"type": "send", "text": text}));
    crate::tool_parse::render_call("send", &args)
}

/// A parent that sent to a child and awaits its reply reads the child's failure:
/// the child's send settles with `reason: timeout` and the parent's next turn is
/// handed that reply, not silence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_awaiting_a_child_that_times_out_is_handed_the_timeout() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.5");
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "rt_parent_to").await;
    let child = add_peer(&kernel, &tmp, &parent).await;
    let cs = script(
        &child,
        vec![pass("never arrives")],
        Duration::from_millis(2500),
    );
    let ps = script(
        &parent,
        vec![
            pass(send_with_deadline(child.as_str(), "work", 5)),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let _ = kernel.operator_send(vec![Item::new(
            parent.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&cs, 1).await, "the child never started");
    assert!(
        until_passes(&ps, 2).await,
        "the parent was never handed the child's failure"
    );
    let seen = ps.request(1);
    assert!(
        seen.contains("[reply to ") && seen.contains("send: timeout after 0.5s"),
        "{seen}"
    );
}

/// A held sender whose answering pass fails on the provider settles with the
/// existing `transport_error` code under its own `client_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_sender_settles_with_transport_error_when_the_turn_that_reads_it_fails() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_held_fail").await;
    let s = script(
        &backend,
        // the first turn fails, and so does the wake turn that reads the held message
        vec![
            vec![ScriptEvent::Fail("boom".into())],
            vec![ScriptEvent::Fail("boom".into())],
        ],
        Duration::from_millis(1000),
    );
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let first = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "o"}),
        )])
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let second = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result
    });
    until_held(&backend, 1).await;
    let one = first.await.expect("first");
    assert_eq!(one["raised"], json!(true), "{one}");
    let two = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the held sender was left waiting")
        .expect("second");
    assert_eq!(two["reason"], json!("transport_error"), "{two}");
    assert_eq!(two["client_id"], json!("t"), "{two}");
    assert!(two["error"].as_str().unwrap_or_default().contains("boom"));
}

fn status_reason<'a>(events: &'a [Value], reason: &str) -> Option<&'a Value> {
    events.iter().find(|e| {
        e.get("type").and_then(Value::as_str) == Some("status")
            && e["detail"]["reason"] == json!(reason)
    })
}

/// A FAILED PASS ENDS VISIBLY, with no turn of its own (#795, Q4; was #533): a
/// provider failure past `transport_retries` stores its error row after the incoming
/// one, ends on `status(failed)` naming the error, and nothing more is asked — no
/// self-sent retry, no loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_pass_ends_with_its_error_row_and_no_turn_of_its_own() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_retry_once").await;
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail("mock_backend: boom".into())],
            // never served: a failed pass is not re-run as a turn
            pass(r#"RETRIED-ANSWER <call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let mut seen: Vec<Value> = Vec::new();
    let out = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["raised"], json!(true), "{out}");
    events_until(&mut rx, &mut seen, |ev| status_reason(ev, "error").is_some()).await;
    let failed = status_reason(&seen, "error").expect("a failed status");
    assert_eq!(failed["phase"], json!("failed"), "{failed}");
    assert!(
        failed["detail"]["error"].as_str().unwrap_or_default().contains("boom"),
        "{failed}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "a failed pass was re-run");
    let rows = stored_rows(&kernel, &backend).await;
    let at = |f: &dyn Fn(&Value) -> bool| rows.iter().position(f);
    let incoming = at(&|r| r["content"] == json!("go")).expect("incoming row");
    let error = at(&|r| r["kind"] == json!("error")).expect("error row");
    assert!(incoming < error, "{rows:#?}");
}

/// A pass cut at `max_tokens` (a provider error) is not silent: its error row and its
/// failed status name the cut (#533, #795).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_cut_at_max_tokens_fails_naming_the_cut() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_retry_cut").await;
    let s = script(
        &backend,
        vec![vec![ScriptEvent::Fail(
            "mock_backend: the turn was cut at max_tokens=16384 and is NOT complete".into(),
        )]],
        Duration::ZERO,
    );
    let out = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        out["error"].as_str().unwrap_or_default().contains("cut at max_tokens=16384"),
        "{out}"
    );
    assert_eq!(s.passes_served(), 1);
    let rows = stored_rows(&kernel, &backend).await;
    let errors = error_rows(&rows);
    assert!(
        errors.iter().any(|r| r["content"]
            .as_str()
            .is_some_and(|c| c.contains("cut at max_tokens=16384"))),
        "{rows:#?}"
    );
}

/// A cancelled turn is not retried (#533).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_turn_is_not_retried() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_retry_cancel").await;
    let s = script(
        &backend,
        vec![
            pass(r#"<call name="done"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::from_millis(500),
    );
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let first = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the pass never started");
    let _ = kernel.operator_send(vec![Item::new(backend.clone(), json!({"type": "stop"}))])
        .await;
    let _ = first.await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(s.passes_served(), 1, "a cancelled turn was retried");
    let rows = stored_rows(&kernel, &backend).await;
    assert!(error_rows(&rows).is_empty(), "{rows:#?}");
}

/// A message held while the chain runs is answered by the one pass that reads it after
/// the chain's pass failed (#533, #795).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_message_is_answered_after_the_failed_pass() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_retry_held").await;
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail("mock_backend: boom".into())],
            pass(r#"HELD-ANSWER <call name="done"></call>"#),
        ],
        Duration::from_millis(400),
    );
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let first = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "probe_a"}),
        )])
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let second = tokio::spawn(async move {
        k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "probe_b"}),
        )])
        .await
        .remove(0)
        .result
    });
    until_held(&backend, 1).await;
    let first_out = first.await.expect("first send");
    assert_eq!(first_out["raised"], json!(true), "{first_out}");
    let second_out = second.await.expect("second send");
    assert!(second_out.get("error").is_none(), "{second_out}");
    assert!(
        second_out["response"]
            .as_str()
            .unwrap_or_default()
            .contains("HELD-ANSWER"),
        "{second_out}"
    );
    assert_eq!(s.passes_served(), 2);
}
