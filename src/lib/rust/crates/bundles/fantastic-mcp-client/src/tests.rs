//! mcp_client — python `mcp_client/tests/test_mcp_client.py`, case for case:
//! against a fake 2026-07-28 server (wiremock) that enforces the spec's request
//! rules (headers mirror the body, `_meta` on every request, the Bearer key),
//! through a real credential store (file backend under a temp dir).

use super::*;
use fantastic_credential_store::test_support as creds;
use fantastic_kernel::outcome::Step;
use fantastic_kernel::{Item, Status};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "mcp-key-0123456789";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01raw";

fn tools() -> Vec<Value> {
    serde_json::from_value(json!([
        {"name": "get_weather", "title": "Weather",
         "description": "Current weather for a city. Units default to metric.",
         "inputSchema": {"type": "object", "properties": {
             "city": {"type": "string"}, "days": {"type": "integer"},
             "region": {"type": "string", "x-mcp-header": "Region"}},
             "required": ["city"]}},
        {"name": "fail", "description": "Always a tool error.", "inputSchema": {"type": "object"}},
        {"name": "picture", "inputSchema": {"type": "object", "additionalProperties": false}},
        {"name": "stream", "description": "Answers over SSE with progress.", "inputSchema": {"type": "object"}},
        {"name": "structured", "inputSchema": {"type": "object"}},
        {"name": "ask", "inputSchema": {"type": "object"}},
        {"name": "weird", "inputSchema": {"type": "object"}},
        {"name": "cut", "inputSchema": {"type": "object"}},
        {"name": "reflect", "inputSchema": {"type": "object"}},
        {"name": "ps", "inputSchema": {"type": "object"}},
        {"name": "fail", "inputSchema": {"type": "object"}},
        {"name": "bad_header", "inputSchema": {"type": "object", "properties": {
            "xs": {"type": "array", "items": {"type": "string", "x-mcp-header": "X"}}}}}
    ]))
    .unwrap()
}

/// What the fake saw: each request's headers (lowercased names) and body.
type Seen = Arc<StdMutex<Vec<(HashMap<String, String>, Value)>>>;

/// A 2026-07-28 server: validates what the spec makes it validate, records every
/// request, answers by tool.
struct FakeMcp {
    key: Option<&'static str>,
    versions: Vec<&'static str>,
    seen: Seen,
}

fn ok(body: &Value, result: Value) -> ResponseTemplate {
    let mut r = json!({"resultType": "complete"});
    for (k, v) in result.as_object().unwrap() {
        r[k] = v.clone();
    }
    ResponseTemplate::new(200)
        .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": r}))
}

fn err(status: u16, body: &Value, code: i64, msg: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(
        json!({"jsonrpc": "2.0", "id": body["id"], "error": {"code": code, "message": msg}}),
    )
}

impl Respond for FakeMcp {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let h: HashMap<String, String> = req
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        self.seen.lock().unwrap().push((h.clone(), body.clone()));
        let get = |k: &str| h.get(k).cloned().unwrap_or_default();
        if let Some(key) = self.key {
            if get("authorization") != format!("Bearer {key}") {
                return ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer");
            }
        }
        let meta = &body["params"]["_meta"];
        if meta
            .get("io.modelcontextprotocol/protocolVersion")
            .is_none()
            || meta
                .get("io.modelcontextprotocol/clientCapabilities")
                .is_none()
        {
            return err(400, &body, -32602, "missing _meta");
        }
        if get("mcp-protocol-version") != meta["io.modelcontextprotocol/protocolVersion"] {
            return err(400, &body, -32020, "protocol header mismatch");
        }
        if get("mcp-method") != body["method"].as_str().unwrap() {
            return err(400, &body, -32020, "Mcp-Method mismatch");
        }
        let accept = get("accept");
        if !accept.contains("application/json") || !accept.contains("text/event-stream") {
            return ResponseTemplate::new(406);
        }
        match body["method"].as_str().unwrap() {
            "server/discover" => ok(
                &body,
                json!({"supportedVersions": self.versions,
                       "capabilities": {"tools": {"listChanged": true}},
                       "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fake", "version": "9"}},
                       "instructions": "Weather and friends."}),
            ),
            "tools/list" => {
                let start: usize = body["params"]["cursor"]
                    .as_str()
                    .map_or(0, |c| c.parse().unwrap());
                let all = tools();
                let chunk: Vec<Value> = all.iter().skip(start).take(3).cloned().collect();
                let mut res = json!({"tools": chunk});
                if start + 3 < all.len() {
                    res["nextCursor"] = json!((start + 3).to_string());
                }
                ok(&body, res)
            }
            "tools/call" => {
                let name = body["params"]["name"].as_str().unwrap().to_string();
                if get("mcp-name") != name {
                    return err(400, &body, -32020, "Mcp-Name mismatch");
                }
                self.tool(&name, &body, &get("mcp-param-region"))
            }
            m => err(404, &body, -32601, &format!("no method {m}")),
        }
    }
}

