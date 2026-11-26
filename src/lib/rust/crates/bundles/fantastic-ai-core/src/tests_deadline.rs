//! #568 under #795: a call's deadline (root `send_timeout`) and its dead letter, on a
//! slow NON-AI target; a send to an AI never times out (it is acked at once); the
//! deadline the model is told; and `status` counting the messages not read yet.

use super::*;
use fantastic_kernel::test_support::as_agent;

const DONE: &str = "<call name=\"done\"></call>";

fn set_root_timeout(kernel: &Arc<Kernel>, v: Value) {
    let root = kernel.root().unwrap().id.clone();
    fantastic_kernel::test_support::update(
        kernel,
        &root,
        serde_json::Map::from_iter([("send_timeout".to_string(), v)]),
    );
}

/// Sets `FANTASTIC_AI_SEND_TIMEOUT` and restores what it found on drop.
struct EnvGuard(Option<String>);

impl EnvGuard {
    fn set(v: Option<&str>) -> Self {
        let prev = std::env::var("FANTASTIC_AI_SEND_TIMEOUT").ok();
        match v {
            Some(v) => std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", v),
            None => std::env::remove_var("FANTASTIC_AI_SEND_TIMEOUT"),
        }
        Self(prev)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", v),
            None => std::env::remove_var("FANTASTIC_AI_SEND_TIMEOUT"),
        }
    }
}

async fn rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    as_agent(
        agent.clone(),
        crate::memory::load_history(agent, kernel, false),
    )
    .await
    .expect("store read")
}

fn spawn_send(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    text: &str,
    client: &str,
    timeout: Option<f64>,
) -> tokio::task::JoinHandle<fantastic_kernel::Outcome> {
    let (k, a, t, c) = (
        Arc::clone(kernel),
        agent.clone(),
        text.to_string(),
        client.to_string(),
    );
    tokio::spawn(async move {
        let mut item = Item::new(a, json!({"type": "send", "text": t, "client_id": c}));
        if let Some(s) = timeout {
            item = item.with_timeout(s);
        }
        k.operator_send(vec![item]).await.remove(0)
    })
}

/// A SEND TO AN AI NEVER TIMES OUT (#795): it is acked once its message is stored, so
/// even a tight deadline is met while the agent's pass runs; the message is read by the
/// next pass, and nothing is withdrawn or cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ai_send_never_times_out_it_is_acked_at_once() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_held").await;
    let s = script(&backend, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let gate = s.hold_next();
    let _first = spawn_send(&kernel, &backend, "one", "o", None);
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let out = spawn_send(&kernel, &backend, "two", "t", Some(0.3)).await.unwrap();
    assert_eq!(out.status, fantastic_kernel::outcome::Status::Completed, "{:?}", out.result);
    assert_eq!(out.result["accepted"], json!(true), "{}", out.result);
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert_eq!(s.passes_served(), 2, "the message was read by the next pass");
    assert!(s.request(1).contains("two"), "{}", s.request(1));
    let all = rows(&kernel, &backend).await;
    assert!(!all.iter().any(|r| r["kind"] == "withdrawn" || r["kind"] == "stopped"), "{all:?}");
}

