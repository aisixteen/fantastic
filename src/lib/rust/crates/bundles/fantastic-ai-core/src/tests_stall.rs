//! A pass that ends in a failure says so in the history (#177): a failed pass
//! stores an ERROR ROW and says so in its `failed` status; the messages it did not
//! read are read by the next pass (#795: they were acked when they were stored).

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

/// A pass that FAILS (a stalled stream, say) ends with an ERROR ROW after the message it
/// read, naming the failure; the next message runs its own pass and reads the row
/// (#533, #795).
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
    let first = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "one", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(first["accepted"], json!(true), "{first}");
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
    // the next message runs its own pass, and it reads the error row
    let second = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(json!(crate::tests::last_answer(&kernel, &backend).await), json!(r#"<call name="done"></call>"#), "{second}");
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
    // EVENT-DRIVEN: each event wakes the check; the bound only FAILS a hang
    let wait = async {
        loop {
            while let Some(v) = rx.try_recv() {
                seen.push(v.payload);
            }
            if done(seen) {
                return true;
            }
            match rx.recv().await {
                Some(v) => seen.push(v.payload),
                None => return done(seen),
            }
        }
    };
    let got = tokio::time::timeout(Duration::from_secs(60), wait).await.unwrap_or(false);
    assert!(got, "the expected status never arrived: {seen:#?}");
}

fn status_of<'a>(events: &'a [Value], client: &str, phase: &str) -> Option<&'a Value> {
    events.iter().find(|e| {
        e.get("type").and_then(Value::as_str) == Some("status")
            && e.get("phase").and_then(Value::as_str) == Some(phase)
            && e.get("client_id").and_then(Value::as_str) == Some(client)
    })
}

const ENDPOINT: &str = "https://api.example/v1/messages";

/// THE READ CLOCK ENDS A STALLED PASS (#177): a pass whose stream goes silent fails
/// once its window passes, naming the stall. A message that arrives meanwhile is
/// stored and acked at once (#795); it is read by the next pass, the moment the window
/// ends the first. The first pass's `status` is `failed` with `reason='error'`.
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
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "probe_a"}),
        )]))
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the stalled pass never started");
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let second = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "probe_b"}),
        )]))
        .await
        .remove(0)
        .result
    });
    // stored behind the stalled pass: no second pass has started, the first is still out
    until_unread(&backend, 1).await;
    assert_eq!(s.passes_served(), 1, "no pass beside the stalled one");
    assert!(crate::state::is_generating(&backend), "the stalled pass is still out");
    // the window passes: the first pass fails visibly, and the message is read
    let first_out = first.await.expect("first send");
    assert_eq!(first_out["accepted"], json!(true), "{first_out}");
    let second_out = second.await.expect("second send");
    assert_eq!(second_out["accepted"], json!(true), "{second_out}");
    assert_eq!(s.passes_served(), 2, "the message was read");
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

/// A pass that outlives the ceiling fails with `status(failed, reason: timeout)` and an
/// error row naming the ceiling, and it is NOT retried: one pass served, however long
/// one waits. The message was acked when it was stored (#795).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_pass_says_timeout_and_is_not_retried() {
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
    let out = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(out["accepted"], json!(true), "{out}");
    assert_eq!(out["client_id"], json!("probe"), "{out}");
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe", "failed").is_some()
    })
    .await;
    let done = status_of(&seen, "probe", "failed").expect("failed");
    assert_eq!(done["detail"]["reason"], json!("timeout"), "{done}");
    let rows = stored_rows(&kernel, &backend).await;
    let errors = error_rows(&rows);
    assert_eq!(errors.len(), 1, "{rows:#?}");
    assert!(
        errors[0]["content"].as_str().unwrap_or_default().contains("send: timeout after 0.1s"),
        "{rows:#?}"
    );
    // nothing retries it: the agent is idle and no second pass starts
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 1, "the failed turn was run again");
}

