//! The call checker runs on the stream (#499): a generation that writes a foreign call
//! tag is cut there, and the turn gets the usual correction.

use super::*;

/// The assistant rows the agent's store holds, whole.
async fn assistant_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
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
        .into_iter()
        .filter(|r| r["role"] == json!("assistant"))
        .collect()
}

/// The assistant rows' text the agent's store holds.
async fn answers(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<String> {
    assistant_rows(kernel, agent)
        .await
        .iter()
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

/// A call that lost its `<` is not cut: it runs and is stored whole (Q1 B).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_that_lost_its_lt_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_lt").await;
    let good = call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None);
    let s = script(
        &backend,
        vec![
            chars(&format!("<think>x</think>{}", &good[1..])),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 2).await);
    let seen = s.request(1);
    assert!(!seen.contains("[malformed call"), "{seen}");
    assert!(seen.contains("kernel reflect"), "{seen}");
    let rows = answers(&kernel, &backend).await;
    assert!(rows[0].starts_with("<call name=\"send\">"), "{rows:?}");
}

const FOREIGN: &str = "Sending.\n<invoke name=\"send\">";

/// Nine turns in a row cut at a foreign tag: the chain gives up, naming the cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nine_cut_turns_in_a_row_give_up() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_cut_nine").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let fb = script(
        &b,
        (0..12)
            .map(|_| pass(r#"<call name="done"></call>"#))
            .collect(),
        Duration::ZERO,
    );
    let s = script(&a, (0..12).map(|_| pass(FOREIGN)).collect(), Duration::ZERO);
    let mut rx = rebind_backend_inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "tab1", "send_to": b.as_str()}),
        )])
        .await;
    assert!(until_passes(&s, 9).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let limit = crate::constants::MAX_CUT_TURNS;
    assert_eq!(s.passes_served(), limit);
    let text = crate::route::cut_gave_up_text(9);
    let mut dones = Vec::new();
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status") && p["phase"] == json!("done") {
            dones.push(p);
        }
    }
    let last = dones.last().expect("a done status");
    assert_eq!(last["detail"]["reason"], json!("gave_up"), "{dones:#?}");
    assert_eq!(last["detail"]["error"], json!(text), "{last:#?}");
    assert_eq!(last["detail"]["silent_turns"], json!(9), "{last:#?}");
    // Only the first turn carries `send_to` (the one-shot hand-off): the nine-turn
    // give-up is told by the status, and the peer got that first turn's text only.
    assert!(until_passes(&fb, 1).await);
    assert_eq!(
        text,
        "The LLM was not able to answer — 9 turns in a row were cut at a foreign call syntax. Reiterate the inquiry."
    );
}

/// The give-up reaches the sender: a peer's `inquiry` to a chain that is cut nine
/// turns in a row is answered with the cut error (`close_inquiry`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_give_up_is_told_to_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_cut_ask").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let ask = call_envelope(a.as_str(), json!({"type": "inquiry", "text": "go"}), None);
    let fb = script(
        &b,
        vec![
            pass(ask),
            pass(r#"<call name="await"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let s = script(&a, (0..12).map(|_| pass(FOREIGN)).collect(), Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            b.clone(),
            json!({"type": "send", "text": "ask", "client_id": "probe"}),
        )])
        .await;
    assert!(until_passes(&fb, 3).await, "the asker never got the answer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), crate::constants::MAX_CUT_TURNS);
    let seen = fb.request(2);
    assert!(seen.contains(&crate::route::cut_gave_up_text(9)), "{seen}");
}

/// Run `passes` on a fresh chain and report whether any done status gave up.
async fn gave_up_after(
    tag: &str,
    misfire_limit: Option<u64>,
    passes: Vec<Vec<ScriptEvent>>,
) -> bool {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, tag).await;
    if let Some(n) = misfire_limit {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "update_agent", "id": a.as_str(), "misfire_limit": n}),
            )])
            .await
            .remove(0)
            .result;
    }
    let total = passes.len();
    let s = script(&a, passes, Duration::ZERO);
    let mut rx = rebind_backend_inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "tab1"}),
        )])
        .await;
    assert!(until_passes(&s, total).await, "the chain stalled");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.passes_served(), total);
    let mut gave_up = false;
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status")
            && p["phase"] == json!("done")
            && p["detail"]["reason"] == json!("gave_up")
        {
            gave_up = true;
        }
    }
    gave_up
}

/// Eight cuts, a call, eight cuts: a dispatched call resets the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eight_cuts_then_a_call_reset_the_cut_count() {
    let reflect = call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None);
    let mut passes: Vec<_> = (0..8).map(|_| pass(FOREIGN)).collect();
    passes.push(pass(reflect));
    passes.extend((0..8).map(|_| pass(FOREIGN)));
    passes.push(pass(r#"<call name="done"></call>"#));
    assert_eq!(passes.len(), 18);
    assert!(!gave_up_after("rt_cut_reset", None, passes).await);
}

/// A prose turn between cut runs breaks "in a row".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prose_turn_breaks_the_cut_run() {
    let mut passes: Vec<_> = (0..8).map(|_| pass(FOREIGN)).collect();
    passes.push(pass("just prose"));
    passes.extend((0..8).map(|_| pass(FOREIGN)));
    passes.push(pass(r#"<call name="done"></call>"#));
    assert_eq!(passes.len(), 18);
    assert!(!gave_up_after("rt_cut_prose", Some(20), passes).await);
}

/// A cut after thinking keeps the thinking in the store (for #556), never in the
/// next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_after_thinking_keeps_it_in_the_store_not_the_view() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_keep").await;
    let mut first =
        chars("<think>I will use my native tool.</think>Sending.\n<invoke name=\"send\">");
    first.extend((0..2000).map(|_| ScriptEvent::Token("\n</invoke".into())));
    let s = script(
        &backend,
        vec![first, pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 2).await);
    let rows = assistant_rows(&kernel, &backend).await;
    assert_eq!(
        rows[0]["content"],
        json!("Sending.\n<invoke name=\"send\">"),
        "{rows:?}"
    );
    assert_eq!(
        rows[0]["thinking"],
        json!("I will use my native tool."),
        "{rows:?}"
    );
    let seen = s.request(1);
    assert!(seen.contains("Sending."), "{seen}");
    assert!(!seen.contains("native tool"), "{seen}");
    assert!(!seen.contains("<think>"), "{seen}");
}
