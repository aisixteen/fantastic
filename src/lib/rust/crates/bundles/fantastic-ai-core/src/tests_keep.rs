//! KEEP (#641) over the harness: a reply that will fold carries the `keep` call with its
//! id, the call stores a summary and is no action, and the summary stands in for the body
//! once the block folds.

use super::*;
use crate::constants::{AUTOFOLD_AT, KEEP_SUMMARY_MAX};

const DONE: &str = "<call name=\"done\"></call>";

async fn say(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) {
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": text, "client_id": "probe"}),
        )])
        .await;
}

async fn widen(kernel: &Arc<Kernel>, agent: &AgentId) {
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "context_window": 1_000_000}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
}

fn large_reply() -> String {
    format!(
        "[reply to 9 · notes read]\nhead line\n{}",
        "x".repeat(AUTOFOLD_AT * 4 + 4)
    )
}

/// The content of the last message of pass `n`.
fn last_message(s: &Arc<MockScript>, n: usize) -> String {
    let msgs = s.request_messages(n);
    msgs.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The id of the stored reply row.
async fn row_id_of(kernel: &Arc<Kernel>, agent: &AgentId) -> i64 {
    let stored = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("read");
    stored["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["content"].as_str() == Some(large_reply().as_str()))
        .expect("the reply row")["id"]
        .as_i64()
        .unwrap()
}

fn keep_call(id: i64, summary: &str) -> String {
    format!("<call name=\"keep\"><of>{id}</of><summary>{summary}</summary></call>")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_reply_that_will_fold_carries_the_keep_call_with_its_id() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "kp_mark").await;
    widen(&kernel, &agent).await;
    let s = script(&agent, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    say(&kernel, &agent, &large_reply()).await;
    assert!(until_passes(&s, 1).await);
    assert!(
        last_message(&s, 0)
            .ends_with("<call name=\"keep\"><of>9</of><summary>\u{2026}</summary></call>"),
        "the call is on the block"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "[reply to 10 \u{b7} notes read]\nsmall").await;
    assert!(until_passes(&s, 2).await);
    assert!(!last_message(&s, 1).contains("<call name=\"keep\""));
    // the stored row is the reply as it arrived
    let stored = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(&agent, &kernel, json!({"type": "read"})),
    )
    .await
    .expect("read");
    assert!(!stored["rows"].to_string().contains("keep"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_of_keep_calls_alone_is_a_turn_with_no_call() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "kp_alone").await;
    widen(&kernel, &agent).await;
    let s = script(
        &agent,
        vec![pass(keep_call(9, "A NOTE")), pass(DONE)],
        Duration::ZERO,
    );
    say(&kernel, &agent, &large_reply()).await;
    assert!(until_passes(&s, 2).await);
    assert!(
        last_message(&s, 1).contains("[no call was dispatched]"),
        "{}",
        last_message(&s, 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keep_beside_a_done_ends_the_chain() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "kp_done").await;
    widen(&kernel, &agent).await;
    let s = script(
        &agent,
        vec![pass(format!("{}\n{DONE}", keep_call(9, "A NOTE")))],
        Duration::ZERO,
    );
    say(&kernel, &agent, &large_reply()).await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_the_answer_the_summary_replaces_the_body_and_recall_returns_it_whole() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "kp_fold").await;
    widen(&kernel, &agent).await;
    let s = script(
        &agent,
        vec![
            pass(format!("{}\n{DONE}", keep_call(9, "WHAT-IS-NEEDED"))),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    say(&kernel, &agent, &large_reply()).await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "again").await;
    assert!(until_passes(&s, 2).await);
    let id = row_id_of(&kernel, &agent).await;
    let seen = s.request(1);
    assert!(
        seen.contains(&format!(
            "[folded · read reply from notes · ~16004 tokens · get: {id}]\\nWHAT-IS-NEEDED"
        )),
        "{}",
        &seen[seen.len().saturating_sub(900)..]
    );
    assert!(!seen.contains("head line"));
    assert!(!seen.contains(&"x".repeat(1000)));
    let r = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "recall", "id": id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["messages"][0]["content"], json!(large_reply()), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_cap_summary_folds_to_the_plain_anchor() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "kp_cap").await;
    widen(&kernel, &agent).await;
    let long = "y".repeat(KEEP_SUMMARY_MAX * 4 + 4);
    let s = script(
        &agent,
        vec![pass(format!("{}\n{DONE}", keep_call(9, &long))), pass(DONE)],
        Duration::ZERO,
    );
    say(&kernel, &agent, &large_reply()).await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "again").await;
    assert!(until_passes(&s, 2).await);
    let id = row_id_of(&kernel, &agent).await;
    let seen = s.request(1);
    assert!(
        seen.contains(&format!(
            "[folded · read reply from notes · ~16004 tokens · head: \\\"head line\\\" · get: {id}]"
        )),
        "{}",
        &seen[seen.len().saturating_sub(900)..]
    );
    assert!(!seen.contains(&format!("get: {id}]\\ny")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keep_for_an_unknown_id_is_refused_with_an_error() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "kp_unknown").await;
    let s = script(
        &agent,
        vec![
            pass(format!("{}\n{DONE}", keep_call(424_242, "note"))),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 2).await);
    assert!(
        last_message(&s, 1).contains("keep: there is no reply with id 424242"),
        "{}",
        last_message(&s, 1)
    );
}