impl FakeMcp {
    fn tool(&self, name: &str, body: &Value, region_header: &str) -> ResponseTemplate {
        let args = &body["params"]["arguments"];
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        match name {
            "get_weather" => {
                let Some(city) = args.get("city").and_then(Value::as_str) else {
                    return err(400, body, -32602, "city required");
                };
                if let Some(r) = args.get("region").and_then(Value::as_str) {
                    if region_header != r {
                        return err(400, body, -32020, "Mcp-Param-Region mismatch");
                    }
                }
                ok(
                    body,
                    json!({"content": [{"type": "text", "text": format!("sunny in {city}")}], "isError": false}),
                )
            }
            "fail" => ok(
                body,
                json!({"content": [{"type": "text", "text": "no such city"}], "isError": true}),
            ),
            "picture" => ok(
                body,
                json!({"content": [
                    {"type": "image", "data": b64, "mimeType": "image/png"},
                    {"type": "resource", "resource": {"uri": "mem://x", "mimeType": "application/octet-stream", "blob": b64}}
                ]}),
            ),
            "structured" => ok(
                body,
                json!({"content": [{"type": "text", "text": "{\"t\": 21}"}], "structuredContent": {"t": 21}}),
            ),
            "ask" => ok(
                body,
                json!({"resultType": "input_required",
                       "inputRequests": {"who": {"method": "elicitation/create", "params": {"message": "name?"}}},
                       "requestState": "st-1"}),
            ),
            "weird" => ok(body, json!({"resultType": "telepathy"})),
            _ => {
                let mut events = Vec::new();
                if name == "stream" {
                    events.push(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "p", "progress": 1, "total": 2}}));
                    events.push(json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "half"}}));
                    events.push(json!({"jsonrpc": "2.0", "id": body["id"], "result": {"resultType": "complete", "content": [{"type": "text", "text": "done"}]}}));
                }
                if name == "cut" {
                    events.push(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "p", "progress": 1}}));
                }
                let sse: String = std::iter::once(": keep-alive\n\n".to_string())
                    .chain(events.iter().map(|e| format!("data: {e}\n\n")))
                    .collect();
                ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
            }
        }
    }
}

struct Rig {
    kernel: Arc<Kernel>,
    seen: Seen,
    _tmp: tempfile::TempDir,
    _server: MockServer,
}

async fn server(key: Option<&'static str>, versions: Vec<&'static str>) -> (MockServer, Seen) {
    let srv = MockServer::start().await;
    let seen: Seen = Arc::default();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(FakeMcp {
            key,
            versions,
            seen: Arc::clone(&seen),
        })
        .mount(&srv)
        .await;
    (srv, seen)
}

/// A kernel with the store, a key for `host`, and agent `mcp` on `url`.
/// `host` names the ref's pinned host; `None` = the URL's own authority.
async fn rig_on(
    url: &str,
    host: Option<&str>,
    key: &str,
    credentials: bool,
) -> (Arc<Kernel>, tempfile::TempDir) {
    let authority = reqwest::Url::parse(url).unwrap();
    let authority = format!(
        "{}:{}",
        authority.host_str().unwrap(),
        authority.port_or_known_default().unwrap()
    );
    let host = host.unwrap_or(&authority);
    let tmp = tempfile::TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(fantastic_credential_store::HANDLER_MODULE, creds::bundle());
    kernel
        .bundles
        .register(HANDLER_MODULE, McpClientBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        json!({"guid": "g1"}).as_object().cloned().unwrap(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    creds::set_key(&kernel, tmp.path(), "mcp", "main", host, key).await;
    let mut meta = Map::new();
    meta.insert("url".into(), json!(url));
    if credentials {
        creds::add_credentials(&mut meta, "main");
    }
    kernel.register(Agent::new(
        AgentId::from("mcp"),
        Some(HANDLER_MODULE.to_string()),
        Some(root.id.clone()),
        meta,
        std::path::PathBuf::new(),
        false,
    ));
    (kernel, tmp)
}

async fn rig() -> Rig {
    let (srv, seen) = server(Some(KEY), vec![PROTOCOL]).await;
    let url = format!("{}/mcp", srv.uri());
    let (kernel, tmp) = rig_on(&url, None, KEY, true).await;
    Rig {
        kernel,
        seen,
        _tmp: tmp,
        _server: srv,
    }
}

async fn send(k: &Arc<Kernel>, target: &str, payload: Value) -> fantastic_kernel::Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

async fn booted() -> Rig {
    let r = rig().await;
    let out = send(&r.kernel, "mcp", json!({"type": "boot"})).await;
    assert_eq!(out.status, Status::Completed, "{}", out.result);
    r
}

fn calls(seen: &Seen, m: &str) -> Vec<(HashMap<String, String>, Value)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, b)| b["method"] == m)
        .cloned()
        .collect()
}

