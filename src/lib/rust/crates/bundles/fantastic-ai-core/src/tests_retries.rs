//! #198 part 2: a turn whose link dropped is retried, visibly. A child module of
//! `tests`, so it reaches the mock harness (`mk_kernel`, `script`, `pass`, ...).

use super::*;

/// A backend with `transport_retries` set to `retries`.
async fn mk_retrying(tmp: &TempDir, tag: &str, retries: i64) -> (Arc<Kernel>, AgentId) {
    let (kernel, agent) = mk_kernel(tmp, tag).await;
    let r = kernel.operator_send(vec![Item::new(
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
    kernel.operator_send(vec![Item::new(
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
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let _ = send_text(&kernel, &agent, "hello").await;
    // the link retry ran out after 2 passes; the failed turn's own retry is the third (#533)
    assert!(until_passes(&s, 3).await, "passes: {}", s.passes_served());
    assert!(s.request(1).contains("[retry 1/1]"), "{}", s.request(1));
    assert!(!s.request(1).contains("[retry 2/"), "{}", s.request(1));
}

#[tokio::test]
async fn a_model_error_is_never_retried() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_model", 3).await;
    let s = script(
        &agent,
        vec![
            vec![ScriptEvent::Fail("boom".into())],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let _ = send_text(&kernel, &agent, "hello").await;
    // no link retry: the second pass is the failed turn's own retry turn (#533)
    assert!(until_passes(&s, 2).await);
    assert!(!s.request(0).contains("[retry"), "{}", s.request(0));
    assert!(!s.request(1).contains("[retry"), "{}", s.request(1));
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
    // no link retry: the second pass is the failed turn's own retry turn (#533)
    assert!(until_passes(&s, 2).await);
    assert!(!s.request(1).contains("[retry"), "{}", s.request(1));
}

/// A backend with `misfire_limit` set too.
async fn mk_bounded(
    tmp: &TempDir,
    tag: &str,
    retries: i64,
    misfire: i64,
) -> (Arc<Kernel>, AgentId) {
    let (kernel, agent) = mk_retrying(tmp, tag, retries).await;
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "misfire_limit": misfire}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
    (kernel, agent)
}

/// Every status that ends a call, in order.
fn ending_statuses(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status")
            && ["done", "failed", "stalled", "awaiting"]
                .contains(&p["phase"].as_str().unwrap_or_default())
        {
            out.push(p);
        }
    }
    out
}

/// A REFUSED KEY IS ASKED ONCE: a 401 is never retried on the link, never nudged as a
/// failed turn (#533); the chain gives up at once with the endpoint's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_is_one_request_and_the_chain_gives_up() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_bounded(&tmp, "rt_401", 3, 10).await;
    let s = script(
        &agent,
        (0..12).map(|_| vec![ScriptEvent::HttpFail(401)]).collect(),
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    let r = send_text(&kernel, &agent, "hello").await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 1, "a 401 is asked once");
    assert!(
        r.to_string().contains("HTTP 401"),
        "the send fails with the endpoint's error: {r}"
    );
    let ends = ending_statuses(&mut rx);
    let last = ends.last().expect("an ending status");
    assert_eq!(last["phase"], json!("failed"), "{ends:#?}");
    assert_eq!(last["detail"]["reason"], json!("gave_up"), "{ends:#?}");
    assert!(
        last["detail"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("HTTP 401"),
        "the give-up names the error: {last:#?}"
    );
}

/// A 404 (an unknown model) is permanent too: one request, the chain gives up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_is_one_request_and_the_chain_gives_up() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_bounded(&tmp, "rt_404", 3, 10).await;
    let s = script(
        &agent,
        (0..12).map(|_| vec![ScriptEvent::HttpFail(404)]).collect(),
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    let _ = send_text(&kernel, &agent, "hello").await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 1, "a 404 is asked once");
    let ends = ending_statuses(&mut rx);
    assert_eq!(
        ends.last().map(|l| l["detail"]["reason"].clone()),
        Some(json!("gave_up")),
        "{ends:#?}"
    );
}

/// A 503 IS TRANSIENT and its retries are BOUNDED: `1 + transport_retries` requests a
/// turn, `misfire_limit` failed turns, then the chain gives up — nothing more is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_503_is_retried_within_its_bounds() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_bounded(&tmp, "rt_503", 1, 2).await;
    let s = script(
        &agent,
        (0..12).map(|_| vec![ScriptEvent::HttpFail(503)]).collect(),
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    let _ = send_text(&kernel, &agent, "hello").await;
    // (1 + transport_retries) x misfire_limit
    assert!(until_passes(&s, 4).await, "passes: {}", s.passes_served());
    assert!(s.request(1).contains("[retry 1/1]"), "{}", s.request(1));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.passes_served(), 4, "the bounds stop the loop");
    // The give-up ends the chain once; the delivery of the last retry turn then
    // reports that turn's own raise (`reason: error`) after it.
    let ends = ending_statuses(&mut rx);
    let gave_up: Vec<&Value> = ends
        .iter()
        .filter(|l| l["detail"]["reason"] == json!("gave_up"))
        .collect();
    assert_eq!(gave_up.len(), 1, "{ends:#?}");
    assert_eq!(gave_up[0]["detail"]["silent_turns"], json!(2), "{ends:#?}");
}

/// A key that cannot be read is permanent, as a refused one.
#[test]
fn a_credential_error_is_permanent() {
    let e: ProviderError =
        crate::credentials::CredentialError("creds/vllm: no agent".into()).into();
    assert_eq!(
        e.permanent_code(),
        Some(fantastic_kernel::refusal::UNAUTHORIZED)
    );
}
