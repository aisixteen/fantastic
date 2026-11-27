//! #795: the AI agent as a message-driven machine — the units of the plan's list. A child
//! module of `tests`, so it reaches the mock harness.
//!
//! The deadlock this fixes (E37): with A→B, B→C and C→A each sender's call waited for
//! the receiver's pass, which held the next question while waiting for its own reply.
//! Now every `send` is acked once its message is stored, and a peer answers with its own
//! `send` back, `re` naming the question's `msg`.

use super::*;

const DONE: &str = "<call name=\"done\"></call>";

/// A `send` of `text` to `target`, with `re` when it answers a message.
fn send_call(target: &str, text: &str, re: Option<&str>) -> String {
    let re = re.map(|r| format!("<re>{r}</re>")).unwrap_or_default();
    format!(
        "<call name=\"send\"><routing><target_id>{target}</target_id>{re}</routing>\
         <payload><type>send</type><text>{text}</text></payload></call>"
    )
}

/// `n` AI peers in one kernel, each with its own leg and a line on every other.
async fn peers(tmp: &TempDir, tag: &str, n: usize) -> (Arc<Kernel>, Vec<AgentId>) {
    let kernel = base_kernel(tmp, |_| {}).await;
    let mut ids = Vec::new();
    for i in 0..n {
        let id = format!("{}_{i}", id_for(tmp, tag));
        let file_id = format!("ff_{id}");
        add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic").join(&id)).await;
        ids.push(add_backend(&kernel, &id, &file_id).await);
    }
    for (i, me) in ids.iter().enumerate() {
        let mut can = json!({
            "$parent": {"call": ["*"]},
            format!("ff_{me}"): {"call": ["*"]},
            "cli": {"call": ["*"]},
            MEMORY: {"call": ["*"]},
        });
        for (j, other) in ids.iter().enumerate() {
            if i != j {
                can[other.as_str()] = json!({"call": ["*"]});
            }
        }
        let r = kernel
            .operator_send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "update_agent", "id": me.as_str(), "can": can}),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    (kernel, ids)
}

/// A peer's model in a ring: a `KICK` makes it ask `next` (once); every question it
/// has not answered yet makes it answer the asker with its own `send`, `re` the
/// question's `msg`; then `done`. It reads its whole request, as a model does: a
/// message that arrived while its last pass worked lies above that pass's answer.
fn ring_model(me: &str, next: &str) -> Responder {
    let (me, next) = (me.to_string(), next.to_string());
    let done_with: std::sync::Mutex<std::collections::HashSet<String>> = Default::default();
    Box::new(move |msgs: &[Value]| {
        let mut calls = String::new();
        let mut seen = done_with.lock().unwrap();
        for m in msgs.iter().filter(|m| m["role"] == "user") {
            let f = m["content"].as_str().map_or_else(|| m["content"].to_string(), str::to_string);
            if f.contains("KICK") && seen.insert("KICK".into()) {
                calls.push_str(&send_call(&next, &format!("QUESTION from {me}"), None));
            }
            if f.contains("QUESTION from") {
                let Some(head) = f.strip_prefix("[from ").and_then(|r| r.split(']').next()) else {
                    continue;
                };
                let mut parts = head.split(" \u{b7} ");
                let from = parts.next().unwrap_or_default().to_string();
                let msg = parts.find_map(|p| p.strip_prefix("msg ")).map(str::to_string);
                if seen.insert(format!("{from}/{msg:?}")) {
                    calls.push_str(&send_call(&from, &format!("ANSWER from {me}"), msg.as_deref()));
                }
            }
        }
        Some(format!("{calls}{DONE}"))
    })
}

/// The id of `agent`'s answer row whose text holds `needle`.
async fn answer_with(kernel: &Arc<Kernel>, agent: &AgentId, needle: &str) -> Option<i64> {
    history_of(kernel, agent)
        .await
        .expect("history")
        .iter()
        .find(|r| r["role"] == "assistant" && r["content"].as_str().is_some_and(|c| c.contains(needle)))
        .and_then(|r| r["id"].as_i64())
}

