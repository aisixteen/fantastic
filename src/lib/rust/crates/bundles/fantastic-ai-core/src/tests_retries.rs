//! #198 part 2: a turn whose link dropped is retried, visibly. A child module of
//! `tests`, so it reaches the mock harness (`mk_kernel`, `script`, `pass`, ...).

use super::*;

/// A backend with `transport_retries` set to `retries`.
async fn mk_retrying(tmp: &TempDir, tag: &str, retries: i64) -> (Arc<Kernel>, AgentId) {
    let (kernel, agent) = mk_kernel(tmp, tag).await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "transport_retries": retries}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
    (kernel, agent)
}

/// One user message to the agent; the reply it gets back.
async fn send_text(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) -> Value {
    kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": text, "client_id": "c_retry"}),
        )])
        .await
        .remove(0)
        .result
}

#[tokio::test]
async fn a_dropped_link_is_retried_and_the_turn_answers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_answers", 2).await;
    let s = script(
        &agent,
        vec![
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass("ANSWER"),
        ],
        Duration::ZERO,
    );
    let _ = send_text(&kernel, &agent, "hello").await;
    assert!(s.passes_served() >= 2, "passes: {}", s.passes_served());
    assert!(s.request(1).contains("[retry 1/2]"), "{}", s.request(1));
}

#[tokio::test]
async fn retries_run_out_and_the_turn_fails() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_runout", 1).await;
    let s = script(
        &agent,
        vec![
            vec![ScriptEvent::TransportFail("connect failed".into())],
            vec![ScriptEvent::TransportFail("connect failed again".into())],
        ],
        Duration::ZERO,
    );
    let _ = send_text(&kernel, &agent, "hello").await;
    assert_eq!(s.passes_served(), 2);
    assert!(s.request(1).contains("[retry 1/1]"), "{}", s.request(1));
    assert!(!s.request(1).contains("[retry 2/"), "{}", s.request(1));
}

#[tokio::test]
async fn a_model_error_is_never_retried() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_model", 3).await;
    let s = script(
        &agent,
        vec![vec![ScriptEvent::Fail("boom".into())]],
        Duration::ZERO,
    );
    let _ = send_text(&kernel, &agent, "hello").await;
    assert_eq!(s.passes_served(), 1);
    assert!(!s.request(0).contains("[retry"), "{}", s.request(0));
}

#[tokio::test]
async fn retries_are_off_by_default() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "rt_default").await;
    let s = script(
        &agent,
        vec![
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass("ANSWER"),
        ],
        Duration::ZERO,
    );
    let _ = send_text(&kernel, &agent, "hello").await;
    assert_eq!(s.passes_served(), 1);
}