// ─── listing ─────────────────────────────────────────────────────────

#[tokio::test]
async fn boot_lists_every_page_and_skips_what_it_cannot_serve() {
    let r = rig().await;
    let out = send(&r.kernel, "mcp", json!({"type": "boot"})).await.result;
    assert_eq!(out["server"], json!({"name": "fake", "version": "9"}));
    assert_eq!(
        out["tools"],
        json!([
            "get_weather",
            "fail",
            "picture",
            "stream",
            "structured",
            "ask",
            "weird",
            "cut"
        ])
    );
    let skipped: HashMap<String, String> = out["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["why"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(skipped.len(), 4, "{skipped:?}");
    assert!(skipped["fail"].contains("listed twice"));
    assert!(skipped["bad_header"].contains("properties"));
    let cursors: Vec<Value> = calls(&r.seen, "tools/list")
        .iter()
        .map(|(_, b)| b["params"]["cursor"].clone())
        .collect();
    assert_eq!(
        cursors,
        vec![Value::Null, json!("3"), json!("6"), json!("9")]
    );
    let rf = send(&r.kernel, "mcp", json!({"type": "reflect"}))
        .await
        .result;
    assert_eq!(rf["protocol_version"], PROTOCOL);
    assert_eq!(rf["tools"], 8);
    assert_eq!(rf["instructions"], "Weather and friends.");
    assert!(
        rf.get("verbs").is_none(),
        "reflect says what it is, never how to call it"
    );
}

#[tokio::test]
async fn docs_serve_this_agents_own_table_built_from_the_schemas() {
    let r = booted().await;
    let d = send(&r.kernel, "kernel", json!({"type": "docs", "of": "mcp"}))
        .await
        .result;
    assert_eq!(d["bundle"], HANDLER_MODULE);
    assert_eq!(
        d["verbs"]["get_weather"],
        "args: city:str (req), days:int?, region:str?"
    );
    assert_eq!(d["verbs"]["picture"], "No args");
    assert_eq!(d["verbs"].as_object().unwrap().len(), 10, "{d}");
    let full = send(
        &r.kernel,
        "kernel",
        json!({"type": "docs", "of": "mcp", "tier": "detailed", "verb": "get_weather"}),
    )
    .await
    .result;
    assert!(full["verbs"]["get_weather"]
        .as_str()
        .unwrap()
        .ends_with("Current weather for a city. Units default to metric."));
    let b = send(
        &r.kernel,
        "kernel",
        json!({"type": "docs", "of": "mcp_client"}),
    )
    .await
    .result;
    let verbs: Vec<&String> = b["verbs"].as_object().unwrap().keys().collect();
    assert_eq!(verbs, ["reflect", "boot"]);
}

// ─── the wire ────────────────────────────────────────────────────────

#[tokio::test]
async fn every_request_carries_meta_mirrored_headers_and_the_key() {
    let r = booted().await;
    let out = send(
        &r.kernel,
        "mcp",
        json!({"type": "get_weather", "city": "Kyiv", "region": "eu-1"}),
    )
    .await;
    assert_eq!(out.status, Status::Completed, "{}", out.result);
    assert_eq!(
        out.result,
        json!({"content": [{"type": "text", "text": "sunny in Kyiv"}]})
    );
    for (h, b) in r.seen.lock().unwrap().iter() {
        assert_eq!(
            h["authorization"],
            format!("Bearer {KEY}"),
            "the key on EVERY request"
        );
        assert_eq!(h["mcp-protocol-version"], PROTOCOL);
        assert_eq!(
            b["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
            "aisixteen-fantastic"
        );
    }
    let (h, b) = calls(&r.seen, "tools/call").pop().unwrap();
    assert_eq!(b["params"]["name"], "get_weather");
    assert_eq!(
        b["params"]["arguments"],
        json!({"city": "Kyiv", "region": "eu-1"})
    );
    assert_eq!(h["mcp-name"], "get_weather");
    assert_eq!(h["mcp-param-region"], "eu-1");
    send(
        &r.kernel,
        "mcp",
        json!({"type": "get_weather", "city": "Lviv"}),
    )
    .await;
    assert!(!calls(&r.seen, "tools/call")
        .pop()
        .unwrap()
        .0
        .contains_key("mcp-param-region"));
    let ledger = send(&r.kernel, "kernel", json!({"type": "ledger"}))
        .await
        .result
        .to_string();
    assert!(!ledger.contains(KEY), "the key never lands in the ledger");
}

#[tokio::test]
async fn a_record_without_credentials_sends_no_key() {
    let (srv, seen) = server(None, vec![PROTOCOL]).await;
    let (k, _tmp) = rig_on(&format!("{}/mcp", srv.uri()), None, KEY, false).await;
    assert_eq!(
        send(&k, "mcp", json!({"type": "boot"})).await.status,
        Status::Completed
    );
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|(h, _)| !h.contains_key("authorization")));
}

