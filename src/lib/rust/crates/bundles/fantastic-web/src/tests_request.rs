//! The outbound `request` verb (issue #695): an allowed host answers; a host
//! outside the list, a redirect leaving it and an absolute-url trick are refused;
//! the host-bound credential is sent and never echoed; the body cap, the timeout,
//! JSON and bytes replies; the view's pure parts. Every server is in-process on
//! loopback — no test reaches the network.

use super::binary_steps::{create, kernel_with, op};
use super::*;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::Router;

const KEY: &str = "sk-REQ-0f3a9c71d2e8";

/// A loopback server: `/echo` (the Authorization it saw), `/leak` (the key in
/// body and header), `/json`, `/text`, `/bin`, `/big`, `/slow`, `/into/:port`
/// and `/out` (redirects), `/method` (the method and body it saw).
async fn server() -> u16 {
    let app = Router::new()
        .route(
            "/echo",
            get(|h: HeaderMap| async move {
                let a = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                axum::Json(json!({"auth": a, "has_cookie": h.contains_key("cookie")}))
            }),
        )
        .route(
            "/check",
            get(|h: HeaderMap| async move {
                let a = h.get("authorization").and_then(|v| v.to_str().ok());
                axum::Json(json!({"sent_key": a == Some(concat!("Bearer ", "sk-REQ-0f3a9c71d2e8")), "any_auth": a.is_some()}))
            }),
        )
        .route(
            "/leak",
            get(|h: HeaderMap| async move {
                let a = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                ([("x-seen", a.clone())], format!("you sent {a}"))
            }),
        )
        .route(
            "/json",
            get(|| async { axum::Json(json!({"a": [1, 2], "ok": true})) }),
        )
        .route("/text", get(|| async { "plain words" }))
        .route(
            "/bin",
            get(|| async {
                (
                    [("content-type", "application/octet-stream")],
                    vec![0xffu8, 0xfe, 0x00, 0x80],
                )
            }),
        )
        .route("/big", get(|| async { "x".repeat(BODY_CAP_TEST + 4096) }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        )
        .route(
            "/into/:port",
            get(|Path(port): Path<u16>| async move {
                (
                    StatusCode::FOUND,
                    [("location", format!("http://127.0.0.1:{port}/echo"))],
                )
                    .into_response()
            }),
        )
        .route(
            "/inside",
            get(|| async { (StatusCode::FOUND, [("location", "/json")]).into_response() }),
        )
        .route(
            "/method",
            any(
                |m: axum::http::Method, h: HeaderMap, b: String| async move {
                    let ct = h
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    axum::Json(json!({"method": m.as_str(), "body": b, "content_type": ct}))
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    port
}

const BODY_CAP_TEST: usize = 1_048_576;

/// A kernel with a credential store `creds` (a key bound to `bound_host`) and a
/// `web` agent `w` allowed to call `allow`. The web is not booted: `request`
/// needs no listener.
async fn world(allow: &[String], bound_host: &str) -> Arc<Kernel> {
    world_named("w", allow, bound_host).await
}

/// [`world`] with the web agent under `web` (the recent ring is keyed by agent id).
async fn world_named(web: &str, allow: &[String], bound_host: &str) -> Arc<Kernel> {
    let k = kernel_with(
        "R-g",
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::test_support::bundle(),
    )
    .await;
    let made = create(
        &k,
        json!({"handler_module": HANDLER_MODULE, "id": web, "port": free_port(),
               "request_allow": allow, "can": {"creds": {"call": ["*"]}}}),
    )
    .await;
    assert_eq!(made["id"], json!(web), "{made}");
    let made = create(
        &k,
        json!({"handler_module": fantastic_credential_store::HANDLER_MODULE, "id": "creds",
               "backend": "keychain", "refs": {"svc/main": {"host": bound_host}}}),
    )
    .await;
    assert_eq!(made["id"], json!("creds"), "{made}");
    let set = op(
        &k,
        "creds",
        json!({"type": "set", "ref": "svc/main", "fields": {"api_key": KEY}}),
    )
    .await;
    assert_eq!(set.result["stored"], json!(true), "{}", set.result);
    k
}

async fn request(k: &Arc<Kernel>, args: Value) -> Value {
    request_as(k, "w", args).await
}

async fn request_as(k: &Arc<Kernel>, web: &str, args: Value) -> Value {
    let mut call = json!({"type": "request"});
    for (key, v) in args.as_object().expect("args").clone() {
        call[key] = v;
    }
    op(k, web, call).await.result
}

fn base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_host_answers_json_text_and_bytes() {
    let port = server().await;
    let k = world(&[base(port)], "other.example").await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/json", base(port))}),
    )
    .await;
    assert_eq!(r["status"], json!(200), "{r}");
    assert_eq!(r["json"], json!({"a": [1, 2], "ok": true}));
    assert_eq!(r["truncated"], json!(false));
    assert!(r["headers"]["content-type"]
        .as_str()
        .unwrap()
        .contains("json"));
    let r = request(
        &k,
        json!({"method": "get", "url": format!("{}/text", base(port))}),
    )
    .await;
    assert_eq!(r["text"], json!("plain words"), "{r}");
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/bin", base(port))}),
    )
    .await;
    assert_eq!(r["bytes"], json!("//4AgA=="), "{r}");
    assert!(r.get("json").is_none() && r.get("text").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_json_body_and_a_write_method_reach_the_host() {
    let port = server().await;
    let k = world(&[base(port)], "other.example").await;
    let r = request(
        &k,
        json!({"method": "POST", "url": format!("{}/method?x=1", base(port)), "query": {"y": "two", "z": 3},
               "json": {"k": "v"}}),
    )
    .await;
    assert_eq!(r["json"]["method"], json!("POST"), "{r}");
    assert_eq!(r["json"]["body"], json!("{\"k\":\"v\"}"));
    assert!(r["json"]["content_type"]
        .as_str()
        .unwrap()
        .starts_with("application/json"));
    let r = request(
        &k,
        json!({"method": "PUT", "url": format!("{}/method", base(port)), "body": "raw"}),
    )
    .await;
    assert_eq!(r["json"]["body"], json!("raw"), "{r}");
    let both = request(
        &k,
        json!({"method": "POST", "url": format!("{}/method", base(port)), "json": {}, "body": "x"}),
    )
    .await;
    assert_eq!(both["reason"], json!("bad_arg"), "{both}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_outside_the_list_is_refused() {
    let (inside, outside) = (server().await, server().await);
    let k = world(&[base(inside)], "other.example").await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/text", base(outside))}),
    )
    .await;
    assert_eq!(r["reason"], json!("not_allowed"), "{r}");
    // the scheme is part of the match
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("https://127.0.0.1:{inside}/text")}),
    )
    .await;
    assert_eq!(r["reason"], json!("not_allowed"), "{r}");
    let r = request(&k, json!({"method": "GET", "url": "not a url"})).await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    let r = request(
        &k,
        json!({"method": "FETCH", "url": format!("{}/text", base(inside))}),
    )
    .await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_or_absent_allowlist_calls_nothing() {
    let port = server().await;
    let k = world(&[], "other.example").await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/text", base(port))}),
    )
    .await;
    assert_eq!(r["reason"], json!("disabled"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absolute_url_tricks_are_refused() {
    let (inside, outside) = (server().await, server().await);
    let k = world(&[base(inside)], "other.example").await;
    for url in [
        // user information naming the allowed host in front of another
        format!("http://127.0.0.1:{inside}@127.0.0.1:{outside}/text"),
        format!("http://127.0.0.1:{inside}:x@127.0.0.1:{outside}/text"),
        // the allowed host as a path or a query of another
        format!("http://127.0.0.1:{outside}/{}/text", base(inside)),
        format!("http://127.0.0.1:{outside}/text?u={}", base(inside)),
        // a backslash does not hide the real host
        format!("http://127.0.0.1:{outside}\\@127.0.0.1:{inside}/text"),
        // a protocol-relative or relative reference is no absolute URL
        format!("//127.0.0.1:{outside}/text"),
        "/text".to_string(),
    ] {
        let r = request(&k, json!({"method": "GET", "url": url})).await;
        assert!(
            matches!(r["reason"].as_str(), Some("not_allowed" | "bad_arg")),
            "{url} -> {r}"
        );
        assert!(r.get("status").is_none(), "{url} reached a host: {r}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_is_followed_inside_the_list_and_refused_out_of_it() {
    let (inside, outside) = (server().await, server().await);
    let k = world(&[base(inside)], "other.example").await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/inside", base(inside))}),
    )
    .await;
    assert_eq!(r["json"]["ok"], json!(true), "{r}");
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/into/{outside}", base(inside))}),
    )
    .await;
    assert_eq!(r["reason"], json!("not_allowed"), "{r}");
    // inside → inside again is allowed when both are listed
    let k = world(&[base(inside), base(outside)], "other.example").await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/into/{outside}", base(inside))}),
    )
    .await;
    assert_eq!(r["status"], json!(200), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_bound_credential_is_sent_only_to_its_host() {
    let (bound, other) = (server().await, server().await);
    let k = world(&[base(bound), base(other)], &format!("127.0.0.1:{bound}")).await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/check", base(bound))}),
    )
    .await;
    assert_eq!(r["json"]["sent_key"], json!(true), "{r}");
    // a listed host the ref is not bound to gets no key
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/echo", base(other))}),
    )
    .await;
    assert_eq!(r["json"]["auth"], Value::Null, "{r}");
    // a redirect to the other host does not carry it along
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/into/{other}", base(bound))}),
    )
    .await;
    assert_eq!(r["json"]["auth"], Value::Null, "{r}");
    // the caller may not pass its own
    for h in ["Authorization", "cookie", "Proxy-Authorization"] {
        let r = request(
            &k,
            json!({"method": "GET", "url": format!("{}/echo", base(bound)), "headers": {h: "x"}}),
        )
        .await;
        assert_eq!(r["reason"], json!("bad_arg"), "{h}: {r}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_credential_is_never_echoed_in_a_reply_or_an_error() {
    let port = server().await;
    let k = world(
        &[base(port), "http://127.0.0.1:1".to_string()],
        &format!("127.0.0.1:{port}"),
    )
    .await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/leak", base(port))}),
    )
    .await;
    assert_eq!(r["status"], json!(200), "{r}");
    assert!(!r.to_string().contains(KEY), "reply carries the key: {r}");
    assert_eq!(r["text"], json!("you sent Bearer [secret]"), "{r}");
    assert_eq!(r["headers"]["x-seen"], json!("Bearer [secret]"));
    // an error from a bound host
    let k2 = world(&["http://127.0.0.1:1".to_string()], "127.0.0.1:1").await;
    let r = request(&k2, json!({"method": "GET", "url": "http://127.0.0.1:1/x"})).await;
    assert_eq!(r["reason"], json!("transport_error"), "{r}");
    assert!(!r.to_string().contains(KEY), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_body_is_capped_and_says_so() {
    let port = server().await;
    let k = world(&[base(port)], "other.example").await;
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/big", base(port))}),
    )
    .await;
    assert_eq!(r["truncated"], json!(true), "{}", r["status"]);
    assert_eq!(r["text"].as_str().unwrap().len(), BODY_CAP_TEST);
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/text", base(port))}),
    )
    .await;
    assert_eq!(r["truncated"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_host_times_out_and_a_bad_timeout_is_refused() {
    let port = server().await;
    let k = world(&[base(port)], "other.example").await;
    let t = std::time::Instant::now();
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/slow", base(port)), "timeout": 0.3}),
    )
    .await;
    assert_eq!(r["reason"], json!("timeout"), "{r}");
    assert!(t.elapsed() < Duration::from_secs(3));
    let r = request(
        &k,
        json!({"method": "GET", "url": format!("{}/text", base(port)), "timeout": 9999}),
    )
    .await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_carries_the_allowlist_the_ref_name_the_recent_rows_and_the_view() {
    let port = server().await;
    let k = world_named("w_refl", &[base(port)], &format!("127.0.0.1:{port}")).await;
    request_as(
        &k,
        "w_refl",
        json!({"method": "GET", "url": format!("{}/text", base(port))}),
    )
    .await;
    request_as(
        &k,
        "w_refl",
        json!({"method": "POST", "url": format!("{}/method", base(port)), "body": "b"}),
    )
    .await;
    let r = op(&k, "w_refl", json!({"type": "reflect"})).await.result;
    assert_eq!(r["request_allow"], json!([base(port)]), "{r}");
    assert_eq!(
        r["request_credentials"][base(port)],
        json!("creds/svc/main")
    );
    let rows = r["recent_requests"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["method"], json!("POST"));
    assert_eq!(rows[0]["host"], json!(format!("127.0.0.1:{port}")));
    assert_eq!(rows[1]["status"], json!(200));
    assert!(!r.to_string().contains(KEY), "{r}");
    assert_eq!(r["view"]["kind"], json!("html"));
    assert_eq!(r["view"]["calls"], json!({"w_refl": ["reflect", "watch"]}));
    let html = r["view"]["bytes"].as_str().unwrap();
    assert!(
        html.contains("var AGENT = \"w_refl\"") || html.contains("AGENT = \"w_refl\""),
        "agent id filled"
    );
    assert!(!html.contains("{{agent}}"));
}

/// THE AGENT PUBLISHES EACH REQUEST IT SERVES (`request_served` on its own inbox), the
/// failed ones too: the view watching it re-reads `recent_requests` on the event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_request_served_is_published_on_the_agents_inbox() {
    let port = server().await;
    let k = world_named("w_pub", &[base(port)], &format!("127.0.0.1:{port}")).await;
    let mut rx = k.ensure_inbox(&AgentId::from("w_view"));
    assert!(fantastic_kernel::send::as_operator(k.watch(&AgentId::from("w_pub"), AgentId::from("w_view"))).await.is_none());
    request_as(&k, "w_pub", json!({"method": "GET", "url": format!("{}/text", base(port))})).await;
    request_as(&k, "w_pub", json!({"method": "GET", "url": format!("{}/slow", base(port)), "timeout": 0.3})).await;
    let mut got = Vec::new();
    while let Ok(Some(it)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        if it.payload["type"] == json!("request_served") {
            got.push(it.payload.clone());
        }
    }
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!((&got[0]["method"], &got[0]["status"]), (&json!("GET"), &json!(200)), "{got:?}");
    assert_eq!(got[0]["host"], json!(format!("127.0.0.1:{port}")));
    assert_eq!(got[1]["status"], Value::Null, "a request that failed is published too: {got:?}");
}

/// The view's pure parts, run in node when it is installed (they are plain JS).
#[test]
fn the_views_pure_parts() {
    let html = include_str!("request_view.html");
    let script = html
        .split("<script>")
        .nth(1)
        .and_then(|s| s.split("// pure").nth(1))
        .expect("pure section");
    let pure = script.split("(function ()").next().expect("pure end");
    assert!(pure.contains("function isWrite") && pure.contains("function statusClass"));
    assert!(
        !html.contains("innerHTML"),
        "the view writes text nodes only"
    );
    let node = std::process::Command::new("node").arg("--version").output();
    if node.map(|o| o.status.success()).unwrap_or(false) {
        let prog = format!(
            "{pure}\nconst a=require('assert');\
             a.strictEqual(isWrite('get'),false);a.strictEqual(isWrite('HEAD'),false);\
             a.strictEqual(isWrite('POST'),true);a.strictEqual(isWrite('delete'),true);\
             a.strictEqual(statusClass(200),'ok');a.strictEqual(statusClass(404),'err');a.strictEqual(statusClass(null),'err');\
             a.strictEqual(credLabel('creds/x'),'creds/x');a.strictEqual(credLabel(null),'none');\
             a.strictEqual(clock(0).length,8);"
        );
        let out = std::process::Command::new("node")
            .arg("-e")
            .arg(&prog)
            .output()
            .expect("node");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