/// THE RING: `n` peers ask each other at once, each the next; every question is answered
/// by its receiver's own `send` back with `re` = the question's `msg`; nothing waits,
/// so the ring ends at the speed of the scripted model, every agent quiet.
async fn ring(tag: &str, n: usize) {
    let tmp = TempDir::new().unwrap();
    let (kernel, ids) = peers(&tmp, tag, n).await;
    let scripts: Vec<Arc<MockScript>> = ids.iter().map(|id| script(id, vec![], Duration::ZERO)).collect();
    for (i, s) in scripts.iter().enumerate() {
        let next = &ids[(i + 1) % n];
        *s.responder.lock().unwrap() = Some(ring_model(ids[i].as_str(), next.as_str()));
    }
    let started = std::time::Instant::now();
    for id in &ids {
        let ack = kernel
            .operator_send(vec![Item::new(id.clone(), json!({"type": "send", "text": "KICK", "client_id": "ring"}))])
            .await
            .remove(0)
            .result;
        assert_eq!(ack["accepted"], json!(true), "{ack}");
    }
    assert!(settle(&kernel).await, "the ring never went quiet: a peer waited on a peer");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    for (i, me) in ids.iter().enumerate() {
        let next = &ids[(i + 1) % n];
        let asked = answer_with(&kernel, me, "QUESTION from").await.expect("the question");
        let got: Vec<String> = history_of(&kernel, me)
            .await
            .expect("history")
            .iter()
            .filter(|r| r["role"] == "user")
            .filter_map(|r| r["content"].as_str().map(str::to_string))
            .filter(|c| c.contains(&format!("ANSWER from {next}")))
            .collect();
        let all = history_of(&kernel, me).await.expect("history");
        assert_eq!(got.len(), 1, "{me}: its answer, once: {got:?}\n{all:#?}");
        assert!(
            got[0].starts_with(&format!("[from {next} \u{b7} msg ")) && got[0].contains(&format!(" \u{b7} re {asked}")),
            "{me}: the answer names the question it answers: {}",
            got[0]
        );
        let passes = scripts[i].passes_served();
        let answers = history_of(&kernel, me)
            .await
            .expect("history")
            .iter()
            .filter(|r| r["role"] == "assistant")
            .count();
        assert!((2..=3).contains(&passes), "{me}: {passes} passes");
        assert_eq!(passes, answers, "{me}: one answer row per pass");
    }
    let table = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel"), json!({"type": "ps"}))])
        .await
        .remove(0)
        .result;
    let left: Vec<&Value> = table["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter(|t| ids.iter().any(|id| t["agent"] == json!(id.as_str())))
        .collect();
    assert!(left.is_empty(), "no task of a peer is left: {left:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_peers_asking_in_a_ring_all_get_answers() {
    ring("ring3", 3).await;
}

/// The smallest cycle: A asks B while B asks A.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_peers_asking_each_other_both_get_answers() {
    ring("ring2", 2).await;
}

/// An answer with `re` lands on the asker as a message `[from B · msg N · re M]`, and it
/// wakes the asker: its pass reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_with_re_lands_as_a_message_on_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ids) = peers(&tmp, "re", 2).await;
    let (a, b) = (&ids[0], &ids[1]);
    let sa = script(a, vec![pass(format!("{}{DONE}", send_call(b.as_str(), "QUESTION from a", None))), pass(DONE)], Duration::ZERO);
    let sb = script(b, vec![], Duration::ZERO);
    *sb.responder.lock().unwrap() = Some(ring_model(b.as_str(), a.as_str()));
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(a.clone(), json!({"type": "send", "text": "go"}))])).await;
    assert_eq!(sa.passes_served(), 2, "the answer woke the asker once");
    let asked = answer_with(&kernel, a, "QUESTION from a").await.expect("the question");
    // the answer is in the asker's next request: after its answer, or above it with the
    // notice when it landed while that pass worked
    let read = Value::Array(sa.request_messages(1)).to_string();
    assert!(read.contains(&format!("re {asked}")) && read.contains(&format!("ANSWER from {b}")), "{read}");
}

