//! #568: the AI-to-AI send deadline (root `send_timeout`), the dead letter and
//! the withdrawal at expiry, the deadline the model is told, and `status`
//! counting held senders.

use super::*;
use fantastic_kernel::test_support::as_agent;

const DONE: &str = "<call name=\"done\"></call>";

fn set_root_timeout(kernel: &Arc<Kernel>, v: Value) {
    kernel
        .root()
        .unwrap()
        .meta
        .write()
        .unwrap()
        .insert("send_timeout".into(), v);
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
        k.send(vec![item]).await.remove(0)
    })
}

/// A message held behind a busy target passes its sender's deadline while held:
/// its entry is withdrawn, a marker row says so, and no pass ever answers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_held_behind_a_busy_target_is_withdrawn_into_the_dead_letter_at_its_deadline() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_held").await;
    let s = script(
        &backend,
        vec![pass(DONE), pass(DONE)],
        Duration::from_millis(1500),
    );
    let first = spawn_send(&kernel, &backend, "one", "o", None);
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let started = std::time::Instant::now();
    let two = spawn_send(&kernel, &backend, "two", "t", Some(0.3));
    let out = two.await.unwrap();
    assert_eq!(out.status, fantastic_kernel::outcome::Status::TimedOut);
    let p = out.pid.expect("a pid").0;
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "queue time counts: {:?}",
        started.elapsed()
    );
    assert!(
        crate::state::state_for(&backend)
            .chain
            .lock()
            .unwrap()
            .held
            .is_empty(),
        "the held entry must be withdrawn"
    );
    let all = rows(&kernel, &backend).await;
    let withdrawn: Vec<&Value> = all.iter().filter(|r| r["kind"] == "withdrawn").collect();
    assert_eq!(withdrawn.len(), 1, "{all:?}");
    let content = withdrawn[0]["content"].as_str().unwrap();
    assert!(content.contains(&format!("dead letter {p}")), "{content}");
    let two_row = all.iter().find(|r| r["content"] == "two").expect("row two");
    assert_eq!(withdrawn[0]["parent"], two_row["id"]);
    let _ = first.await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        s.passes_served(),
        1,
        "no pass may answer the withdrawn message"
    );
}

/// REGRESSION PIN (it holds without #568's change): a message whose own pass is
/// running at its deadline is cut — no late finish — and settles timed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_whose_pass_is_running_at_its_deadline_is_cut_into_the_dead_letter() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_cut").await;
    let _s = script(&backend, vec![pass("late")], Duration::from_millis(1500));
    let out = spawn_send(&kernel, &backend, "go", "o", Some(0.3))
        .await
        .unwrap();
    assert_eq!(out.status, fantastic_kernel::outcome::Status::TimedOut);
    assert!(out.pid.is_some());
    assert!(out.result.get("response").is_none(), "{}", out.result);
    until_true("the generation to stop", || {
        !crate::state::is_generating(&backend)
    })
    .await;
    let all = rows(&kernel, &backend).await;
    assert!(all.iter().any(|r| r["kind"] == "stopped"), "{all:?}");
}

/// `status` answers at once while a pass runs, counts held messages as pending
/// and names every pending sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_counts_held_messages_and_names_their_senders() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_status").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let _s = script(
        &backend,
        vec![pass(DONE), pass(DONE)],
        Duration::from_millis(1500),
    );
    let (k, b) = (Arc::clone(&kernel), backend.clone());
    let _first = tokio::spawn(async move {
        k.send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": "o"}),
        )])
        .await
    });
    assert!(until_passes(&_s, 1).await);
    let (k, b, p) = (Arc::clone(&kernel), backend.clone(), peer.clone());
    let _held = tokio::spawn(async move {
        as_agent(
            p,
            k.send(vec![Item::new(
                b,
                json!({"type": "send", "text": "two", "client_id": "pc"}),
            )]),
        )
        .await
    });
    until_held(&backend, 1).await;
    let t = std::time::Instant::now();
    let bare = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(bare["generating"], json!(true), "{bare}");
    assert_eq!(bare["others_pending"], json!(1), "{bare}");
    assert_eq!(bare["pending_from"], json!([peer.as_str()]), "{bare}");
    let mine = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "status", "client_id": "pc"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        mine["mine_pending"][0]["text"]
            .as_str()
            .unwrap()
            .contains("two"),
        "{mine}"
    );
    assert_eq!(mine["others_pending"], json!(0), "{mine}");
    assert_eq!(mine["pending_from"], json!([peer.as_str()]), "{mine}");
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
    for bad in [json!(-1), json!("x"), json!(true)] {
        set_root_timeout(&kernel, bad);
        assert_eq!(crate::route::route_deadline(&kernel), 45.0);
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_reply_states_its_deadline_and_a_per_call_timeout_overrides_it() {
    let _lock = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _env = EnvGuard::set(None);
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_told").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    set_root_timeout(&kernel, json!(5));
    let _sp = script(&peer, vec![pass("ok"), pass("ok")], Duration::ZERO);
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{}{}",
                send_call(peer.as_str(), "a", None),
                send_call(peer.as_str(), "b", Some("30"))
            )),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "o"}),
        )])
        .await;
    assert!(until_passes(&s, 2).await);
    let req = s.request(1);
    assert!(req.contains("(deadline 5.0s)"), "{req}");
    assert!(req.contains("(deadline 30s)"), "{req}");
    assert!(s.request(0).contains("resend it, or drop it with"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_past_its_deadline_tells_the_sender_its_dead_letter_and_can_be_resent() {
    let _lock = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _env = EnvGuard::set(None);
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dl_resend").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    set_root_timeout(&kernel, json!(0.3));
    let sp = script(
        &peer,
        vec![pass(DONE), pass(DONE), pass("resent answer")],
        Duration::from_millis(1500),
    );
    // Grants: the backend may ack its own dead letters at the root.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "update_agent", "id": "kernel_state",
                "callable_by": {backend.as_str(): ["ack_deadletter"]},
            }),
        )])
        .await;
    let busy = spawn_send(&kernel, &peer, "busy", "o", None);
    assert!(until_passes(&sp, 1).await, "the peer never got busy");
    let s = script(
        &backend,
        vec![pass(send_call(peer.as_str(), "ping", None)), pass(DONE)],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "o"}),
        )])
        .await;
    assert!(until_passes(&s, 2).await);
    let req = s.request(1);
    let marker = format!("your message to {peer} hit its 0.3s deadline \u{2192} dead letter ");
    let at = req.find(&marker).unwrap_or_else(|| panic!("{req}"));
    let pid: String = req[at + marker.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    assert!(!pid.is_empty(), "{req}");
    let peer_rows = rows(&kernel, &peer).await;
    assert!(
        peer_rows
            .iter()
            .any(|r| r["kind"] == "withdrawn" && r["content"].as_str().unwrap().contains(&pid)),
        "{peer_rows:?}"
    );
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
    let _ = busy.await;
    until_true("the peer to be idle", || {
        !crate::state::is_generating(&peer)
            && !crate::state::state_for(&peer).chain.lock().unwrap().alive
    })
    .await;
    let resent = as_agent(
        backend.clone(),
        kernel.send(vec![Item::new(
            peer.clone(),
            json!({"type": "send", "text": "ping", "client_id": "r"}),
        )
        .with_timeout(10.0)]),
    )
    .await
    .remove(0)
    .result;
    assert!(resent.get("response").is_some(), "{resent}");
}
