//! A turn that ends in a failure says so in the history (#177): a failed pass
//! stores an ERROR ROW, and the FIFO lock it held is released, so the queue moves
//! on.

use super::*;
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