#[tokio::test]
async fn a_key_pinned_to_another_host_is_never_sent() {
    let (srv, seen) = server(Some(KEY), vec![PROTOCOL]).await;
    let (k, _tmp) = rig_on(
        &format!("{}/mcp", srv.uri()),
        Some("elsewhere.test"),
        KEY,
        true,
    )
    .await;
    let out = send(&k, "mcp", json!({"type": "boot"})).await;
    assert_eq!(out.result["reason"], "unauthorized", "{}", out.result);
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing left for the server"
    );
}

#[tokio::test]
async fn a_refused_key_is_unauthorized() {
    let (srv, _seen) = server(Some(KEY), vec![PROTOCOL]).await;
    let (k, _tmp) = rig_on(&format!("{}/mcp", srv.uri()), None, "wrong-key", true).await;
    let out = send(&k, "mcp", json!({"type": "boot"})).await.result;
    assert_eq!(out["reason"], "unauthorized");
    assert_eq!(out["http_status"], 401);
}

#[test]
fn header_values_ride_plain_or_as_the_base64_sentinel() {
    use crate::wire::{header_value, mirrors, param_headers};
    assert_eq!(header_value("us-west1"), "us-west1");
    assert_eq!(
        header_value("Hello, 世界"),
        "=?base64?SGVsbG8sIOS4lueVjA==?="
    );
    assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
    assert_eq!(
        header_value("=?base64?literal?="),
        "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
    );
    let found = mirrors(&tools()[0]["inputSchema"]).unwrap();
    assert_eq!(
        found,
        vec![("Region".to_string(), vec!["region".to_string()])]
    );
    assert_eq!(
        param_headers(&found, &json!({"region": "日本"})).unwrap(),
        vec![(
            "Mcp-Param-Region".to_string(),
            "=?base64?5pel5pys?=".to_string()
        )]
    );
    assert_eq!(
        param_headers(
            &[("On".to_string(), vec!["on".to_string()])],
            &json!({"on": true})
        )
        .unwrap(),
        vec![("Mcp-Param-On".to_string(), "true".to_string())]
    );
    assert!(
        mirrors(&json!({"properties": {"n": {"type": "number", "x-mcp-header": "N"}}})).is_err()
    );
    assert!(mirrors(&json!({"properties": {
        "a": {"type": "string", "x-mcp-header": "A"}, "b": {"type": "string", "x-mcp-header": "a"}}}))
    .is_err());
}

// ─── answers ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_tool_error_settles_errored_with_its_text() {
    let r = booted().await;
    let out = send(&r.kernel, "mcp", json!({"type": "fail"})).await;
    assert_eq!(out.status, Status::Errored, "{}", out.result);
    assert_eq!(out.result["error"], "no such city");
    assert_eq!(
        out.result["content"],
        json!([{"type": "text", "text": "no such city"}])
    );
}

