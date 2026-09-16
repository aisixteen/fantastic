//! The request handlers behind the ordered route table.
//!
//! One `dispatch` walks the table exactly as starlette's `Router.app` walks
//! `app.routes` (first FULL match, then the first PARTIAL as `405`, then the
//! trailing-slash toggle as `307`, then FastAPI's `404`), and each handler
//! mirrors its python twin in `PB/web/host/src/web/app.py` and
//! `PB/io/web_rest/src/web_rest/tools.py`.

use crate::routes::{self, Handler, Resolved, RouteTable};
use crate::{mimetypes, ws, AppState, FAVICON_PNG, ROOT_INDEX_HTML};
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use fantastic_bundle::{py_json_dumps, py_truthy};
use fantastic_kernel::{AgentId, Item};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock as AsyncRwLock;

/// Header carrying a caller-chosen execution id on a REST POST. Mirrors
/// `X-Fantastic-Auth`: dispatch metadata rides the carrier, never the body.
const EXEC_ID_HEADER: &str = "x-fantastic-execution-id";
/// The binary leg — JSON has no bytes type, so a verb whose payload OR reply
/// carries RAW bytes rides an octet-stream body: the raw body IS the blob, the
/// rest of the envelope is JSON on `X-Fantastic-Payload`, and the one
/// bytes-valued field is named by `X-Fantastic-Bytes-Field` (default `bytes`).
const OCTET_CT: &str = "application/octet-stream";
/// The INGRESS credential on the http modality — the http envelope is the
/// request HEADER, so the dispatched payload never carries the token.
const AUTH_HEADER: &str = "x-fantastic-auth";
const PAYLOAD_HEADER: &str = "x-fantastic-payload";
/// Header naming the one bytes-valued field on an octet request/reply.
const BYTES_FIELD_HEADER: &str = "x-fantastic-bytes-field";

/// One chunk of the file proxy, python's `length: 262144`.
const CHUNK: u64 = 262_144;

/// starlette's `Router.app` over the live table.
pub(crate) async fn dispatch(
    state: AppState,
    table: Arc<AsyncRwLock<RouteTable>>,
    req: Request<Body>,
) -> Response {
    let websocket = routes::is_websocket(&req);
    let (path, query) = routes::split_target(&req);
    let method = req.method().clone();
    let resolved = {
        let t = table.read().await;
        match routes::resolve(&t, &path, &method, websocket) {
            Resolved::Full(route, caps) => Some(Ok((route.handler.clone(), caps))),
            Resolved::MethodNotAllowed(m) => Some(Err(m.clone())),
            Resolved::None => None,
        }
    };
    match resolved {
        Some(Ok((handler, caps))) => run(state, handler, caps, &query, req).await,
        Some(Err(allow)) => routes::method_not_allowed(&allow),
        None => {
            // starlette's `redirect_slashes`: http only, never for `/`.
            if !websocket && path != "/" {
                let toggled = if path.ends_with('/') {
                    path.trim_end_matches('/').to_string()
                } else {
                    format!("{path}/")
                };
                let hit = {
                    let t = table.read().await;
                    routes::any_match(&t, &toggled, websocket)
                };
                if hit {
                    return routes::redirect_slash(req.headers(), &toggled, Some(&query));
                }
            }
            routes::not_found()
        }
    }
}