/// A CALL PAST ITS DEADLINE IS A DEAD LETTER, on a slow NON-AI target (#568): the
/// reply tells the sender where it went and how to drop it, the dead letter can be
/// acked, and the call can be sent again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_call_past_its_deadline_tells_the_sender_its_dead_letter_and_can_be_resent() {
    let _lock = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _env = EnvGuard::set(None);
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = with_sleeper(&tmp, "dl_resend").await;
    set_root_timeout(&kernel, json!(0.3));
    let s = script(
        &backend,
        vec![pass(send_call("sleeper", "ping", None)), pass(DONE)],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "o"}),
        )]))
        .await;
    assert!(until_passes(&s, 2).await, "the dead letter woke no pass");
    let req = s.request(1);
    let marker = "your message to sleeper hit its 0.3s deadline \u{2192} dead letter ";
    let at = req.find(marker).unwrap_or_else(|| panic!("{req}"));
    let pid: String = req[at + marker.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    assert!(!pid.is_empty(), "{req}");
    let acked = as_agent(
        backend.clone(),
        kernel.send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "ack_deadletter", "id": pid.parse::<i64>().unwrap()}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(acked["acked"], json!(true), "{acked}");
    let resent = as_agent(
        backend.clone(),
        kernel.send(vec![Item::new(
            AgentId::from("sleeper"),
            json!({"type": "send", "text": "ping", "ms": 10}),
        )
        .with_timeout(10.0)]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(resent["slept"], json!(true), "{resent}");
}

/// `status` answers at once while a pass runs, counts the messages stored and not read
/// yet as pending and names their senders.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_counts_unread_messages_and_names_their_senders() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_status").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let s = script(&backend, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let gate = s.hold_next();
    let _first = spawn_send(&kernel, &backend, "one", "o", None);
    assert!(until_passes(&s, 1).await);
    let two = as_agent(
        peer.clone(),
        kernel.send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two", "client_id": "pc"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(two["accepted"], json!(true), "{two}");
    let t = std::time::Instant::now();
    let bare = kernel.operator_send(vec![Item::new(backend.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
    assert_eq!(bare["generating"], json!(true), "{bare}");
    assert_eq!(bare["others_pending"], json!(1), "{bare}");
    assert_eq!(bare["pending_from"], json!([peer.as_str()]), "{bare}");
    let mine = kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "status", "client_id": "pc"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(mine["mine_pending"][0]["text"].as_str().unwrap().contains("two"), "{mine}");
    assert_eq!(mine["others_pending"], json!(0), "{mine}");
    assert_eq!(mine["pending_from"], json!([peer.as_str()]), "{mine}");
    gate.notify_one();
    assert!(settle(&kernel).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_deadline_takes_the_root_field_then_the_env_then_600() {
    let _lock = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let (kernel, _backend) = mk_kernel(&tmp, "dl_route").await;
    {
        let _env = EnvGuard::set(None);
        assert_eq!(
            crate::route::route_deadline(&kernel),
            crate::constants::SEND_TIMEOUT_SECS
        );
    }
    let _env = EnvGuard::set(Some("45"));
    assert_eq!(crate::route::route_deadline(&kernel), 45.0);
    set_root_timeout(&kernel, json!(7));
    assert_eq!(crate::route::route_deadline(&kernel), 7.0);
    set_root_timeout(&kernel, json!(0.5));
    assert_eq!(crate::route::route_deadline(&kernel), 0.5);
}

/// A call routed to a peer: `<routing>` carries the target and, optionally, the
/// per-call timeout.
fn send_call(target: &str, text: &str, timeout: Option<&str>) -> String {
    let t = timeout
        .map(|t| format!("<timeout>{t}</timeout>"))
        .unwrap_or_default();
    format!(
        "<call name=\"send\"><routing><target_id>{target}</target_id>{t}<reply_to>self</reply_to></routing><payload><type>send</type><text>{text}</text></payload></call>"
    )
}

/// The reply to a `send` says the deadline it ran under: the root's, or the call's own
/// `timeout`. To a peer AI the reply is its ack, stored for the sender's next pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_reply_states_its_deadline_and_a_per_call_timeout_overrides_it() {
    let _lock = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _env = EnvGuard::set(None);
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_told").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    set_root_timeout(&kernel, json!(5));
    let _sp = script(&peer, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let s = script(
        &backend,
        vec![pass(format!(
            "{}{}",
            send_call(peer.as_str(), "a", None),
            send_call(peer.as_str(), "b", Some("30"))
        ))],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "o"}),
        )]))
        .await;
    let kept: String = rows(&kernel, &backend)
        .await
        .iter()
        .filter_map(|r| r["content"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(kept.contains("(deadline 5.0s)"), "{kept}");
    assert!(kept.contains("(deadline 30s)"), "{kept}");
    assert!(s.request(0).contains("resend it, or drop it with"));
}

async fn status_reply(kernel: &Arc<Kernel>, to: &AgentId, payload: Value) -> Value {
    kernel.operator_send(vec![Item::new(to.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// Sends `to` an inquiry, as `from` (an agent) or as the operator (`None`).
async fn ask_inquiry(kernel: &Arc<Kernel>, to: &AgentId, from: Option<&AgentId>) {
    let item = vec![Item::new(
        to.clone(),
        json!({"type": "inquiry", "text": "ship?"}),
    )];
    let out = match from {
        Some(f) => as_agent(f.clone(), kernel.operator_send(item)).await,
        None => kernel.operator_send(item).await,
    };
    assert_eq!(
        out[0].result["accepted"],
        json!(true),
        "{:?}",
        out[0].result
    );
}

/// `status` names the askers of the inquiries the agent holds and has not answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_has_no_inquiry_from_when_nothing_is_open() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "iq_none").await;
    let bare = status_reply(&kernel, &backend, json!({"type": "status"})).await;
    assert_eq!(bare["inquiry_from"], json!([]), "{bare}");
    let mine = status_reply(
        &kernel,
        &backend,
        json!({"type": "status", "client_id": "pc"}),
    )
    .await;
    assert_eq!(mine["inquiry_from"], json!([]), "{mine}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_names_the_asker_of_an_open_inquiry() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "iq_one").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let _s = script(&backend, vec![pass(DONE)], Duration::from_millis(1500));
    ask_inquiry(&kernel, &backend, Some(&peer)).await;
    let bare = status_reply(&kernel, &backend, json!({"type": "status"})).await;
    assert_eq!(bare["inquiry_from"], json!([peer.as_str()]), "{bare}");
    let mine = status_reply(
        &kernel,
        &backend,
        json!({"type": "status", "client_id": "pc"}),
    )
    .await;
    assert_eq!(mine["inquiry_from"], json!([peer.as_str()]), "{mine}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_drops_an_inquiry_from_inquiry_from_once_it_is_answered() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "iq_answered").await;
    let s = script(
        &backend,
        vec![pass(answer_call("ship it"))],
        Duration::from_millis(300),
    );
    ask_inquiry(&kernel, &backend, None).await;
    let open = status_reply(&kernel, &backend, json!({"type": "status"})).await;
    assert_eq!(open["inquiry_from"], json!(["operator"]), "{open}");
    assert!(until_passes(&s, 1).await);
    let probe = (kernel.clone(), backend.clone());
    for _ in 0..100 {
        let now = status_reply(&probe.0, &probe.1, json!({"type": "status"})).await;
        if now["inquiry_from"] == json!([]) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the answered inquiry is still named in inquiry_from");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_names_two_askers_sorted() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "iq_two").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let _s = script(
        &backend,
        vec![pass(DONE), pass(DONE)],
        Duration::from_millis(1500),
    );
    ask_inquiry(&kernel, &backend, Some(&peer)).await;
    ask_inquiry(&kernel, &backend, None).await;
    let mut want = vec![peer.as_str().to_string(), "operator".to_string()];
    want.sort();
    let bare = status_reply(&kernel, &backend, json!({"type": "status"})).await;
    assert_eq!(bare["inquiry_from"], json!(want), "{bare}");
}
