//! The call checker runs on the stream (#499): a generation that writes a foreign call
//! tag is cut there, and the turn gets the usual correction.

use super::*;

/// The assistant rows the agent's store holds.
async fn answers(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<String> {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|r| r["role"] == json!("assistant"))
        .filter_map(|r| r["content"].as_str().map(str::to_string))
        .collect()
}

async fn send_go(kernel: &Arc<Kernel>, backend: &AgentId) {
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await;
}

fn chars(text: &str) -> Vec<ScriptEvent> {
    text.chars()
        .map(|c| ScriptEvent::Token(c.to_string()))
        .collect()
}

/// `text <invoke name="send">` then thousands of looping tokens: the stream is cut
/// right after the tag, the rest is never stored, the correction reaches the next
/// pass, and the turn ends in a blink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_looping_foreign_tag_is_cut_and_corrected() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_loop").await;
    let mut first = pass("I will send.\n<invoke name=\"send\">");
    first.extend((0..5000).map(|_| ScriptEvent::Token("\n</invoke".into())));
    let s = script(
        &backend,
        vec![first, pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let started = std::time::Instant::now();
    send_go(&kernel, &backend).await;
    assert!(
        until_passes(&s, 2).await,
        "the correction never reached the model"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    let rows = answers(&kernel, &backend).await;
    assert_eq!(rows[0], "I will send.\n<invoke name=\"send\">", "{rows:?}");
    assert!(!rows[0].contains("</invoke"), "{rows:?}");
    let seen = s.request(1);
    assert!(seen.contains("[malformed call — nothing ran]"), "{seen}");
}

/// A correct call is never cut, even delivered a char at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_correct_call_stream_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_ok").await;
    let text = "Done soon.\n<call name=\"done\"></call>";
    let s = script(&backend, vec![chars(text)], Duration::ZERO);
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    let rows = answers(&kernel, &backend).await;
    assert_eq!(rows[0], text, "{rows:?}");
    assert_eq!(s.passes_served(), 1);
}

/// A quoted complete call inside an argument is text, not foreign (#543).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quoted_call_in_an_argument_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_quoted").await;
    let text = "<call name=\"send\"><target_id>probe</target_id><type>say</type>\
                <text>end with <call name=\"done\"></call> then</text></call>";
    let s = script(
        &backend,
        vec![chars(text), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    let rows = answers(&kernel, &backend).await;
    assert_eq!(rows[0], text, "{rows:?}");
}

/// A foreign tag inside thinking is not cut (thinking is excluded); prose that only
/// mentions the word invoke is not cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_and_prose_are_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_think").await;
    let text = "<think>maybe <invoke name=\"send\"> is it</think>I should invoke the done call.\n\
                <call name=\"done\"></call>";
    let s = script(&backend, vec![chars(text)], Duration::ZERO);
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    let rows = answers(&kernel, &backend).await;
    assert_eq!(
        rows[0], "I should invoke the done call.\n<call name=\"done\"></call>",
        "{rows:?}"
    );
}