async fn run(
    state: AppState,
    handler: Handler,
    caps: Vec<String>,
    query: &str,
    req: Request<Body>,
) -> Response {
    match handler {
        Handler::Index => state.scoped(index_page(&state)).await,
        Handler::Favicon => {
            routes::respond(StatusCode::OK, &[], Some("image/png"), FAVICON_PNG.to_vec())
        }
        Handler::FileProxy => {
            let agent_id = caps.first().cloned().unwrap_or_default();
            let path = caps.get(1).cloned().unwrap_or_default();
            let st = state.clone();
            state.scoped(file_proxy(st, agent_id, path)).await
        }
        Handler::RestPost(owner) => {
            let target = caps.first().cloned().unwrap_or_default();
            let (parts, body) = req.into_parts();
            let body = match axum::body::to_bytes(body, usize::MAX).await {
                Ok(b) => b.to_vec(),
                Err(_) => Vec::new(),
            };
            let st = state.clone();
            state
                .scoped(rest_post(st, owner, target, parts.headers, body))
                .await
        }
        Handler::RestReflectRoot(owner) => {
            let st = state.clone();
            let q = query.to_string();
            state
                .scoped(rest_reflect(
                    st,
                    owner,
                    "kernel".to_string(),
                    q,
                    req.headers().clone(),
                ))
                .await
        }
        Handler::RestReflectTarget(owner) => {
            let target = caps.first().cloned().unwrap_or_default();
            let st = state.clone();
            let q = query.to_string();
            state
                .scoped(rest_reflect(st, owner, target, q, req.headers().clone()))
                .await
        }
        Handler::Ws => ws_upgrade(state, caps, req).await,
    }
}

// ─── `GET /` ────────────────────────────────────────────────────────

