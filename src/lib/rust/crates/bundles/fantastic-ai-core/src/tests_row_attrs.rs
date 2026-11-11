//! What a row carries about itself (#618): its `attributes` map: `ts` on every row,
//! `usage` and `parts` on the assistant row. Returned whole by `rows` and `history`,
//! never part of a request, whatever keys it holds.

use super::*;
use crate::provider::ProviderReport;

const GO: &str = r#"ok <call name="done"></call>"#;

fn passes(n: usize) -> Vec<Vec<ScriptEvent>> {
    (0..n).map(|_| pass(GO)).collect()
}

async fn ask(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

async fn say(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) {
    let r = ask(kernel, agent, json!({"type": "send", "text": text})).await;
    assert!(r.get("error").is_none(), "{r}");
}

async fn rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let r = ask(kernel, agent, json!({"type": "rows"})).await;
    r["rows"].as_array().cloned().unwrap_or_default()
}

/// The newest assistant row.
async fn last_answer(kernel: &Arc<Kernel>, agent: &AgentId) -> Value {
    rows(kernel, agent)
        .await
        .into_iter()
        .rev()
        .find(|r| r["role"] == "assistant")
        .expect("an assistant row")
}

fn names(parts: &Value) -> Vec<String> {
    parts
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["part"].as_str().unwrap().to_string())
        .collect()
}

fn part<'a>(parts: &'a Value, name: &str) -> &'a Value {
    parts
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["part"] == name)
        .unwrap_or_else(|| panic!("no {name} in {parts}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_assistant_row_keeps_usage_and_every_row_a_ts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_usage").await;
    let s = script(&agent, passes(2), Duration::ZERO);
    *s.report.lock().unwrap() = ProviderReport {
        prompt_tokens: Some(1000),
        completion_tokens: Some(40),
        cache_read: Some(600),
        cache_write: Some(300),
        stop_reason: None,
    };
    say(&kernel, &agent, "go").await;
    let all = rows(&kernel, &agent).await;
    assert!(all.len() >= 2, "{all:?}");
    for r in &all {
        assert!(
            r["attributes"]["ts"].as_f64().is_some_and(|t| t > 0.0),
            "no ts: {r}"
        );
    }
    let answer = last_answer(&kernel, &agent).await;
    assert_eq!(
        answer["attributes"]["usage"],
        json!({"input": 100, "output": 40, "cache_read": 600, "cache_write": 300})
    );
    // `history` returns them too
    let h = ask(&kernel, &agent, json!({"type": "history"})).await;
    let digest = h["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["role"] == "assistant")
        .unwrap();
    assert_eq!(digest["attributes"], answer["attributes"]);
    assert!(h["rows"][0]["attributes"]["ts"].is_number(), "{h}");

    // an endpoint with no cache counts leaves them absent, never 0
    *s.report.lock().unwrap() = ProviderReport {
        prompt_tokens: Some(1000),
        completion_tokens: Some(40),
        ..Default::default()
    };
    say(&kernel, &agent, "again").await;
    let answer = last_answer(&kernel, &agent).await;
    assert_eq!(
        answer["attributes"]["usage"],
        json!({"input": 1000, "output": 40})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endpoint_that_reports_nothing_leaves_no_usage() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_none").await;
    let _s = script(&agent, passes(1), Duration::ZERO);
    say(&kernel, &agent, "go").await;
    let answer = last_answer(&kernel, &agent).await;
    assert!(answer["attributes"].get("usage").is_none(), "{answer}");
    assert!(answer["attributes"]["parts"].is_array(), "{answer}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_parts_follow_the_request_and_the_request_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_parts").await;
    let s = script(&agent, passes(2), Duration::ZERO);
    say(&kernel, &agent, "go").await;
    let first = last_answer(&kernel, &agent).await["attributes"]["parts"].clone();
    assert_eq!(
        names(&first),
        ["reflect_tree", "self_reflect", "verb_docs", "new_input"]
    );
    say(&kernel, &agent, "again").await;
    let second = last_answer(&kernel, &agent).await["attributes"]["parts"].clone();
    assert_eq!(
        names(&second),
        [
            "reflect_tree",
            "self_reflect",
            "verb_docs",
            "row",
            "row",
            "new_input"
        ]
    );
    let all = rows(&kernel, &agent).await;
    let (user, answer) = (&all[0], &all[1]);
    let held: Vec<&Value> = second
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["part"] == "row")
        .collect();
    assert_eq!(held[0]["ref"], user["id"]);
    assert_eq!(held[0]["form"], "whole");
    assert_eq!(held[1]["ref"], answer["id"]);
    // the same text, the same hash: the first turn's new input is the user row now
    assert_eq!(held[0]["hash"], part(&first, "new_input")["hash"]);
    assert_eq!(held[0]["bytes"], json!(2));
    for p in second.as_array().unwrap() {
        let hash = p["hash"].as_str().unwrap();
        assert!(hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    // THE REQUEST: role and content only, the system block exactly the pieces
    // end to end, the history as the rows hold it
    let sent = s.request_messages(1);
    for m in &sent {
        let keys: Vec<&String> = m.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["role", "content"], "{m}");
    }
    let system = sent[0]["content"].as_str().unwrap();
    let pieces: usize = ["reflect_tree", "self_reflect", "verb_docs"]
        .iter()
        .map(|n| part(&second, n)["bytes"].as_u64().unwrap() as usize)
        .sum();
    assert_eq!(system.len(), pieces + 4, "two blank lines between pieces");
    assert_eq!(sent[1], json!({"role": "user", "content": "go"}));
    assert_eq!(sent[2]["content"], answer["content"]);
    assert_eq!(sent[3], json!({"role": "user", "content": "again"}));
}

#[tokio::test]
async fn two_identical_builds_give_the_same_parts_and_a_changed_prompt_only_its_own() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_hash").await;
    let state = crate::state::state_for(&agent);
    let build = |prompt: &'static str| {
        let (kernel, agent, state) = (kernel.clone(), agent.clone(), state.clone());
        async move {
            fantastic_kernel::test_support::as_agent(
                agent.clone(),
                crate::assembly::assemble_messages(
                    &agent,
                    &state,
                    "next",
                    &kernel,
                    "c",
                    Some(prompt),
                    crate::caching::Caching::Cleanup,
                ),
            )
            .await
            .expect("assemble");
            state.request_parts.lock().unwrap().clone()
        }
    };
    let a = build("ROLE-A {howto}").await;
    assert_eq!(a, build("ROLE-A {howto}").await);
    let b = build("ROLE-B {howto}").await;
    assert_eq!(a.len(), b.len());
    assert_ne!(a[0]["hash"], b[0]["hash"], "system_text changed");
    assert_eq!(a[0]["part"], "system_text");
    assert_eq!(a[1..], b[1..], "nothing else changed");
}
