//! THE RECORD'S `system_prompt` (owner, 2026-10-03, option b): set on an ai_core
//! agent's record it REPLACES the default system block on EVERY turn — a main
//! turn, a self-delivery, a turn woken by a reply. Precedence: the per-call
//! argument (that delivery) > the record field > the default block; the record
//! is read LIVE, so an `update_agent` lands on the next turn. Python
//! `ai_core/core.py:_assemble`.

use super::*;

/// The system block handed to pass `n`.
fn system_of(s: &Arc<MockScript>, n: usize) -> String {
    let msgs = s.request_messages(n);
    assert_eq!(msgs[0]["role"], "system", "{msgs:?}");
    msgs[0]["content"].as_str().unwrap_or_default().to_string()
}

async fn set_record_prompt(kernel: &Arc<Kernel>, backend: &AgentId, v: Value) -> Value {
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "system_prompt": v}),
        )])
        .await
        .remove(0)
        .result
}

async fn say(kernel: &Arc<Kernel>, backend: &AgentId, payload: Value) {
    let _ = kernel
        .send(vec![Item::new(backend.clone(), payload)])
        .await
        .remove(0)
        .result;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_record_prompt_is_the_system_block_of_a_main_and_a_reply_woken_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "sp_rec").await;
    let r = set_record_prompt(&kernel, &backend, json!("You are the record's role.")).await;
    assert!(r.get("error").is_none(), "{r}");
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                "kernel_state",
                json!({"type": "list_agents"}),
                None,
            )),
            pass(r#"done <call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    say(&kernel, &backend, json!({"type": "send", "text": "go"})).await;
    assert!(until_passes(&s, 2).await, "the reply never woke a turn");
    assert_eq!(system_of(&s, 0), "You are the record's role.");
    // turn 2 is the one the list_agents reply woke
    assert!(s.request(1).contains("[reply to "), "{}", s.request(1));
    assert_eq!(system_of(&s, 1), "You are the record's role.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_per_call_prompt_beats_the_record() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "sp_call").await;
    set_record_prompt(&kernel, &backend, json!("record role")).await;
    let s = script(
        &backend,
        vec![pass(r#"ok <call name="done"></call>"#)],
        Duration::ZERO,
    );
    say(
        &kernel,
        &backend,
        json!({"type": "send", "text": "go", "system_prompt": "call role"}),
    )
    .await;
    assert!(until_passes(&s, 1).await);
    assert_eq!(system_of(&s, 0), "call role");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_agent_lands_on_the_next_turn_and_null_restores_the_default() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "sp_upd").await;
    let s = script(
        &backend,
        vec![
            pass(r#"one <call name="done"></call>"#),
            pass(r#"two <call name="done"></call>"#),
            pass(r#"three <call name="done"></call>"#),
            pass(r#"four <call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    // no record prompt: the default block (it names the agent)
    say(&kernel, &backend, json!({"type": "send", "text": "a"})).await;
    assert!(until_passes(&s, 1).await);
    let default = system_of(&s, 0);
    assert!(
        default.contains(&format!("You are `{}`.", backend.as_str())),
        "{default}"
    );
    set_record_prompt(&kernel, &backend, json!("first role")).await;
    say(&kernel, &backend, json!({"type": "send", "text": "b"})).await;
    assert!(until_passes(&s, 2).await);
    assert_eq!(system_of(&s, 1), "first role");
    set_record_prompt(&kernel, &backend, json!("second role")).await;
    say(&kernel, &backend, json!({"type": "send", "text": "c"})).await;
    assert!(until_passes(&s, 3).await);
    assert_eq!(system_of(&s, 2), "second role");
    // null clears it: the default block again
    let r = set_record_prompt(&kernel, &backend, Value::Null).await;
    assert!(r.get("error").is_none(), "{r}");
    say(&kernel, &backend, json!({"type": "send", "text": "d"})).await;
    assert!(until_passes(&s, 4).await);
    assert!(
        system_of(&s, 3).contains(&format!("You are `{}`.", backend.as_str())),
        "{}",
        system_of(&s, 3)
    );
}