/// python `_index_page` — render the substrate tree into the scaffold. NO
/// per-agent `render_html` probe and no `↗` link: python deleted the
/// server-side render route, frontend panels live in the TS kernel.
async fn index_page(state: &AppState) -> Response {
    let primer = state
        .kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    let tree = primer.get("tree").cloned().unwrap_or(Value::Null);
    let body = if py_truthy(&tree) {
        render_node(&tree)
    } else {
        "<li><em>empty tree</em></li>".to_string()
    };
    routes::respond(
        StatusCode::OK,
        &[],
        Some("text/html; charset=utf-8"),
        ROOT_INDEX_HTML.replace("{{tree_body}}", &body).into_bytes(),
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_node(node: &Value) -> String {
    let Some(obj) = node.as_object() else {
        return String::new();
    };
    let aid = obj.get("id").and_then(Value::as_str).unwrap_or("");
    let name = obj
        .get("display_name")
        .filter(|v| py_truthy(v))
        .and_then(Value::as_str)
        .unwrap_or(aid);
    let hm = obj
        .get("handler_module")
        .filter(|v| py_truthy(v))
        .and_then(Value::as_str)
        .unwrap_or("(root)");
    let children = obj.get("children").and_then(Value::as_array);
    let kids = match children {
        Some(c) if !c.is_empty() => {
            let inner: String = c.iter().map(render_node).collect();
            format!("<ul>{inner}</ul>")
        }
        _ => String::new(),
    };
    format!(
        r#"<li><span class="id">{}</span> <code>{}</code> <span class="hm">{}</span>{}</li>"#,
        esc(name),
        esc(aid),
        esc(hm),
        kids
    )
}

// ─── `GET /{agent_id}/file/{path:path}` ─────────────────────────────

/// python `agent_file` — any agent answering `read_stream{path,offset,length}`
/// becomes an HTTP file server. The read is dispatched AS THIS WEB AGENT, so
/// the served agent's `callable_by` AND its io leg decide; anything that does
/// not answer with a chunk is a 404, and only an `unauthorized` denial carries
/// a body (a missing file must not leak its absence).
async fn file_proxy(state: AppState, agent_id: String, path: String) -> Response {
    let target = AgentId::from(agent_id.as_str());
    if !state.kernel.agents.contains_key(&target) {
        return routes::respond(StatusCode::NOT_FOUND, &[], None, Vec::new());
    }
    let sender = state.web_agent_id.clone();
    let kernel = Arc::clone(&state.kernel);
    let (meta, first) = fantastic_kernel::send::with_sender(sender.clone(), async {
        kernel
            .send_with_binary(
                &target,
                json!({"type": "read_stream", "path": path, "offset": 0, "length": CHUNK}),
                Vec::new(),
            )
            .await
    })
    .await;
    if meta.get("error").is_some() {
        // A GATE denial carries its reason in the body: this kernel teaches
        // through denials, and the route used to throw that away. A MISSING
        // FILE stays a bare 404 — no body, no existence leak.
        if meta.get("reason").and_then(Value::as_str) == Some("unauthorized") {
            let mut detail = serde_json::Map::new();
            for k in ["error", "reason", "side", "hint"] {
                detail.insert(k.into(), meta.get(k).cloned().unwrap_or(Value::Null));
            }
            return routes::respond(
                StatusCode::NOT_FOUND,
                &[],
                Some("application/json"),
                py_json_dumps(&Value::Object(detail), true, None).into_bytes(),
            );
        }
        return routes::respond(StatusCode::NOT_FOUND, &[], None, Vec::new());
    }
    let ct = mimetypes::content_type(&path);
    let eof = meta.get("eof").and_then(Value::as_bool).unwrap_or(true);
    // python: `headers={"content-length": str(size)} if size else None` — a
    // zero size sends NO content-length and the body is chunked.
    let size = meta.get("size").and_then(Value::as_u64).filter(|n| *n != 0);
    let next = meta
        .get("next_offset")
        .and_then(Value::as_u64)
        .unwrap_or(first.len() as u64);
    let kernel = Arc::clone(&state.kernel);
    let stream = futures_util::stream::unfold(
        (Some(first), kernel, target, path, next, eof, sender),
        move |(pending, kernel, target, path, offset, done, sender)| async move {
            if let Some(buf) = pending {
                return Some((
                    Ok::<Vec<u8>, std::io::Error>(buf),
                    (None, kernel, target, path, offset, done, sender),
                ));
            }
            if done {
                return None;
            }
            // The generator is iterated AFTER the handler returned, so it
            // re-enters the sender context per chunk (python does the same).
            let (m, body) = fantastic_kernel::send::with_sender(sender.clone(), {
                let kernel = Arc::clone(&kernel);
                let target = target.clone();
                let path = path.clone();
                async move {
                    kernel
                        .send_with_binary(
                            &target,
                            json!({"type": "read_stream", "path": path, "offset": offset, "length": CHUNK}),
                            Vec::new(),
                        )
                        .await
                }
            })
            .await;
            if m.get("error").is_some() {
                return None;
            }
            let eof = m.get("eof").and_then(Value::as_bool).unwrap_or(true);
            let next = m
                .get("next_offset")
                .and_then(Value::as_u64)
                .unwrap_or(offset + body.len() as u64);
            Some((Ok(body), (None, kernel, target, path, next, eof, sender)))
        },
    );
    routes::respond_stream(&ct, size, Body::from_stream(stream))
}

// ─── `POST /<rest_id>/{target_id}` ──────────────────────────────────

/// python `_rest_call` — the REST verb channel, JSON leg and octet leg.
async fn rest_post(
    state: AppState,
    owner: AgentId,
    target_id: String,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Response {
    let target = AgentId::from(target_id.as_str());
    let header_str =
        |name: &str| -> Option<String> { headers.get(name).map(|v| routes::latin1(v.as_bytes())) };
    let exec_id = header_str(EXEC_ID_HEADER).filter(|v| !v.is_empty());
    let content_type = header_str(header::CONTENT_TYPE.as_str()).unwrap_or_default();
    let auth_token = header_str(AUTH_HEADER);

    if content_type.starts_with(OCTET_CT) {
        // python `json.loads(headers.get(...) or "{}")` — an EMPTY header value
        // is an empty envelope, not a parse error.
        let raw = header_str(PAYLOAD_HEADER).filter(|v| !v.is_empty());
        let payload: Value = match raw {
            Some(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(_) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "web_rest: X-Fantastic-Payload is not valid JSON",
                    )
                }
            },
            None => json!({}),
        };
        let Some(mut obj) = payload.as_object().cloned() else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "web_rest: X-Fantastic-Payload must be a JSON object",
            );
        };
        // python binds the body under the NAMED field (`payload[field] = body`).
        // The kernel's binary channel carries ONE blob, which the target reads
        // as `bytes`, so the leg can only honour the default name: the body
        // rides the blob when the field IS `bytes` (overwriting any header
        // `bytes` key, as python's assignment does), and any other name sends
        // an EMPTY blob with the header untouched — so the target sees the
        // header's own `bytes` (any value ⇒ missing_arg) or none (empty chunk).
        let field = header_str(BYTES_FIELD_HEADER)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "bytes".to_string());
        let blob = if field == "bytes" {
            obj.remove("bytes");
            body
        } else {
            Vec::new()
        };
        let payload = Value::Object(obj);
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(denial) = rest_gate(&state, &owner, &target_id, verb, auth_token.as_deref()) {
            return denial;
        }
        let kernel = Arc::clone(&state.kernel);
        let (reply, out_body) = fantastic_kernel::send::with_sender(owner.clone(), async move {
            kernel.send_with_binary(&target, payload, blob).await
        })
        .await;
        if !out_body.is_empty() {
            // A reply that carries raw bytes goes back the way it came in —
            // octet body + JSON header — never base64. The header is python's
            // `json.dumps(rest)`: `, `/`: ` separators and ensure_ascii.
            return routes::respond(
                StatusCode::OK,
                &[
                    (
                        header::HeaderName::from_static(PAYLOAD_HEADER),
                        py_json_dumps(&reply, true, None),
                    ),
                    (
                        header::HeaderName::from_static(BYTES_FIELD_HEADER),
                        "bytes".to_string(),
                    ),
                ],
                Some(OCTET_CT),
                out_body,
            );
        }
        return json_reply(reply);
    }

    // ── JSON leg ────────────────────────────────────────────────────
    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            // Don't echo the parser exception back — generic message only
            // (CodeQL: information exposure through an exception).
            Err(_) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "web_rest: request body is not valid JSON",
                )
            }
        }
    };
    if !payload.is_object() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "web_rest: body must be a JSON object",
        );
    }
    let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if let Some(denial) = rest_gate(&state, &owner, &target_id, verb, auth_token.as_deref()) {
        return denial;
    }
    let kernel = Arc::clone(&state.kernel);
    let reply = fantastic_kernel::send::with_sender(owner.clone(), async move {
        let mut item = Item::new(target.clone(), payload);
        if let Some(id) = exec_id {
            item = item.with_id(id);
        }
        kernel.send(vec![item]).await.remove(0).result
    })
    .await;
    json_reply(reply)
}

