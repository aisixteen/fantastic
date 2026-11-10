//! A bridge leg across its LIFETIME: a live record change, a sender restart — the
//! twin of python `ws_bridge/tests/test_leg_lifetime.py`.
//!
//!   - The leg reads its rules off the record per frame — never resolved once at
//!     boot and cached — so an operator's `update_agent
//!     ingress_rule=deny_ingress` on a LIVE leg changes what the gate does the
//!     moment it is written, in both directions.
//!   - A forward's frame id names the execution on the receiver, so every boot
//!     mints a nonce into the ids: a restarted sender's first forward must never
//!     reuse an id the long-lived receiver already settled (it would be JOINED to
//!     the old one, answered with its recorded outcome, and never run).

use super::*;
use std::sync::atomic::AtomicBool;

/// One leg on a kernel (guid `local-g`), its raw peer end in hand; `notes` (a
/// child of the leg) accepts its parent — the leg, which serves every inbound
/// call AS ITSELF.
async fn leg(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, String, Arc<dyn BridgeTransport>) {
    let kernel = mk_kernel_guid(tmp, "local-g").await;
    let bid = id_for(tag, tmp);
    create_bridge(&kernel, &bid, "stand_in", "memory").await;
    let notes = ask(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": bid, "handler_module": "file_bridge.tools",
               "id": format!("notes_{bid}"), "root": tmp.path().to_string_lossy(),
               "callable_by": {"$parent": "*"}}),
    )
    .await;
    assert!(notes.get("error").is_none(), "{notes}");
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let booted = ask(&kernel, &bid, json!({"type": "boot"})).await;
    assert_eq!(booted["booted"], true, "{booted}");
    // the leg's first frame names its kernel to the far side
    let hello = peer.recv_frame().await.expect("hello").into_value();
    assert_eq!(
        hello,
        json!({"type": "hello", "guid": "local-g", "leg": bid})
    );
    (kernel, bid, peer)
}

async fn inbound(peer: &Arc<dyn BridgeTransport>, bid: &str, n: u32) -> Value {
    peer.send_frame(json!({"type": "call", "id": format!("peer-call-{n}"),
                           "target": format!("notes_{bid}"), "payload": {"type": "reflect"}}))
        .await
        .unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv_frame())
        .await
        .expect("a reply")
        .expect("a frame")
        .into_value();
    reply["data"].clone()
}

async fn op(kernel: &Arc<Kernel>, payload: Value) -> Value {
    ask(kernel, "kernel_state", payload).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resealing_a_live_leg_refuses_the_next_inbound_call() {
    let tmp = TempDir::new().unwrap();
    let (k, bid, peer) = leg(&tmp, "brg_reseal").await;
    assert_eq!(inbound(&peer, &bid, 1).await["id"], format!("notes_{bid}"));
    let sealed = op(
        &k,
        json!({"type": "update_agent", "id": bid, "ingress_rule": "deny_ingress"}),
    )
    .await;
    assert_eq!(sealed["updated"], true, "{sealed}");
    let shown = ask(&k, &bid, json!({"type": "reflect"})).await;
    assert_eq!(shown["sealed"], true, "{shown}");
    let out = inbound(&peer, &bid, 2).await;
    assert_eq!(out["reason"], "unauthorized", "{out}");
    assert!(
        out.get("id").is_none(),
        "a sealed leg dispatches nothing: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_a_live_sealed_leg_admits_the_next_inbound_call() {
    let tmp = TempDir::new().unwrap();
    let (k, bid, peer) = leg(&tmp, "brg_reopen").await;
    op(
        &k,
        json!({"type": "update_agent", "id": bid, "ingress_rule": "deny_ingress"}),
    )
    .await;
    assert_eq!(inbound(&peer, &bid, 1).await["reason"], "unauthorized");
    let opened = op(
        &k,
        json!({"type": "update_agent", "id": bid, "ingress_rule": "allow_all_ingress"}),
    )
    .await;
    assert_eq!(opened["updated"], true, "{opened}");
    assert_eq!(inbound(&peer, &bid, 2).await["id"], format!("notes_{bid}"));
}

/// Run one forward and hand back the frame that went on the wire.
async fn forward_frame(k: &Arc<Kernel>, bid: &str, peer: &Arc<dyn BridgeTransport>) -> Value {
    let kk = Arc::clone(k);
    let id = bid.to_string();
    let task = tokio::spawn(async move {
        ask(
            &kk,
            &id,
            json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}}),
        )
        .await
    });
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv_frame())
        .await
        .expect("a frame")
        .expect("a frame")
        .into_value();
    peer.send_frame(json!({"type": "reply", "id": frame["id"], "data": {"ok": true}}))
        .await
        .unwrap();
    assert_eq!(task.await.unwrap(), json!({"ok": true}));
    frame
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_egress_change_stamps_the_next_frame() {
    let env = "FANTASTIC_GROUP_TOKEN_LEG_LIVE";
    std::env::set_var(env, "grp-1");
    let tmp = TempDir::new().unwrap();
    let (k, bid, peer) = leg(&tmp, "brg_egress").await;
    assert!(forward_frame(&k, &bid, &peer)
        .await
        .get("auth_token")
        .is_none());
    op(
        &k,
        json!({"type": "update_agent", "id": bid,
               "egress_rule": {"type": "password_egress", "env": env}}),
    )
    .await;
    assert_eq!(forward_frame(&k, &bid, &peer).await["auth_token"], "grp-1");
    op(
        &k,
        json!({"type": "update_agent", "id": bid, "egress_rule": "silent_egress"}),
    )
    .await;
    assert!(forward_frame(&k, &bid, &peer)
        .await
        .get("auth_token")
        .is_none());
}