#[tokio::test]
async fn binary_content_arrives_as_raw_bytes() {
    let r = booted().await;
    let out = send(&r.kernel, "mcp", json!({"type": "picture"})).await;
    let body = out
        .body
        .as_ref()
        .expect("the first binary item is the reply's body");
    // ONE binary body, at the reply's top-level `bytes`; the item names it
    assert_eq!(body.bytes, PNG);
    assert_eq!(body.path, vec![Step::Key("bytes".into())]);
    assert_eq!(
        out.result["content"][0],
        json!({"type": "image", "mimeType": "image/png", "size": PNG.len(), "body": "bytes"})
    );
    assert_eq!(
        out.result["content"][1]["resource"],
        json!({"uri": "mem://x", "mimeType": "application/octet-stream", "size": PNG.len(), "omitted": OMITTED})
    );
}

#[tokio::test]
async fn structured_content_is_kept() {
    let r = booted().await;
    let out = send(&r.kernel, "mcp", json!({"type": "structured"}))
        .await
        .result;
    assert_eq!(out["structured"], json!({"t": 21}));
}

#[tokio::test]
async fn an_sse_answer_settles_and_its_notes_reach_the_watchers() {
    let r = booted().await;
    let watcher = AgentId::from("_t_watch");
    let mut rx = r.kernel.ensure_inbox(&watcher);
    r.kernel.watch(&AgentId::from("mcp"), watcher).await;
    let out = send(&r.kernel, "mcp", json!({"type": "stream"}))
        .await
        .result;
    assert_eq!(out, json!({"content": [{"type": "text", "text": "done"}]}));
    let mut kinds = Vec::new();
    let mut prog = Value::Null;
    while let Some(item) = rx.try_recv() {
        let t = item.payload["type"].as_str().unwrap_or("").to_string();
        if t == "progress" {
            prog = item.payload.clone();
        }
        kinds.push(t);
    }
    assert!(
        kinds.contains(&"progress".to_string()) && kinds.contains(&"log".to_string()),
        "{kinds:?}"
    );
    assert_eq!(prog["tool"], "stream");
    assert_eq!(prog["total"], 2);
}

#[tokio::test]
async fn input_required_answers_what_the_server_asked() {
    let r = booted().await;
    let out = send(&r.kernel, "mcp", json!({"type": "ask"})).await;
    assert_eq!(out.status, Status::Errored);
    assert_eq!(out.result["input_required"]["requestState"], "st-1");
    assert!(out.result["input_required"]["inputRequests"]
        .get("who")
        .is_some());
}

#[tokio::test]
async fn failures_are_refusals_from_the_closed_set() {
    let r = booted().await;
    for (payload, reason) in [
        (json!({"type": "weird"}), "transport_error"),
        (json!({"type": "cut"}), "transport_closed"),
        (json!({"type": "get_weather"}), "bad_arg"),
        (json!({"type": "no_such_tool"}), "unhandled"),
    ] {
        let out = send(&r.kernel, "mcp", payload.clone()).await.result;
        assert_eq!(out["reason"], reason, "{payload}: {out}");
    }
    let r2 = send(&r.kernel, "mcp", json!({"type": "nope"})).await.result;
    let hint = r2["hint"].as_str().unwrap();
    assert!(
        hint.contains("get_weather") && hint.contains("docs of=mcp"),
        "{hint}"
    );
}

// ─── versions, connection, lifecycle ─────────────────────────────────

#[tokio::test]
async fn a_server_on_an_older_protocol_is_refused_naming_its_versions() {
    let (srv, _seen) = server(Some(KEY), vec!["2025-11-25"]).await;
    let (k, _tmp) = rig_on(&format!("{}/mcp", srv.uri()), None, KEY, true).await;
    let out = send(&k, "mcp", json!({"type": "boot"})).await.result;
    assert_eq!(out["reason"], "transport_error");
    let e = out["error"].as_str().unwrap();
    assert!(
        e.contains("2025-11-25") && e.contains("2026-07-28 only"),
        "{e}"
    );
}

