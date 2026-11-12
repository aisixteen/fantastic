//! THE TWO-WAY LEG (issue #476), on the wire: a `ws_bridge` says `hello {guid,
//! leg}` first at every boot, and serves an inbound `watch` of a local agent —
//! gated by its ingress rule and its own `watch` grant — by streaming that
//! agent's events back until `unwatch`. The far end here is the test, holding the
//! memory transport's other half. The twin of python
//! `ws_bridge/tests/test_two_way_leg.py`; the whole round trip against a real
//! `web_ws` is `fantastic-web`'s `tests_two_way.rs`.

use super::*;

async fn next(peer: &Arc<dyn BridgeTransport>) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv_frame())
        .await
        .expect("a frame in time")
        .expect("a frame")
        .into_value()
}

/// A ws leg `bid` (SEALED, no grants) on a kernel with `guid`, booted on a
/// memory half, and a plain agent `notes_<bid>` to watch. Returns the far end,
/// its hello already read.
async fn sealed_leg(
    tmp: &TempDir,
    tag: &str,
) -> (Arc<Kernel>, String, String, Arc<dyn BridgeTransport>) {
    let kernel = mk_kernel_guid(tmp, "near-g").await;
    let bid = id_for(tag, tmp);
    let made = ask(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel", "id": bid,
               "handler_module": WS_HANDLER_MODULE, "transport": "memory",
               "peer_id": "far"}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let notes = format!("notes_{bid}");
    let made = ask(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel", "id": notes,
               "handler_module": "file_bridge.tools", "root": tmp.path().to_string_lossy()}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let booted = ask(&kernel, &bid, json!({"type": "boot"})).await;
    assert_eq!(booted["booted"], true, "{booted}");
    assert_eq!(
        next(&peer).await,
        json!({"type": "hello", "guid": "near-g", "leg": bid})
    );
    (kernel, bid, notes, peer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_goes_first_at_every_boot_and_only_with_a_guid() {
    let tmp = TempDir::new().unwrap();
    let (kernel, bid, _notes, _peer) = sealed_leg(&tmp, "tw_hello").await;
    // a redial says it again, on the new link
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let r = ask(&kernel, &bid, json!({"type": "reconnect"})).await;
    assert_eq!(r["booted"], true, "{r}");
    assert_eq!(
        next(&peer).await,
        json!({"type": "hello", "guid": "near-g", "leg": bid})
    );
    // a kernel with no guid has no address to give: the first frame is the call
    let tmp2 = TempDir::new().unwrap();
    let bare = mk_kernel(&tmp2).await;
    let b2 = id_for("tw_noguid", &tmp2);
    create_bridge(&bare, &b2, "far", "memory").await;
    let peer2 = inject_one(&AgentId::from(b2.as_str()));
    let booted = ask(&bare, &b2, json!({"type": "boot"})).await;
    assert_eq!(booted["booted"], true, "{booted}");
    let k = Arc::clone(&bare);
    let id = b2.clone();
    let fwd = tokio::spawn(async move {
        ask(
            &k,
            &id,
            json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "forward_timeout": 30.0}),
        )
        .await
    });
    let first = next(&peer2).await;
    assert_eq!(first["type"], "call", "{first}");
    peer2
        .send_frame(json!({"type": "reply", "id": first["id"], "data": {"ok": true}}))
        .await
        .unwrap();
    assert_eq!(fwd.await.unwrap(), json!({"ok": true}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbound_watch_is_gated_then_streams_until_unwatch() {
    let tmp = TempDir::new().unwrap();
    let (kernel, bid, notes, peer) = sealed_leg(&tmp, "tw_watch").await;
    let watch = json!({"type": "watch", "src": notes});

    // sealed: the ingress rule refuses, and says so
    peer.send_frame(watch.clone()).await.unwrap();
    let refused = next(&peer).await;
    assert_eq!(
        (&refused["type"], &refused["src"], &refused["reason"]),
        (&json!("error"), &json!(notes), &json!("unauthorized")),
        "{refused}"
    );
    // opened, but the leg holds no `watch` on the agent: the kernel's gate refuses
    patch(&kernel, &bid, "ingress_rule", json!("allow_all_ingress"));
    peer.send_frame(watch.clone()).await.unwrap();
    let ungranted = next(&peer).await;
    assert_eq!(ungranted["reason"], "unauthorized", "{ungranted}");
    assert_eq!(ungranted["side"], "egress", "{ungranted}");
    // a name that is no agent
    peer.send_frame(json!({"type": "watch", "src": "nobody"}))
        .await
        .unwrap();
    let unknown = next(&peer).await;
    assert_eq!(unknown["reason"], "unknown_agent", "{unknown}");
    assert!(!kernel.has_watchers(&AgentId::from(notes.as_str())).await);

    // granted both ends: every event of the agent streams back, stamped `src`
    patch(&kernel, &bid, "can_call", json!({notes.as_str(): "*"}));
    patch(&kernel, &notes, "callable_by", json!({bid.as_str(): "*"}));
    peer.send_frame(watch.clone()).await.unwrap();
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !kernel.has_watchers(&AgentId::from(notes.as_str())).await {
        assert!(tokio::time::Instant::now() < end, "never watched");
        tokio::task::yield_now().await;
    }
    kernel
        .emit(
            &AgentId::from(notes.as_str()),
            json!({"type": "ping", "n": 1}),
        )
        .await;
    assert_eq!(
        next(&peer).await,
        json!({"type": "event", "src": notes, "payload": {"type": "ping", "n": 1}})
    );

    // unwatched: an event emitted before the next reply never reaches the wire
    peer.send_frame(json!({"type": "unwatch", "src": notes}))
        .await
        .unwrap();
    peer.send_frame(
        json!({"type": "call", "id": "after-unwatch", "target": notes,
                           "payload": {"type": "reflect"}}),
    )
    .await
    .unwrap();
    let reply = next(&peer).await;
    assert_eq!(reply["type"], "reply", "{reply}");
    assert!(!kernel.has_watchers(&AgentId::from(notes.as_str())).await);
    kernel
        .emit(
            &AgentId::from(notes.as_str()),
            json!({"type": "ping", "n": 2}),
        )
        .await;
    peer.send_frame(json!({"type": "call", "id": "after-ping", "target": notes,
                           "payload": {"type": "reflect"}}))
        .await
        .unwrap();
    let reply = next(&peer).await;
    assert_eq!(
        (&reply["type"], &reply["id"]),
        (&json!("reply"), &json!("after-ping")),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_follows_the_events_its_call_produced() {
    let tmp = TempDir::new().unwrap();
    let (kernel, bid, notes, peer) = sealed_leg(&tmp, "tw_order").await;
    patch(&kernel, &bid, "ingress_rule", json!("allow_all_ingress"));
    patch(&kernel, &bid, "can_call", json!({notes.as_str(): "*"}));
    patch(&kernel, &notes, "callable_by", json!({bid.as_str(): "*"}));
    peer.send_frame(json!({"type": "watch", "src": notes}))
        .await
        .unwrap();
    // the call frame follows the watch on the same link: the watch is in place
    // first, so the call's own inbound payload is mirrored — and goes out BEFORE
    // the call's reply
    peer.send_frame(json!({"type": "call", "id": "c1", "target": notes,
                           "payload": {"type": "reflect"}}))
        .await
        .unwrap();
    let mut before = Vec::new();
    loop {
        let f = next(&peer).await;
        if f["type"] == "reply" {
            assert_eq!(f["id"], "c1", "{f}");
            break;
        }
        before.push(f);
    }
    assert_eq!(
        before.first(),
        Some(&json!({"type": "event", "src": notes, "payload": {"type": "reflect"}})),
        "{before:?}"
    );
    assert!(before.iter().all(|f| f["type"] == "event"), "{before:?}");
    // the link goes: the far side's watch goes with it
    peer.close().await;
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while kernel.has_watchers(&AgentId::from(notes.as_str())).await {
        assert!(
            tokio::time::Instant::now() < end,
            "the watch outlived the link"
        );
        tokio::task::yield_now().await;
    }
}