/// THE ACK OF A MESSAGE TO AN AI IS STORED AND WAKES NOTHING (Q2): the sender's next
/// pass reads it, but it runs no pass of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ack_only_reply_is_stored_and_wakes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ids) = peers(&tmp, "ack", 2).await;
    let (a, b) = (&ids[0], &ids[1]);
    let sa = script(a, vec![pass(format!("{}{DONE}", send_call(b.as_str(), "fyi", None)))], Duration::ZERO);
    let _sb = script(b, vec![pass(DONE)], Duration::ZERO);
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(a.clone(), json!({"type": "send", "text": "go"}))])).await;
    assert_eq!(sa.passes_served(), 1, "an ack woke a pass");
    let replies = reply_rows(&kernel, a).await;
    assert_eq!(replies.len(), 1, "{replies:#?}");
    assert!(replies[0].contains(&format!("{b} send")) && replies[0].contains("<accepted>true</accepted>"), "{}", replies[0]);
}

/// A REFUSED send wakes the sender (Q2): a refusal is news the model must read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_send_in_a_batch_wakes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ids) = peers(&tmp, "refused", 2).await;
    let a = &ids[0];
    // `stranger` is an agent `a` holds no line on
    let stranger = add_backend(&kernel, &format!("{}_stranger", id_for(&tmp, "refused")), "nobody").await;
    let sa = script(
        a,
        vec![pass(format!("{}{DONE}", send_call(stranger.as_str(), "hi", None))), pass(DONE)],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(a.clone(), json!({"type": "send", "text": "go"}))])).await;
    assert_eq!(sa.passes_served(), 2, "the refusal woke the sender");
    let read = Value::Array(sa.request_messages(1)).to_string();
    assert!(read.contains(&format!("{stranger} send")) && read.contains("<error>"), "{read}");
}

/// A chat on the echo provider agent: its id.
async fn echo_chat(kernel: &Arc<Kernel>, chat: &str, prov: &str, model: &str) -> AgentId {
    make_provider(kernel, prov, json!({}), Some(model)).await;
    let memory = fantastic_ram_store::test_support::memory_meta(
        kernel,
        chat,
        json!({"cli": {"call": ["*"]}, prov: {"call": ["complete", "watch"]}}),
    )
    .await;
    let mut create = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
                            "id": chat, "provider_id": prov, "context_window": 32768});
    for (k, v) in memory {
        create[k] = v;
    }
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], chat, "{made}");
    AgentId::from(chat)
}

async fn echo_kernel(tmp: &TempDir) -> Arc<Kernel> {
    base_kernel(tmp, |k| {
        k.bundles.register("echo_provider.tools", EchoProvider);
        k.bundles.register(crate::chat_bundle::HANDLER_MODULE, crate::chat_bundle::AiChatBundle);
    })
    .await
}

