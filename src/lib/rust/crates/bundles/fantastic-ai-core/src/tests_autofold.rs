//! Autofold (#556) over the harness: a thought, a large reply and a recall reply
//! show as anchors in the next request, the store keeps every row whole, and `recall`
//! with an id returns a row whole.

use super::*;
use crate::constants::AUTOFOLD_AT;

/// Every row of the agent's store.
async fn rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// One send to the agent.
async fn say(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) {
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": text, "client_id": "probe"}),
        )])
        .await;
}

/// One verb on the agent; its result.
async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// A window wide enough that a 16k-token reply is delivered inline.
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

/// A reply text with a body just over [`AUTOFOLD_AT`] tokens.
fn large_reply() -> String {
    format!(
        "[reply to 9 · notes read]\nhead line\n{}",
        "x".repeat(AUTOFOLD_AT * 4 + 4)
    )
}

const DONE: &str = "<call name=\"done\"></call>";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_folds_to_its_anchor_from_the_next_request() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "af_think").await;
    let s = script(
        &agent,
        vec![
            pass(format!("<think>SECRET-PLAN</think>Answer one.\n{DONE}")),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "again").await;
    assert!(until_passes(&s, 2).await);
    let seen = s.request(1);
    assert!(seen.contains("[thought ~"), "{seen}");
    assert!(seen.contains("Answer one."), "{seen}");
    assert!(!seen.contains("SECRET-PLAN"), "{seen}");
    let kept = rows(&kernel, &agent).await;
    let row = kept
        .iter()
        .find(|r| r["role"] == json!("assistant"))
        .expect("assistant row");
    assert_eq!(row["thinking"], json!("SECRET-PLAN"), "{kept:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_reply_is_whole_the_turn_it_arrives_and_an_anchor_from_the_next() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "af_large").await;
    widen(&kernel, &agent).await;
    let s = script(
        &agent,
        vec![pass(format!("Noted.\n{DONE}")), pass(DONE)],
        Duration::ZERO,
    );
    let text = large_reply();
    say(&kernel, &agent, &text).await;
    assert!(until_passes(&s, 1).await);
    assert!(s.request(0).contains(&"x".repeat(1000)), "whole on arrival");
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "next").await;
    assert!(until_passes(&s, 2).await);
    let kept = rows(&kernel, &agent).await;
    let stored = kept
        .iter()
        .find(|r| r["content"].as_str() == Some(text.as_str()))
        .expect("the store row is whole");
    let seen = s.request(1);
    assert!(
        seen.contains(&format!(
            "[folded · read reply from notes · ~16004 tokens · head: \\\"head line\\\" · get: {}]",
            stored["id"]
        )),
        "{}",
        &seen[seen.len().saturating_sub(900)..]
    );
    assert!(!seen.contains(&"x".repeat(1000)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_by_id_returns_the_row_whole_with_its_thinking() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "af_recall_id").await;
    let id = put_row(
        &kernel,
        &agent,
        json!({"role": "assistant", "content": "the answer", "thinking": "the deep thought"}),
    )
    .await;
    for given in [json!(id)] {
        let r = verb(&kernel, &agent, json!({"type": "recall", "id": given})).await;
        assert_eq!(
            r,
            json!({"messages": [{"index": 0, "role": "assistant", "content": "the answer",
                                  "thinking": "the deep thought"}],
                   "total": 1, "truncated": false}),
        );
    }
    let r = verb(&kernel, &agent, json!({"type": "recall", "id": 99999})).await;
    assert_eq!(r["reason"], json!("not_found"), "{r}");
    assert_eq!(r["arg"], json!("id"), "{r}");
    let r = verb(&kernel, &agent, json!({"type": "recall", "id": "x"})).await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert_eq!(r["arg"], json!("id"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recall_reply_refolds_after_the_next_answer() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "af_refold").await;
    widen(&kernel, &agent).await;
    let b = put_row(
        &kernel,
        &agent,
        json!({"role": "user", "content": large_reply()}),
    )
    .await;
    put_row(
        &kernel,
        &agent,
        json!({"role": "assistant", "content": "seen"}),
    )
    .await;
    let recall = call_envelope(agent.as_str(), json!({"type": "recall", "id": b}), None);
    let s = script(
        &agent,
        vec![
            // the owed recall of a store written by hand is a pass of its own
            pass("ok"),
            pass(recall),
            pass(format!("Noted.\n{DONE}")),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 3).await);
    let last = s.request_messages(2).pop().unwrap();
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains(&"x".repeat(1000)),
        "the recalled row is whole the turn it arrives: {}",
        &last["content"].as_str().unwrap()[..400]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "next").await;
    assert!(until_passes(&s, 4).await);
    let seen = s.request(3);
    assert!(
        seen.contains(&format!(
            "[folded · recall reply from {} · id {b} · ~",
            agent.as_str()
        )),
        "{}",
        &seen[seen.len().saturating_sub(900)..]
    );
    assert!(!seen.contains(&"x".repeat(1000)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reflect_reply_never_folds() {
    use crate::constants::REFERENCE_FOLD_AT;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "af_reflect").await;
    widen(&kernel, &agent).await;
    let s = script(
        &agent,
        vec![pass(format!("Noted.\n{DONE}")), pass(DONE)],
        Duration::ZERO,
    );
    let text = format!(
        "[reply to 9 · notes reflect]\nhead line\n{}",
        "x".repeat(REFERENCE_FOLD_AT * 4 + 4)
    );
    say(&kernel, &agent, &text).await;
    assert!(until_passes(&s, 1).await);
    assert!(s.request(0).contains(&"x".repeat(1000)), "whole on arrival");
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&kernel, &agent, "next").await;
    assert!(until_passes(&s, 2).await);
    let seen = s.request(1);
    assert!(
        seen.contains(&"x".repeat(1000)),
        "still whole after it is answered"
    );
    assert!(
        !seen.contains("[folded · reflect reply"),
        "a reflect never folds"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reflect_reply_carrying_an_html_view_reaches_the_model_without_the_body() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register(
            crate::chat_bundle::HANDLER_MODULE,
            crate::chat_bundle::AiChatBundle,
        );
    })
    .await;
    let backend_id = id_for(&tmp, "af_viewhide");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(
        &kernel,
        &file_id,
        &tmp.path().join(".fantastic"),
        &backend_id,
    )
    .await;
    let agent = add_backend(&kernel, &backend_id, &file_id).await;
    let made = verb(
        &kernel,
        &AgentId::from("kernel_state"),
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
               "id": "viewchat", "callable_by": {agent.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], "viewchat", "{made}");
    let r = verb(
        &kernel,
        &AgentId::from("kernel_state"),
        json!({"type": "update_agent", "id": agent.as_str(),
               "can_call": {"$self": "*", file_id.as_str(): "*", "cli": "*", MEMORY: "*", "viewchat": "*"}}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let whole = verb(
        &kernel,
        &AgentId::from("viewchat"),
        json!({"type": "reflect"}),
    )
    .await;
    let size = whole["view"]["bytes"]
        .as_str()
        .expect("the view body")
        .chars()
        .count();
    let call = call_envelope("viewchat", json!({"type": "reflect"}), Some("self"));
    let s = script(&agent, vec![pass(call), pass(DONE)], Duration::ZERO);
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 2).await);
    let seen = s.request(1);
    assert!(
        seen.contains(&format!("<view>html, {size} chars, not shown</view>")),
        "{seen}"
    );
    assert!(
        !seen.contains("<!DOCTYPE") && !seen.contains("<html"),
        "{seen}"
    );
}