/// A provider failure that is not a timeout emits `failed {reason: error}` once, and its
/// error row names it; the sender had its ack (#795).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_provider_pass_emits_failed_error_once() {
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
    let out = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(out["accepted"], json!(true), "{out}");
    let rows = stored_rows(&kernel, &backend).await;
    assert!(
        error_rows(&rows).iter().any(|r| r["content"].as_str().unwrap_or_default().contains("boom")),
        "{rows:#?}"
    );
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe", "failed").is_some()
    })
    .await;
    // the agent is quiet (`settled`): every status its one pass emitted is queued
    // already, so a second `failed` would be here now
    assert!(crate::tests::settle(&kernel).await, "the agent never went quiet");
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

/// A message that lands during a pass that times out was acked at once (#795): no sender
/// waits on the pass. It is read by the next pass, which times out too here; each failed
/// pass leaves its error row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_during_a_timed_out_pass_is_read_by_the_next_pass() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.5");
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_held_to").await;
    let s = script(
        &backend,
        vec![pass("never arrives"), pass("never arrives")],
        Duration::from_millis(1500),
    );
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let first = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "o"}),
        )]))
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let second = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "t"}),
        )]))
        .await
        .remove(0)
        .result
    });
    until_unread(&backend, 1).await;
    let one = first.await.expect("first");
    assert_eq!(one["accepted"], json!(true), "{one}");
    let two = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the sender was left waiting")
        .expect("second");
    assert_eq!(two["accepted"], json!(true), "{two}");
    assert_eq!(two["client_id"], json!("t"), "{two}");
    assert_eq!(s.passes_served(), 2, "the message was read by the next pass");
    let rows = stored_rows(&kernel, &backend).await;
    assert_eq!(error_rows(&rows).len(), 2, "{rows:#?}");
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

/// A parent that sends to a child AI is handed the child's ACK at once (#795), however
/// long the child's pass takes; the child's timed-out pass is the child's own, said in
/// its error row. Nothing of the parent waits on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_sending_to_a_child_that_times_out_gets_its_ack_at_once() {
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
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            parent.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&cs, 1).await, "the child never started");
    let replies = reply_rows(&kernel, &parent).await;
    assert!(
        replies.iter().any(|r| r.contains("<accepted>true</accepted>")),
        "the parent holds the child's ack: {replies:#?}"
    );
    assert_eq!(ps.passes_served(), 1, "an ack wakes nothing");
    let rows = stored_rows(&kernel, &child).await;
    assert!(
        error_rows(&rows).iter().any(|r| r["content"].as_str().unwrap_or_default().contains("send: timeout after 0.5s")),
        "{rows:#?}"
    );
}

/// A message that lands during a pass that fails on the provider was acked at once
/// (#795); the next pass reads it, and fails too here: each failure is its own error row
/// naming the provider's words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_during_a_failed_pass_is_read_by_the_next_pass() {
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
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "o"}),
        )]))
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let second = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "t"}),
        )]))
        .await
        .remove(0)
        .result
    });
    until_unread(&backend, 1).await;
    let one = first.await.expect("first");
    assert_eq!(one["accepted"], json!(true), "{one}");
    let two = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the sender was left waiting")
        .expect("second");
    assert_eq!(two["accepted"], json!(true), "{two}");
    assert_eq!(two["client_id"], json!("t"), "{two}");
    assert_eq!(s.passes_served(), 2, "the message was read by the next pass");
    let rows = stored_rows(&kernel, &backend).await;
    let errors = error_rows(&rows);
    assert_eq!(errors.len(), 2, "{rows:#?}");
    assert!(errors.iter().all(|r| r["content"].as_str().unwrap_or_default().contains("boom")));
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
    let out = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(out["accepted"], json!(true), "{out}");
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
    let out = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(out["accepted"], json!(true), "{out}");
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
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
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
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "probe_a"}),
        )]))
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let second = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "two", "client_id": "probe_b"}),
        )]))
        .await
        .remove(0)
        .result
    });
    until_unread(&backend, 1).await;
    let first_out = first.await.expect("first send");
    assert_eq!(first_out["accepted"], json!(true), "{first_out}");
    let second_out = second.await.expect("second send");
    assert!(second_out.get("error").is_none(), "{second_out}");
    assert!(
        crate::tests::last_answer(&kernel, &backend).await.contains("HELD-ANSWER"),
        "{second_out}"
    );
    assert_eq!(s.passes_served(), 2);
}
