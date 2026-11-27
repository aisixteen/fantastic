//! #198 part 2: a pass whose link dropped is retried, visibly. A child module of
//! `tests`, so it reaches the mock harness (`mk_kernel`, `script`, `pass`, ...).

use super::*;

/// A backend with `transport_retries` set to `retries`.
async fn mk_retrying(tmp: &TempDir, tag: &str, retries: i64) -> (Arc<Kernel>, AgentId) {
    let (kernel, agent) = mk_kernel(tmp, tag).await;
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "transport_retries": retries}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
    (kernel, agent)
}

/// One user message to the agent; its ack, read once the agent is quiet.
async fn send_text(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) -> Value {
    crate::tests::settled(kernel, fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": text, "client_id": "c_retry"}),
        )]))
        .await
        .remove(0)
        .result
}

#[tokio::test]
async fn a_dropped_link_is_retried_and_the_pass_answers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_answers", 2).await;
    let s = script(
        &agent,
        vec![
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass(r#"ANSWER <call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let r = send_text(&kernel, &agent, "hello").await;
    assert_eq!(json!(crate::tests::last_answer(&kernel, &agent).await), json!(r#"ANSWER <call name="done"></call>"#), "{r}");
    assert_eq!(s.passes_served(), 2);
    assert!(s.request(1).contains("[retry 1/2]"), "{}", s.request(1));
}

#[tokio::test]
async fn retries_run_out_and_the_pass_fails() {
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
    let r = send_text(&kernel, &agent, "hello").await;
    // the link retry ran out after 2 requests, and the pass failed: nothing re-runs it (#795)
    assert_eq!(r["accepted"], json!(true), "{r}");
    let errors = crate::tests::error_rows(&kernel, &agent).await;
    assert!(errors.iter().any(|e| e.contains("connect failed again")), "{errors:?}");
    assert!(s.request(1).contains("[retry 1/1]"), "{}", s.request(1));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 2, "the bounds stop it");
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
    let r = send_text(&kernel, &agent, "hello").await;
    // no link retry, and no retry pass (#795): one request, the pass failed
    assert_eq!(r["accepted"], json!(true), "{r}");
    let errors = crate::tests::error_rows(&kernel, &agent).await;
    assert!(errors.iter().any(|e| e.contains("boom")), "{errors:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "a model error is never retried");
}

#[tokio::test]
async fn retries_are_off_by_default() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "rt_default").await;
    let s = script(
        &agent,
        vec![
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass(r#"ANSWER <call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let r = send_text(&kernel, &agent, "hello").await;
    // no link retry by default: one request, the pass failed (#795)
    assert_eq!(r["accepted"], json!(true), "{r}");
    let errors = crate::tests::error_rows(&kernel, &agent).await;
    assert!(errors.iter().any(|e| e.contains("connect failed")), "{errors:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "retries are off by default");
}

/// Every status that ends a call, in order.
fn ending_statuses(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status")
            && ["done", "failed"]
                .contains(&p["phase"].as_str().unwrap_or_default())
        {
            out.push(p);
        }
    }
    out
}

/// A REFUSED KEY IS ASKED ONCE: a 401 is never retried on the link (#533); the chain
/// gives up at once with the endpoint's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_is_one_request_and_the_chain_gives_up() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_401", 3).await;
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
    assert_eq!(r["accepted"], json!(true), "{r}");
    let errors = crate::tests::error_rows(&kernel, &agent).await;
    assert!(
        errors.iter().any(|e| e.contains("HTTP 401")),
        "the pass fails with the endpoint's error: {errors:?}"
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
    let (kernel, agent) = mk_retrying(&tmp, "rt_404", 3).await;
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

/// A 503 IS TRANSIENT and its retries are BOUNDED: `1 + transport_retries` requests,
/// then the pass fails (#795: no retry pass of its own) — nothing more is sent, and the
/// call ends `failed` naming the endpoint's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_503_is_retried_within_its_bounds() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_retrying(&tmp, "rt_503", 1).await;
    let s = script(
        &agent,
        (0..12).map(|_| vec![ScriptEvent::HttpFail(503)]).collect(),
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    let _ = send_text(&kernel, &agent, "hello").await;
    // 1 + transport_retries
    assert!(until_passes(&s, 2).await, "passes: {}", s.passes_served());
    assert!(s.request(1).contains("[retry 1/1]"), "{}", s.request(1));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.passes_served(), 2, "the bounds stop the loop");
    let ends = ending_statuses(&mut rx);
    let last = ends.last().expect("an ending status");
    assert_eq!(last["phase"], json!("failed"), "{ends:#?}");
    assert!(
        last["detail"]["error"].as_str().unwrap_or_default().contains("HTTP 503"),
        "{ends:#?}"
    );
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
