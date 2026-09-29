//! The ONE watch path (connect-time auto-watch + explicit `watch` frame): the
//! name resolves the way `send` resolves a target (an id or `kernel`), the watch
//! runs under the leg's own identity, and a refusal is an `error` frame — never
//! silence, and never recorded as watched. Measured before: a socket whose
//! connect-time watch did not resolve mirrored nothing and said nothing, and the
//! explicit watch that followed was skipped as a duplicate. The twin of python
//! `web_ws/tests/test_watch_path.py` (a `file_bridge` stands in for python's
//! `exec_bridge`: any agent whose inbox has events will do).

use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TMessage;

/// The watched agent, id `files`. `grant` wires both gate ends between it and
/// the WS leg; without it the leg's watch is refused.
async fn watched_agent(kernel: &Arc<Kernel>, tmp: &TempDir, leg: &str, grant: bool) {
    let mut rec = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": "file_bridge.tools", "allow": ["."],
        "id": "files",
        "root": tmp.path().join("served").to_string_lossy(),
        "ingress_rule": "allow_all_ingress",
    });
    if grant {
        rec["callable_by"] = json!({leg: "*"});
    }
    let r = call(kernel, "kernel", rec).await;
    assert_eq!(r["id"], json!("files"), "{r}");
    if grant {
        let g = call(
            kernel,
            "kernel",
            json!({"type": "update_agent", "id": leg,
                   "can_call": {"kernel_state": "*", "files": "*"}}),
        )
        .await;
        assert!(g.get("error").is_none(), "grant the leg: {g}");
    }
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// The next TEXT frame, as JSON, within 5 s.
async fn next_text(ws: &mut TestWs) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("stream open")
            .expect("frame ok");
        if let TMessage::Text(t) = msg {
            return serde_json::from_str(&t).expect("json frame");
        }
    }
}

#[tokio::test]
async fn a_socket_opened_on_an_agent_receives_its_events() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_watch_path";
    let leg = format!("{web_id}_ws");
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    watched_agent(&kernel, &tmp, &leg, true).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/files/ws"))
        .await
        .expect("ws connect");
    // Let the auto-watch land before the traffic it must mirror.
    tokio::time::sleep(Duration::from_millis(150)).await;
    kernel
        .emit(
            &AgentId::from("files"),
            json!({"type": "job_done", "data": "v-42"}),
        )
        .await;
    let (t, _) = drain_frames(&mut ws, 600).await;
    assert!(
        t.iter()
            .any(|f| f["type"] == "event" && f["payload"]["type"] == "job_done"),
        "the agent's events reach the socket: {t:?}"
    );
    assert!(!t.iter().any(|f| f["type"] == "error"), "{t:?}");
    stop(&kernel, web_id).await;
}

#[tokio::test]
async fn a_refused_auto_watch_is_an_error_frame_not_silence() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_refused_watch";
    let leg = format!("{web_id}_ws");
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    watched_agent(&kernel, &tmp, &leg, false).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/files/ws"))
        .await
        .expect("ws connect");
    let first = next_text(&mut ws).await;
    assert_eq!(first["type"], "error", "{first}");
    assert_eq!(first["reason"], "unauthorized", "{first}");
    // NOT REGISTERED: the agent's events reach nobody on this socket.
    kernel
        .emit(
            &AgentId::from("files"),
            json!({"type": "job_done", "data": "leak"}),
        )
        .await;
    let (t, _) = drain_frames(&mut ws, 400).await;
    assert!(
        !t.iter().any(|f| f["type"] == "event"),
        "a refused watch mirrored: {t:?}"
    );
    // …and an explicit watch is re-tried, not skipped as a duplicate
    ws.send(TMessage::Text(
        json!({"type": "watch", "src": "files", "id": "w"}).to_string(),
    ))
    .await
    .expect("send");
    let again = next_text(&mut ws).await;
    assert_eq!(again["type"], "error", "{again}");
    assert_eq!(again["id"], "w", "{again}");
    stop(&kernel, web_id).await;
}

#[tokio::test]
async fn an_unknown_name_is_an_error_frame() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_unknown_watch";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/nobody/ws"))
        .await
        .expect("ws connect");
    let first = next_text(&mut ws).await;
    assert_eq!(first["type"], "error", "{first}");
    assert_eq!(first["reason"], "unknown_agent", "{first}");
    assert_eq!(first["src"], "nobody", "{first}");
    stop(&kernel, web_id).await;
}
