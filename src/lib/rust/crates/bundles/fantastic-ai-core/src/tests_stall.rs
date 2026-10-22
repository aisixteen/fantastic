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
/// incoming message and a row naming the failure. The FIFO lock is released, so
/// the next send runs at once and reads the row.
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
    let first = kernel
        .send(vec![Item::new(
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
    // the queue moves on: the next send is the next pass, and it reads the failure
    let second = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(second.get("raised").is_none(), "{second}");
    assert_eq!(s.passes_served(), 2, "the next send ran");
    let seen = s.request(1);
    assert!(
        seen.contains("two") && seen.contains("[turn failed]"),
        "{seen}"
    );
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
        k.send(vec![Item::new(
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
        k.send(vec![Item::new(
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
    assert!(until_passes(&s, 2).await, "the held send ran");
    // status: the first turn is done with reason='error', naming the stall
    events_until(&mut rx, &mut seen, |ev| {
        status_of(ev, "probe_a", "done").is_some()
    })
    .await;
    let done_a = status_of(&seen, "probe_a", "done").expect("done");
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