// ─── `GET /<rest_id>/_reflect[/{target_id}]` ────────────────────────

/// python `_reflect_target` / `_reflect_kernel` — the browser-pastable reflect
/// shortcuts, composed from the query flags; a null reply is a bare 404.
async fn rest_reflect(
    state: AppState,
    owner: AgentId,
    target_id: String,
    query: String,
    headers: HeaderMap,
) -> Response {
    let payload = routes::reflect_payload(&query);
    let token = headers
        .get(AUTH_HEADER)
        .map(|v| routes::latin1(v.as_bytes()));
    if let Some(denial) = rest_gate(&state, &owner, &target_id, "reflect", token.as_deref()) {
        return denial;
    }
    let target = AgentId::from(target_id.as_str());
    let kernel = Arc::clone(&state.kernel);
    let reply = fantastic_kernel::send::with_sender(owner.clone(), async move {
        kernel
            .send(vec![Item::new(target, payload)])
            .await
            .remove(0)
            .result
    })
    .await;
    if reply.is_null() {
        return routes::respond(StatusCode::NOT_FOUND, &[], None, Vec::new());
    }
    routes::respond(
        StatusCode::OK,
        &[],
        Some("application/json"),
        reply.to_string().into_bytes(),
    )
}

// ─── shared REST pieces ─────────────────────────────────────────────

