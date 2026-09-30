//! THE EXPERIMENT the web_ui plan starts with (tmp/web_ui_plan.md §9.1-2),
//! before any UI code: does the kernel's `web` host, UNCHANGED, carry a browser
//! View built outside the kernel tree?
//!
//! 1. A `web_ui` child's declared websocket route is mounted, and a frame on it
//!    runs AS `web_ui` (its reflect answers, a refusal names `web_ui`).
//! 2. A static page is served through `web`'s file proxy from a `ui_assets`
//!    file_bridge.
//! 3. The host binds loopback, and the Origin guard refuses a foreign page while
//!    admitting the host's own.
//! 4. `state_subscribe` works for `web_ui` once the root grant is named.
//!
//! One test per binary: `file_bridge` clamps against the process cwd, so the
//! test must chdir.

use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{AgentId, BundleRegistry, Item, Kernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

fn registry() -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    reg.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
    reg.register(fantastic_web::HANDLER_MODULE, fantastic_web::WebBundle);
    reg.register(fantastic_file::HANDLER_MODULE, fantastic_file::FileBundle::default());
    reg.register(fantastic_web_ui::HANDLER_MODULE, fantastic_web_ui::WebUiBundle);
    reg
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from(target), payload)]).await.remove(0).result
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let code = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (code, text)
}

async fn ws(port: u16, origin: Option<&str>) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let mut req = format!("ws://127.0.0.1:{port}{}", fantastic_web_ui::WS_PATH).into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("Origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(s, _)| s).map_err(|e| e.to_string())
}

async fn frame(s: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(3), s.next()).await.expect("a frame in 3 s").unwrap().unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_web_host_carries_a_view_built_outside_the_kernel() {
    let dir = std::env::temp_dir().join(format!("fc-webui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ui")).unwrap();
    std::fs::write(dir.join("ui/index.html"), "<!doctype html><title>fantastic</title>hello view").unwrap();
    std::env::set_current_dir(&dir).unwrap();

    let booted = bootstrap::bootstrap(registry(), BootstrapOptions::daemon(&dir)).expect("boots");
    let k = Arc::clone(&booted.kernel);
    let root = bootstrap::DEFAULT_ROOT_ID;
    let port = free_port();

    let r = op(&k, "kernel", json!({"type":"create_agent","parent_id":root,"id":"web","handler_module":"web.tools","port":port})).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = op(&k, "kernel", json!({"type":"create_agent","parent_id":"web","id":"web_ui","handler_module":"web_ui.tools",
        "ingress_rule":"allow_all_ingress",
        "can_call":{root:["state_subscribe"]}})).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = op(&k, "kernel", json!({"type":"create_agent","parent_id":"web","id":"ui_assets","handler_module":"file_bridge.tools",
        "root":"ui","readonly":true,"allow":["index.html"],"ingress_rule":"allow_all_ingress"})).await;
    assert!(r.get("error").is_none(), "{r}");
    // The root must name web_ui on its side of the gate too.
    let mut cb = k.agents.get(&AgentId::from(root)).unwrap().value().record().meta
        .get("callable_by").and_then(Value::as_object).cloned().unwrap_or_default();
    cb.insert("web_ui".into(), json!(["state_subscribe"]));
    let r = op(&k, "kernel", json!({"type":"update_agent","id":root,"callable_by":cb})).await;
    assert!(r.get("error").is_none(), "{r}");

    let boot = op(&k, "web", json!({"type":"boot"})).await;
    assert_eq!(boot["running"], true, "{boot}");
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 3 · loopback, and the reflect says so.
    let refl = op(&k, "web", json!({"type":"reflect"})).await;
    eprintln!("web reflect host={} allowed_origins={}", refl["host"], refl["allowed_origins"]);
    assert_eq!(refl["host"], "127.0.0.1");

    // 2 · the page, through the file proxy.
    let (code, body) = http_get(port, "/ui_assets/file/index.html").await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("hello view"));

    // 1 · the socket runs AS web_ui.
    let mut s = ws(port, None).await.expect("no Origin (a CLI) is admitted");
    s.send(Message::Text(json!({"type":"call","target":"web_ui","payload":{"type":"reflect"},"id":"a"}).to_string())).await.unwrap();
    let f = frame(&mut s).await;
    assert_eq!(f["data"]["ws_path"], fantastic_web_ui::WS_PATH, "{f}");
    s.send(Message::Text(json!({"type":"call","target":"ui_assets","payload":{"type":"list","path":""},"id":"b"}).to_string())).await.unwrap();
    let f = frame(&mut s).await;
    let err = f["data"]["error"].as_str().unwrap_or("");
    assert!(err.contains("web_ui may not call"), "a call is gated AS web_ui: {f}");

    // 4 · the state stream, once the root grant is named on both sides.
    s.send(Message::Text(json!({"type":"state_subscribe","id":"c"}).to_string())).await.unwrap();
    let f = frame(&mut s).await;
    assert_eq!(f["type"], "state_snapshot", "{f}");

    // 3 · Origin: a foreign page is refused, the host's own is admitted.
    let bad = ws(port, Some("http://evil.example")).await;
    assert!(bad.as_ref().is_err_and(|e| e.contains("403")), "foreign origin: {:?}", bad.err());
    let own = ws(port, Some(&format!("http://127.0.0.1:{port}"))).await;
    assert!(own.is_ok(), "own origin: {:?}", own.err());
    let own = ws(port, Some(&format!("http://localhost:{port}"))).await;
    assert!(own.is_ok(), "localhost alias: {:?}", own.err());

    let _ = op(&k, "web", json!({"type":"stop"})).await;
    let _ = std::fs::remove_dir_all(&dir);
}