/// An egress rule is resolved per forward now, so one the registry does not know
/// refuses that forward, loud — nothing goes on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_egress_rule_written_live_refuses_the_forward() {
    let tmp = TempDir::new().unwrap();
    let (k, bid, _peer) = leg(&tmp, "brg_badegress").await;
    op(
        &k,
        json!({"type": "update_agent", "id": bid, "egress_rule": "nope_egress"}),
    )
    .await;
    let out = ask(
        &k,
        &bid,
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}}),
    )
    .await;
    assert_eq!(
        out,
        json!({"error": "bridge: bad auth rule: unknown egress rule type 'nope_egress'"})
    );
}

/// `<bridge id>-<boot nonce>-<n>`: the id keeps bridges apart, the nonce keeps
/// BOOTS apart — the counter restarts at every boot, and a frame id names the
/// execution on the receiver.
#[test]
fn the_corr_prefix_is_the_leg_id_and_a_per_boot_nonce() {
    let a = new_corr_prefix(&AgentId::from("a"));
    assert!(a.starts_with("a-") && a.len() == "a-".len() + 12, "{a}");
    assert!(
        a["a-".len()..].bytes().all(|b| b.is_ascii_hexdigit()),
        "{a}"
    );
    assert!(!a.contains(':'), "the kernel-minted namespace stays out");
    assert_ne!(
        new_corr_prefix(&AgentId::from("a")),
        a,
        "every boot mints its own nonce"
    );
}

// ─── A restarted sender never joins a stale execution ─────────────────

async fn write_doc(ka: &Arc<Kernel>, a_id: &str, doc: &str, text: &str) -> Value {
    ask(
        ka,
        a_id,
        json!({"type": "forward", "target": doc,
               "payload": {"type": "write", "path": "doc.txt", "bytes": text}}),
    )
    .await
}