/// A dispatched reply is 200 even when it carries `{error}` (python parity);
/// only the INGRESS gate and a malformed request are 4xx, and a `None` reply
/// is `204`.
fn json_reply(reply: Value) -> Response {
    if reply.is_null() {
        return routes::respond(StatusCode::NO_CONTENT, &[], None, Vec::new());
    }
    routes::respond(
        StatusCode::OK,
        &[],
        Some("application/json"),
        reply.to_string().into_bytes(),
    )
}

fn json_error(status: StatusCode, message: &str) -> Response {
    routes::respond(
        status,
        &[],
        Some("application/json"),
        json!({"error": message}).to_string().into_bytes(),
    )
}

/// The INGRESS gate on a REST leg — python parity (`web_rest/tools.py::_gate`).
/// A leg carrying NO `ingress_rule` is SEALED, like every other io leg, and the
/// denial names the true cause instead of an egress refusal.
fn rest_gate(
    state: &AppState,
    owner: &AgentId,
    target: &str,
    verb: &str,
    token: Option<&str>,
) -> Option<Response> {
    use fantastic_io_bridge::authorizer::{ingress, manifold_spec, Action, Decision, Denial};

    let (primary, auth) = match state.kernel.agents.get(owner).map(|e| Arc::clone(&e)) {
        Some(a) => {
            let meta = a.meta.read().expect("meta poisoned");
            (meta.get("ingress_rule").cloned(), meta.get("auth").cloned())
        }
        // No record for the mounting leg ⇒ nothing granted it anything. Sealed.
        None => (None, None),
    };
    let spec = manifold_spec(primary.as_ref(), auth.as_ref(), "_ingress");
    let decision = match ingress::resolve(spec.as_ref()) {
        Ok(rule) => rule.authorize(&Action {
            kind: "call",
            target,
            verb,
            token,
        }),
        // A spec that will not parse is not an authorization.
        Err(e) => Decision::Deny(Denial::new(format!("web_rest: bad ingress rule: {e}"))),
    };
    match decision {
        Decision::Allow => None,
        Decision::Deny(denial) => {
            let mut body = json!({"error": denial.reason, "reason": "unauthorized"});
            if !denial.hint.is_empty() {
                body["hint"] = json!(denial.hint);
            }
            if !denial.see.is_empty() {
                body["see"] = json!(denial.see);
            }
            Some(routes::respond(
                StatusCode::FORBIDDEN,
                &[],
                Some("application/json"),
                body.to_string().into_bytes(),
            ))
        }
    }
}

// ─── the WS upgrade ─────────────────────────────────────────────────

/// Hand the socket to [`ws::ws_loop`], then rewrite the `101` header block into
/// uvicorn's order (`Upgrade, Connection, Sec-WebSocket-Accept, …, date,
/// server`). RESIDUAL: python also negotiates `Sec-WebSocket-Extensions:
/// permessage-deflate` and sends capitalised header names; neither survives
/// hyper's writer, which lowercases every name and has no deflate codec.
async fn ws_upgrade(state: AppState, caps: Vec<String>, req: Request<Body>) -> Response {
    let host = AgentId::from(caps.first().map(String::as_str).unwrap_or(""));
    let (mut parts, _body) = req.into_parts();
    let upgrade =
        match axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(u) => u,
            Err(rej) => return rej.into_response(),
        };
    let mut resp = upgrade.on_upgrade(move |socket| ws::ws_loop(state, socket, host));
    let accept = resp.headers().get(header::SEC_WEBSOCKET_ACCEPT).cloned();
    let h = resp.headers_mut();
    h.clear();
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Some(a) = accept {
        h.insert(header::SEC_WEBSOCKET_ACCEPT, a);
    }
    if let Ok(v) = HeaderValue::from_str(&routes::http_date()) {
        h.insert(header::DATE, v);
    }
    h.insert(header::SERVER, HeaderValue::from_static("uvicorn"));
    resp
}