/// The `ps` rows of `agent` with `verb`.
async fn tasks_of(kernel: &Arc<Kernel>, agent: &str, verb: Option<&str>) -> Vec<Value> {
    kernel
        .operator_send(vec![Item::new(AgentId::from("kernel"), json!({"type": "ps"}))])
        .await
        .remove(0)
        .result["tasks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t["agent"] == json!(agent) && verb.is_none_or(|v| t["verb"] == json!(v)))
        .collect()
}

/// The `complete` calls the provider `prov` was sent: the kernel's call rows about it.
fn completes(kernel: &Arc<Kernel>, prov: &str) -> usize {
    fantastic_kernel::test_support::rows_about(kernel, &AgentId::from(prov))
        .iter()
        .filter(|r| r.get("kind") == Some(&json!("call")) && r.get("verb") == Some(&json!("complete")))
        .count()
}

/// A PASS ENDS WHEN IT SENDS `complete` (#795): while the provider still answers, no task
/// of the chat is running — the request is the provider's execution, its settle the next
/// step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_ends_when_it_sends_complete() {
    let tmp = TempDir::new().unwrap();
    let kernel = echo_kernel(&tmp).await;
    let chat = echo_chat(&kernel, "chat_gated", "prov_gated", "m-gated-ends").await;
    let ack = kernel
        .operator_send(vec![Item::new(chat.clone(), json!({"type": "send", "text": "hi"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(ack["accepted"], json!(true), "{ack}");
    until_true("the provider's complete in flight", || completes(&kernel, "prov_gated") == 1).await;
    // the provider's execution is the request; nothing of the chat waits on it
    let mut theirs = Vec::new();
    for _ in 0..100 {
        theirs = tasks_of(&kernel, "prov_gated", Some("complete")).await;
        if !theirs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // one execution is the request; the provider's own I/O (its stream) runs under it
    let ids: Vec<Value> = theirs.iter().map(|t| t["id"].clone()).collect();
    let requests = theirs.iter().filter(|t| !ids.contains(&t["parent"])).count();
    assert_eq!(requests, 1, "the provider answers the request: {theirs:?}");
    let mine = tasks_of(&kernel, chat.as_str(), None).await;
    assert!(mine.is_empty(), "no task of the chat waits: {mine:?}");
    assert!(crate::state::is_generating(&chat), "the pass is in flight, as a record");
    echo_gate("m-gated-ends").notify_one();
    assert!(settle(&kernel).await);
    assert_eq!(last_answer(&kernel, &chat).await, "released <call name=\"done\"></call>");
}

/// A CANCEL OF THE PASS'S `complete` ends the pass as stopped: no re-ask, a `stopped`
/// row, nothing generating.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_complete_ends_the_pass() {
    let tmp = TempDir::new().unwrap();
    let kernel = echo_kernel(&tmp).await;
    let chat = echo_chat(&kernel, "chat_cut", "prov_cut", "m-gated-cut").await;
    let _ = kernel
        .operator_send(vec![Item::new(chat.clone(), json!({"type": "send", "text": "hi"}))])
        .await;
    let mut theirs = Vec::new();
    for _ in 0..200 {
        theirs = tasks_of(&kernel, "prov_cut", Some("complete")).await;
        if !theirs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = theirs.first().and_then(|t| t["id"].as_i64()).expect("the provider's complete in ps");
    let r = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel"), json!({"type": "cancel", "task_id": pid}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["cancelled"], json!(true), "{r}");
    assert!(settle(&kernel).await);
    assert_eq!(completes(&kernel, "prov_cut"), 1, "a cancelled request is not re-asked");
    let rows = history_of(&kernel, &chat).await.expect("history");
    assert!(rows.iter().any(|r| r["kind"] == json!("stopped")), "{rows:#?}");
    assert!(!rows.iter().any(|r| r["role"] == json!("assistant") && r["content"].as_str().is_some_and(|c| c.contains("released"))));
    assert!(!crate::state::is_generating(&chat));
}

/// A RE-ASK IS A SECOND `complete` (#795): the malformed answer's settle sends the second
/// request; one answer row, the re-asked one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reask_is_a_second_complete() {
    let tmp = TempDir::new().unwrap();
    let kernel = echo_kernel(&tmp).await;
    let chat = echo_chat(&kernel, "chat_reask", "prov_reask", "m-reask").await;
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(chat.clone(), json!({"type": "send", "text": "hi"}))])).await;
    assert_eq!(completes(&kernel, "prov_reask"), 2, "the re-ask is a second complete");
    let answers: Vec<String> = history_of(&kernel, &chat)
        .await
        .expect("history")
        .iter()
        .filter(|r| r["role"] == "assistant")
        .filter_map(|r| r["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(answers, vec!["fixed <call name=\"done\"></call>".to_string()]);
}

/// A MESSAGE STORED BEFORE A PASS READS ITS HISTORY IS THAT PASS'S (#795): it arrived
/// after the pass took its tail, so it is on the unread list too, but the request
/// carries it, and no second pass reads it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_the_history_read_holds_wakes_no_second_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ids) = peers(&tmp, "readby", 1).await;
    let a = &ids[0];
    let sa = script(a, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let _ = kernel.operator_send(vec![Item::new(a.clone(), json!({"type": "send", "text": "first"}))]).await;
    let _ = kernel.operator_send(vec![Item::new(a.clone(), json!({"type": "send", "text": "second"}))]).await;
    assert!(settle(&kernel).await);
    let reqs: Vec<String> = (0..sa.passes_served()).map(|i| Value::Array(sa.request_messages(i)).to_string()).collect();
    let reading = reqs.iter().filter(|r| r.contains("second")).count();
    let fresh_second = reqs
        .iter()
        .filter(|r| {
            let at = r.rfind("\"assistant\"").unwrap_or(0);
            r[at..].contains("second")
        })
        .count();
    assert!(reading >= 1, "{reqs:#?}");
    assert_eq!(fresh_second, 1, "the second message is read anew by exactly one pass: {reqs:#?}");
}