async fn boot_pair(ka: &Arc<Kernel>, a_id: &str, kb: &Arc<Kernel>, b_id: &str) {
    inject_pair(&AgentId::from(a_id), &AgentId::from(b_id));
    for (k, i) in [(ka, a_id), (kb, b_id)] {
        let r = ask(k, i, json!({"type": "reconnect"})).await;
        assert_eq!(r["booted"], true, "{r}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_sender_never_joins_a_settled_execution() {
    let (ta, tb) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let ka = mk_kernel_guid(&ta, "guid-a").await;
    let kb = mk_kernel_guid(&tb, "guid-b").await;
    let a_id = id_for("brg_restart_a", &ta);
    let b_id = id_for("brg_restart_b", &tb);
    create_bridge(&ka, &a_id, "stand_in", "memory").await;
    create_bridge(&kb, &b_id, "stand_in", "memory").await;
    let doc = format!("doc_{b_id}");
    let made = ask(
        &kb,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": b_id, "handler_module": "file_bridge.tools",
               "id": doc, "allow": ["."], "root": tb.path().to_string_lossy(),
               "ingress_rule": "allow_all_ingress", "callable_by": {"$parent": "*"}}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    boot_pair(&ka, &a_id, &kb, &b_id).await;
    let first = write_doc(&ka, &a_id, &doc, "v1").await;
    assert_eq!(first["size"], 2, "{first}");
    // THE SENDER RESTARTS: its leg state is gone and rebuilt; the receiver keeps
    // running, every settled execution still named.
    boot_pair(&ka, &a_id, &kb, &b_id).await;
    let second = write_doc(&ka, &a_id, &doc, "v2-NEW").await;
    assert_eq!(second["size"], 6, "{second}");
    let read = ask(&kb, &doc, json!({"type": "read", "path": "doc.txt"})).await;
    assert_eq!(read["lines"][0]["text"], "v2-NEW", "{read}");
}

// ─── A drop mid-turn releases the called agent's turn ─────────────────

/// An agent whose turn holds state only `on_cancel` frees — the shape of an AI
/// agent's `generating`: dropping the turn's future releases nothing. `turn`
/// with `park` starts a turn that never ends on its own; a `turn` while one is
/// held is refused as busy (the later send that hangs on a wedged agent).
struct TurnProbe {
    busy: Arc<AtomicBool>,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Bundle for TurnProbe {
    fn name(&self) -> &str {
        "turn_probe.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload["type"].as_str().unwrap_or("") {
            "turn" => {
                if self.busy.swap(true, Ordering::SeqCst) {
                    return Ok(Some(json!({"error": "busy"})));
                }
                if payload["park"] == json!(true) {
                    self.started.notify_one();
                    std::future::pending::<()>().await;
                }
                self.busy.store(false, Ordering::SeqCst);
                Ok(Some(json!({"answered": true})))
            }
            "status" => Ok(Some(
                json!({"generating": self.busy.load(Ordering::SeqCst)}),
            )),
            _ => Ok(Some(
                json!({"id": id.0, "sentence": "turn probe", "verbs": {}}),
            )),
        }
    }
    async fn on_cancel(
        &self,
        _id: &AgentId,
        _task: &fantastic_kernel::TaskId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.busy.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// The connection drops while the peer's call is mid-turn on a local agent: the
/// turn ends through `cancel` (its state released), so after a reconnect the
/// next call is served and `status` is idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_mid_turn_releases_the_called_agent() {
    let tmp = TempDir::new().unwrap();
    let busy = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    kernel.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
    kernel.bundles.register(
        "turn_probe.tools",
        TurnProbe {
            busy: Arc::clone(&busy),
            started: Arc::clone(&started),
        },
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let bid = id_for("brg_dropturn", &tmp);
    create_bridge(&kernel, &bid, "stand_in", "memory").await;
    let agent = ask(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": bid, "handler_module": "turn_probe.tools",
               "id": format!("turner_{bid}"), "callable_by": {"$parent": "*"}}),
    )
    .await;
    assert!(agent.get("error").is_none(), "{agent}");
    let target = format!("turner_{bid}");
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let mut events = kernel.ensure_inbox(&AgentId::from(bid.as_str()));
    let booted = ask(&kernel, &bid, json!({"type": "boot"})).await;
    assert_eq!(booted["booted"], true, "{booted}");

    // the peer's call starts a turn that does not end on its own
    peer.send_frame(
        json!({"type": "call", "id": "peer-turn-1", "target": target,
                           "payload": {"type": "turn", "park": true}}),
    )
    .await
    .unwrap();
    started.notified().await;
    assert!(busy.load(Ordering::SeqCst), "the turn is in flight");

    // the connection drops mid-turn; `bridge_down` is the leg's last word, after
    // the in-flight dispatch has been ended
    peer.close().await;
    loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("bridge_down arrives")
            .expect("the inbox stays open");
        if ev.payload["type"] == "bridge_down" {
            break;
        }
    }
    assert!(
        !busy.load(Ordering::SeqCst),
        "the drop ended the turn cleanly: its state is released"
    );
    assert_eq!(
        ask(&kernel, &target, json!({"type": "status"})).await["generating"],
        false
    );

    // reconnect, and the next call is served
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let again = ask(&kernel, &bid, json!({"type": "reconnect"})).await;
    assert_eq!(again["booted"], true, "{again}");
    peer.send_frame(
        json!({"type": "call", "id": "peer-turn-2", "target": target,
                           "payload": {"type": "turn"}}),
    )
    .await
    .unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(10), peer.recv_frame())
        .await
        .expect("the next call is answered")
        .expect("a frame")
        .into_value();
    assert_eq!(reply["data"], json!({"answered": true}), "{reply}");
}
