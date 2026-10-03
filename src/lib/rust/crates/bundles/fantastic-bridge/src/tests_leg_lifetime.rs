//! A bridge leg across its LIFETIME: a live record change, a sender restart — the
//! twin of python `ws_bridge/tests/test_leg_lifetime.py`.
//!
//! Audit 2026-10-03:
//!
//!   - ARCH-2 — the leg's rules were resolved ONCE at boot and cached, so an
//!     operator's `update_agent ingress_rule=deny_ingress` on a LIVE leg changed
//!     what `reflect` reported (sealed) and nothing the gate did: inbound calls
//!     kept being dispatched until the transport was torn down. The opening
//!     direction failed the same way. Now the leg reads its rules off the record
//!     per frame — the record is the truth the moment it is written.
//!   - IO-5 — a forward's frame id names the execution on the receiver, and it was
//!     `<leg>-<n>` with `n` counting from 1 at every boot. A restarted sender's
//!     first forward reused `<leg>-1`, which the long-lived receiver had already
//!     settled: the new write was JOINED to the old one, answered with its
//!     recorded outcome, and never ran. Now every boot mints a nonce into the ids.

use super::*;

/// One leg on a kernel (guid `local-g`), pinned to `peer-g`, its raw peer end in
/// hand; `notes` (a child of the leg) accepts the foreign caller `peer-g|ai`.
async fn leg(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, String, Arc<dyn BridgeTransport>) {
    let kernel = mk_kernel_guid(tmp, "local-g").await;
    let bid = id_for(tag, tmp);
    create_bridge(&kernel, &bid, "stand_in", "memory").await;
    patch(&kernel, &bid, "peer_guid", json!("peer-g"));
    let notes = ask(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": bid, "handler_module": "file_bridge.tools",
               "id": format!("notes_{bid}"), "root": tmp.path().to_string_lossy(),
               "callable_by": {"peer-g|ai": "*"}}),
    )
    .await;
    assert!(notes.get("error").is_none(), "{notes}");
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let booted = ask(&kernel, &bid, json!({"type": "boot"})).await;
    assert_eq!(booted["booted"], true, "{booted}");
    (kernel, bid, peer)
}

async fn inbound(peer: &Arc<dyn BridgeTransport>, bid: &str, n: u32) -> Value {
    peer.send_frame(json!({"type": "call", "id": format!("peer-call-{n}"),
                           "target": format!("notes_{bid}"), "payload": {"type": "reflect"},
                           "caller": "peer-g|ai"}))
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
/// execution on the receiver (audit IO-5).
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

// ─── IO-5: a restarted sender never joins a stale execution ─────────────────

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
    patch(&ka, &a_id, "peer_guid", json!("guid-b"));
    create_bridge(&kb, &b_id, "stand_in", "memory").await;
    patch(&kb, &b_id, "peer_guid", json!("guid-a"));
    let doc = format!("doc_{b_id}");
    let made = ask(
        &kb,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": b_id, "handler_module": "file_bridge.tools",
               "id": doc, "allow": ["."], "root": tb.path().to_string_lossy(),
               "ingress_rule": "allow_all_ingress", "callable_by": {"guid-a|*": "*"}}),
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
