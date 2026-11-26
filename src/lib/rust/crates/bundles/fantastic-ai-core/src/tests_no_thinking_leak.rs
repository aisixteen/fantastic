//! Thinking never leaves the agent that produced it: a peer's `send` reply, a
//! reply that wakes an agent after its `done` and the delivered reply row carry its answer only, never its
//! `<think>` text nor its rows' stored `thinking`; the producer's own `recall` of
//! its own row still returns it (E37: a solver read a peer's deliberation off a
//! send reply).
//!
//! A child of `tests`, so it drives the same harness: the mock provider, a real
//! `ram_store` and `mk_kernel` backends.

use super::*;

const DONE: &str = r#"<call name="done"></call>"#;

/// Every row of `agent`'s store, as one string.
async fn stored(kernel: &Arc<Kernel>, agent: &AgentId) -> String {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].to_string()
}

/// Start `agent`'s run as the operator.
async fn start(kernel: &Arc<Kernel>, agent: &AgentId) {
    let _ = crate::tests::settled(kernel, kernel
        .operator_send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await;
}

/// The asker sends `payload` to `target` (the reply comes back to it).
fn ask(target: &AgentId, payload: Value) -> String {
    call_envelope(target.as_str(), payload, None)
}

/// An asker sends its peer a message (and ends with `done`, when `done`); the peer
/// thinks, then answers with its own `send` back (#795: the model decides to answer).
/// The asker's next request and its stored rows hold the peer's answer, and none of the
/// peer's thinking.
async fn a_peer_answer_reaches_the_asker_without_its_thinking(tag: &str, done: bool) {
    let tmp = TempDir::new().unwrap();
    let (kernel, peer) = mk_kernel(&tmp, tag).await;
    let asker = add_peer(&kernel, &tmp, &peer).await;
    let question = ask(&peer, json!({"type": "send", "text": "how many lamps?"}));
    let first = if done {
        format!("{question}\n{DONE}")
    } else {
        question
    };
    let a = script(&asker, vec![pass(first), pass(DONE)], Duration::ZERO);
    let answer = ask(&asker, json!({"type": "send", "text": "Four lamps."}));
    script(
        &peer,
        vec![pass(format!(
            "<think>PEER-SECRET-PLAN</think>{answer}\n{DONE}"
        ))],
        Duration::ZERO,
    );
    start(&kernel, &asker).await;
    assert!(until_passes(&a, 2).await, "the reply never came back");
    let seen = a.request(1);
    assert!(seen.contains("Four lamps."), "{seen}");
    assert!(!seen.contains("PEER-SECRET-PLAN"), "{seen}");
    assert!(!seen.contains(crate::think::THINK_OPEN), "{seen}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = stored(&kernel, &asker).await;
    assert!(rows.contains("Four lamps."), "{rows}");
    assert!(!rows.contains("PEER-SECRET-PLAN"), "{rows}");
    // the peer keeps its own thinking, on its own row
    let own = stored(&kernel, &peer).await;
    assert!(own.contains("\"thinking\":\"PEER-SECRET-PLAN\""), "{own}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peers_send_reply_holds_no_thinking() {
    a_peer_answer_reaches_the_asker_without_its_thinking("nl_send", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_reply_after_done_holds_no_thinking() {
    a_peer_answer_reaches_the_asker_without_its_thinking("nl_done", true).await;
}

/// The send reply itself, as its caller gets it: the answer, no `<think>` span.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_send_reply_is_the_answer_only() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "nl_reply").await;
    script(
        &agent,
        vec![pass(format!("<think>DEEP-THOUGHT</think>The answer.\n{DONE}"))],
        Duration::ZERO,
    );
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(json!(crate::tests::last_answer(&kernel, &agent).await), json!(format!("The answer.\n{DONE}")), "{r}");
}

/// A peer's ROWS read through `recall`: the content comes back, the stored
/// `thinking` does not — neither in the asker's next request nor in its rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peers_recalled_row_holds_no_thinking() {
    let tmp = TempDir::new().unwrap();
    let (kernel, peer) = mk_kernel(&tmp, "nl_recall").await;
    let asker = add_peer(&kernel, &tmp, &peer).await;
    let id = put_row(
        &kernel,
        &peer,
        json!({"role": "assistant", "content": "the peer answer",
               "thinking": "PEER-ROW-THOUGHT", "thinking_cut": 3}),
    )
    .await;
    let a = script(
        &asker,
        vec![pass(ask(&peer, json!({"type": "recall", "id": id}))), pass(DONE)],
        Duration::ZERO,
    );
    start(&kernel, &asker).await;
    assert!(until_passes(&a, 2).await, "the reply never came back");
    let seen = a.request(1);
    assert!(seen.contains("the peer answer"), "{seen}");
    assert!(!seen.contains("PEER-ROW-THOUGHT"), "{seen}");
    assert!(!seen.contains("thinking_cut"), "{seen}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = stored(&kernel, &asker).await;
    assert!(rows.contains("the peer answer"), "{rows}");
    assert!(!rows.contains("PEER-ROW-THOUGHT"), "{rows}");
}

/// The producer's own `recall` of its own row returns its thinking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_producers_own_recall_returns_its_thinking() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "nl_own").await;
    let id = put_row(
        &kernel,
        &agent,
        json!({"role": "assistant", "content": "my answer", "thinking": "MY-OWN-THOUGHT"}),
    )
    .await;
    let s = script(
        &agent,
        vec![
            // the owed recall of a store written by hand is a pass of its own
            pass("ok"),
            pass(call_envelope(
                agent.as_str(),
                json!({"type": "recall", "id": id}),
                None,
            )),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    start(&kernel, &agent).await;
    assert!(until_passes(&s, 3).await, "the recall reply never came back");
    let seen = s.request(2);
    assert!(seen.contains("MY-OWN-THOUGHT"), "{seen}");
}
