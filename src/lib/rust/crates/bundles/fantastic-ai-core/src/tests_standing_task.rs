//! #218: a long-running agent keeps its standing task across a clear and a restart.
//! The thread's first message set the agent's role; `context_clear` copies it onto the
//! clear row, so the head a restarted agent loads (from the last clear on) still has it.
//!
//! A child of `tests`, so it drives the same harness.

use super::*;
use fantastic_kernel::test_support::as_agent;

const TASK: &str = "You are the DRIVER. Protocol: dispatch the issues, poll the workers, report.";

async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

async fn store(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// The text of every message the next turn would send, as the model view assembles it.
async fn prompt(kernel: &Arc<Kernel>, agent: &AgentId) -> String {
    let state = crate::state::state_for(agent);
    let (msgs, _) = as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            agent,
            &state,
            "next",
            kernel,
            "c",
            Some("sys"),
            crate::caching::Caching::Cleanup,
        ),
    )
    .await
    .expect("assemble");
    msgs.iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A working thread: the task, then replies and chatter.
async fn work(kernel: &Arc<Kernel>, agent: &AgentId) {
    for (role, text) in [
        ("user", TASK),
        ("assistant", "starting"),
        ("user", "[reply to 7 · gh_exec run]\nlisted 3 issues"),
        ("assistant", "polling"),
    ] {
        put_row(kernel, agent, json!({"role": role, "content": text})).await;
    }
}

#[tokio::test]
async fn the_clear_row_carries_the_standing_task_and_the_view_shows_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "st_clear").await;
    work(&kernel, &agent).await;
    let clear = verb(&kernel, &agent, json!({"type": "context_clear"})).await;
    let cid = clear["cleared"].as_i64().expect("the clear row");
    let row = store(&kernel, &agent)
        .await
        .into_iter()
        .find(|r| r["id"].as_i64() == Some(cid))
        .expect("clear row");
    assert_eq!(row["task"], TASK);
    assert!(row["content"].as_str().unwrap().contains(TASK));
    assert!(prompt(&kernel, &agent).await.contains(TASK));
}

#[tokio::test]
async fn a_restarted_agent_still_has_its_task_in_view() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "st_restart").await;
    work(&kernel, &agent).await;
    verb(&kernel, &agent, json!({"type": "context_clear"})).await;
    put_row(
        &kernel,
        &agent,
        json!({"role": "user", "content": "start ais"}),
    )
    .await;
    // A restart: the head cache is gone, the head reloads from the last clear.
    crate::memory::forget_head(&agent, &kernel);
    let seen = prompt(&kernel, &agent).await;
    assert!(
        seen.contains(TASK),
        "the task is lost after a restart: {seen}"
    );
    assert!(seen.contains("start ais"));
}

#[tokio::test]
async fn the_task_rides_across_a_second_clear() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "st_twice").await;
    work(&kernel, &agent).await;
    verb(&kernel, &agent, json!({"type": "context_clear"})).await;
    put_row(
        &kernel,
        &agent,
        json!({"role": "user", "content": "later message"}),
    )
    .await;
    let second = verb(&kernel, &agent, json!({"type": "context_clear"})).await;
    let cid = second["cleared"].as_i64().expect("the clear row");
    let row = store(&kernel, &agent)
        .await
        .into_iter()
        .find(|r| r["id"].as_i64() == Some(cid))
        .expect("clear row");
    assert_eq!(
        row["task"], TASK,
        "the second clear took 'later message' as the task"
    );
    assert!(prompt(&kernel, &agent).await.contains(TASK));
}

#[tokio::test]
async fn a_reply_is_never_taken_for_the_task() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "st_reply").await;
    put_row(
        &kernel,
        &agent,
        json!({"role": "user", "content": "[reply to 7 · files read]\nsome file"}),
    )
    .await;
    put_row(&kernel, &agent, json!({"role": "user", "content": TASK})).await;
    let clear = verb(&kernel, &agent, json!({"type": "context_clear"})).await;
    let cid = clear["cleared"].as_i64().expect("the clear row");
    let row = store(&kernel, &agent)
        .await
        .into_iter()
        .find(|r| r["id"].as_i64() == Some(cid))
        .expect("clear row");
    assert_eq!(row["task"], TASK);
}

#[tokio::test]
async fn an_empty_thread_has_no_task_and_the_clear_row_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "st_none").await;
    let clear = verb(&kernel, &agent, json!({"type": "context_clear"})).await;
    let cid = clear["cleared"].as_i64().expect("the clear row");
    let row = store(&kernel, &agent)
        .await
        .into_iter()
        .find(|r| r["id"].as_i64() == Some(cid))
        .expect("clear row");
    assert!(row["task"].is_null());
    assert!(!row["content"].as_str().unwrap().contains("Standing task"));
}
