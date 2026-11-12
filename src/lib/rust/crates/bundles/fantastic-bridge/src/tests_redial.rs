//! THE AUTOMATIC REDIAL (issue #493), on a real socket: a `ws` leg whose link
//! dropped dials its known address again by itself — no `reconnect` — keeps the
//! watches it held, and answers the next call. The far end here is the test, a
//! `tokio-tungstenite` server it closes and reopens on the same port. Nothing
//! waits on a clock: the test awaits the leg's own `bridge_down` / `bridge_up`
//! and the next accept, each bounded.

use super::*;
use futures_util::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type Far = WebSocketStream<tokio::net::TcpStream>;

const BOUND: std::time::Duration = std::time::Duration::from_secs(20);

async fn accept(listener: &TcpListener) -> Far {
    let (stream, _) = tokio::time::timeout(BOUND, listener.accept())
        .await
        .expect("the leg dialled in time")
        .unwrap();
    tokio_tungstenite::accept_async(stream).await.unwrap()
}

async fn frame(far: &mut Far) -> Value {
    let msg = tokio::time::timeout(BOUND, far.next())
        .await
        .expect("a frame in time")
        .expect("the link is up")
        .expect("a frame");
    serde_json::from_str(msg.to_text().expect("a text frame")).unwrap()
}

/// Read the leg's inbox until an item of `kind` (and, when given, `src`) lands.
async fn until(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    kind: &str,
    src: Option<&str>,
) -> Value {
    tokio::time::timeout(BOUND, async {
        loop {
            let item = rx.recv().await.expect("the inbox stays open");
            let p = item.payload;
            if p["type"] == kind && src.is_none_or(|s| p["src"] == s) {
                return p;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} on the leg's inbox"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_link_is_redialled_by_the_leg_with_its_watches_and_answers_again() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel_guid(&tmp, "near-g").await;
    let bid = id_for("rd_ws", &tmp);
    create_bridge(&kernel, &bid, "far", "ws").await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    patch(&kernel, &bid, "host", json!("127.0.0.1"));
    patch(&kernel, &bid, "port", json!(port));
    let mut rx = kernel.ensure_inbox(&AgentId::from(bid.as_str()));

    let (booted, mut far) = tokio::join!(
        ask(&kernel, &bid, json!({"type": "boot"})),
        accept(&listener)
    );
    assert_eq!(booted["booted"], true, "{booted}");
    let hello = json!({"type": "hello", "guid": "near-g", "leg": bid});
    assert_eq!(frame(&mut far).await, hello);
    let w = ask(
        &kernel,
        &bid,
        json!({"type": "watch_remote", "target": "far_agent"}),
    )
    .await;
    assert_eq!(w["ok"], true, "{w}");
    assert_eq!(
        frame(&mut far).await,
        json!({"type": "watch", "src": "far_agent"})
    );

    // the link drops; the leg's first redial reaches a far end that hangs up
    // before the handshake, so it is backing off when the next dial comes
    drop(far);
    until(&mut rx, "bridge_down", None).await;
    let (refused, _) = tokio::time::timeout(BOUND, listener.accept())
        .await
        .expect("the leg redialled in time")
        .unwrap();
    drop(refused);

    // no `reconnect` is sent: the leg dials in by itself, says hello first, and
    // asks again for the agent it was watching
    let mut far = accept(&listener).await;
    assert_eq!(frame(&mut far).await, hello);
    assert_eq!(
        frame(&mut far).await,
        json!({"type": "watch", "src": "far_agent"})
    );
    until(&mut rx, "bridge_up", None).await;

    // the watch made before the drop keeps delivering to its local watcher
    far.send(Message::Text(
        json!({"type": "event", "src": "far_agent", "payload": {"type": "tick", "n": 1}})
            .to_string(),
    ))
    .await
    .unwrap();
    let tick = until(&mut rx, "tick", Some("far_agent")).await;
    assert_eq!(tick["n"], 1, "{tick}");

    // and the next call answers
    let k = Arc::clone(&kernel);
    let id = bid.clone();
    let fwd = tokio::spawn(async move {
        ask(
            &k,
            &id,
            json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "forward_timeout": 30.0}),
        )
        .await
    });
    let call = frame(&mut far).await;
    assert_eq!(call["type"], "call", "{call}");
    far.send(Message::Text(
        json!({"type": "reply", "id": call["id"], "data": {"ok": true}}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(fwd.await.unwrap(), json!({"ok": true}));
}

/// The wait before each redial attempt: at once, then one second, doubling to
/// thirty.
#[test]
fn the_redial_backoff_starts_at_once_doubles_and_is_capped() {
    let secs: Vec<u64> = (0..9).map(|n| redial_delay(n).as_secs()).collect();
    assert_eq!(secs, vec![0, 1, 2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(redial_delay(u32::MAX), REDIAL_MAX_DELAY);
}