#[tokio::test]
async fn an_unsupported_version_error_names_the_servers_versions() {
    let srv = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(400).set_body_json(json!({"jsonrpc": "2.0", "id": body["id"],
                "error": {"code": -32022, "message": "Unsupported protocol version",
                          "data": {"supported": ["2025-06-18"], "requested": "2026-07-28"}}}))
        })
        .mount(&srv)
        .await;
    let (k, _tmp) = rig_on(&format!("{}/mcp", srv.uri()), None, KEY, true).await;
    let out = send(&k, "mcp", json!({"type": "boot"})).await.result;
    assert_eq!(out["reason"], "transport_error");
    assert!(
        out["error"].as_str().unwrap().contains("2025-06-18"),
        "{out}"
    );
}

#[tokio::test]
async fn an_unreachable_server_is_not_connected_and_calls_say_so() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (k, _tmp) = rig_on(&format!("http://127.0.0.1:{port}/mcp"), None, KEY, true).await;
    let boot = send(&k, "mcp", json!({"type": "boot"})).await.result;
    assert_eq!(boot["reason"], "not_connected", "{boot}");
    let call = send(&k, "mcp", json!({"type": "get_weather", "city": "x"}))
        .await
        .result;
    assert_eq!(call["reason"], "not_connected");
    assert!(
        call["error"].as_str().unwrap().contains("cannot connect"),
        "{call}"
    );
    assert!(send(&k, "mcp", json!({"type": "reflect"}))
        .await
        .result
        .get("last_error")
        .is_some());
}

#[tokio::test]
async fn a_failed_relist_keeps_the_tools_already_listed() {
    let r = booted().await;
    r._server.reset().await; // the server is gone: every request 404s
    let out = send(&r.kernel, "mcp", json!({"type": "boot"})).await.result;
    assert_eq!(out["reason"], "transport_error", "{out}");
    let d = send(&r.kernel, "kernel", json!({"type": "docs", "of": "mcp"}))
        .await
        .result;
    assert!(
        d["verbs"].get("get_weather").is_some(),
        "the earlier listing stands"
    );
}

#[tokio::test]
async fn a_grant_names_tools_like_verbs() {
    let r = booted().await;
    let k = &r.kernel;
    let mut meta = Map::new();
    meta.insert("can_call".into(), json!({"mcp": ["get_weather"]}));
    k.register(Agent::new(
        AgentId::from("caller"),
        None,
        Some(AgentId::from("kernel_state")),
        meta,
        std::path::PathBuf::new(),
        false,
    ));
    let mut patch = Map::new();
    patch.insert(
        "callable_by".into(),
        json!({"caller": ["get_weather", "fail"]}),
    );
    k.update(&AgentId::from("mcp"), patch);
    let (ok, no) = fantastic_kernel::test_support::as_agent("caller", async {
        (
            send(k, "mcp", json!({"type": "get_weather", "city": "Kyiv"})).await,
            send(k, "mcp", json!({"type": "fail"})).await,
        )
    })
    .await;
    assert_eq!(ok.status, Status::Completed, "{}", ok.result);
    assert_eq!(no.result["reason"], "unauthorized");
    assert_eq!(no.result["side"], "egress");
}

#[test]
fn the_record_is_checked_at_the_write() {
    let b = McpClientBundle::default();
    let m = |v: Value| v.as_object().cloned().unwrap();
    for (meta, fragment) in [
        (json!({}), "required on the record and missing: url"),
        (json!({"url": "mcp.test/mcp"}), "absolute http(s) URL"),
        (json!({"url": "https://mcp.test/mcp#frag"}), "no fragment"),
        (
            json!({"url": "https://mcp.test/mcp", "credentials": {"store": "creds"}}),
            "credentials must be",
        ),
    ] {
        let e = b
            .validate_meta(&m(meta.clone()), &Map::new())
            .unwrap_or_default();
        assert!(e.contains(fragment), "{meta}: {e}");
    }
}

#[tokio::test]
async fn the_catalog_publishes_the_record_schema() {
    let r = rig().await;
    let cat = send(
        &r.kernel,
        "kernel",
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await
    .result;
    let row = cat["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == NAME)
        .unwrap()
        .clone();
    let names: Vec<&str> = row["schema"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["url", "credentials"]);
}

#[tokio::test]
async fn a_call_past_its_timeout_settles_timed_out() {
    let r = booted().await;
    r._server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&r._server)
        .await;
    let mut item = Item::new(
        AgentId::from("mcp"),
        json!({"type": "get_weather", "city": "x"}),
    );
    item.timeout = Some(0.5);
    let out = r.kernel.send(vec![item]).await.remove(0);
    assert_eq!(out.status, Status::TimedOut, "{}", out.result);
}
