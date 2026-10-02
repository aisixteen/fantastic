//! The request handlers behind the ordered route table.
//!
//! One `dispatch` walks the table exactly as starlette's `Router.app` walks
//! `app.routes` (first FULL match, then the first PARTIAL as `405`, then the
//! trailing-slash toggle as `307`, then FastAPI's `404`), and each handler
//! mirrors its python twin in `PB/web/host/src/web/app.py` and
//! `PB/io/web_rest/src/web_rest/tools.py`.

use crate::routes::{self, Handler, Resolved, RouteTable};
use crate::{mimetypes, ws, AppState, FAVICON_PNG};
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use fantastic_bundle::py_json_dumps;
use fantastic_kernel::{AgentId, Blob, Item};
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
/// SECRET MARKS: the item's `secret` paths (`fantastic_kernel::secret`) ride
/// `X-Fantastic-Secret` — dispatch metadata about the call, like the execution
/// id, never part of the payload. The value is a JSON ARRAY of paths, the SAME
/// value a ws/bridge `call` frame carries in its `secret` field: one parser (the
/// strict wire `loads`), and the kernel's own check judges it. Not
/// comma-separated — a path step is a payload key, and a key may hold a comma; a
/// JSON string escapes any character into the ASCII a header value is safe in;
/// and a wrong shape (a bare string, `[]`, a number) reaches the kernel's refusal
/// verbatim instead of being re-invented by a splitter. Python `_SECRET_HEADER`.
const SECRET_HEADER: &str = "x-fantastic-secret";

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
        Handler::Ws(leg) => ws_upgrade(state, leg, caps, req).await,
    }
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
    let (meta, first) = read_chunk(&kernel, &sender, &target, &path, json!(0)).await;
    // PYTHON'S TEST IS THE CHUNK, not the absence of an error:
    // `isinstance(first.get("bytes"), (bytes, bytearray))`. Anything that did
    // not answer with a chunk — a denial, a missing file, an agent that is not
    // a SOURCE at all — is a 404 (C37 gives rust the same question to ask).
    let Some(first) = first else {
        // A GATE denial carries its reason in the body: this kernel teaches
        // through denials, and the route used to throw that away. A MISSING
        // FILE stays a bare 404 — no body, no existence leak.
        if meta.get("reason").and_then(Value::as_str) == Some("unauthorized") {
            // EXACTLY four keys, in THIS order, a missing one as `null` (a
            // sealed io-gate denial carries no `side`); `see` and anything else
            // is dropped. Note the order differs from the REST refusal's.
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
    };
    let ct = mimetypes::content_type(&path);
    // python: `headers={"content-length": str(size)} if size else None` — a
    // zero size sends NO content-length and the body is chunked.
    let size = meta.get("size").and_then(Value::as_u64).filter(|n| *n != 0);
    let kernel = Arc::clone(&state.kernel);
    // python's generator: yield the chunk; stop on a TRUTHY `eof`; else read on
    // at `chunk["next_offset"]` — a chunk without one raises KeyError, which
    // aborts the body mid-stream.
    let stream = futures_util::stream::unfold(
        (Some(Step::Yield(meta, first)), kernel, target, path, sender),
        move |(step, kernel, target, path, sender)| async move {
            match step? {
                Step::Yield(m, buf) => Some((
                    Ok::<Vec<u8>, std::io::Error>(buf),
                    (Some(Step::Next(m)), kernel, target, path, sender),
                )),
                Step::Next(m) => {
                    if fantastic_bundle::py_truthy(m.get("eof").unwrap_or(&Value::Null)) {
                        return None;
                    }
                    let Some(n) = m.get("next_offset").cloned() else {
                        return Some((
                            Err(std::io::Error::other("KeyError: 'next_offset'")),
                            (None, kernel, target, path, sender),
                        ));
                    };
                    // The generator is iterated AFTER the handler returned, so it
                    // re-enters the sender context per chunk (python does the
                    // same). python breaks on anything that is not a chunk.
                    let (m2, body) = read_chunk(&kernel, &sender, &target, &path, n).await;
                    let body = body?;
                    Some((
                        Ok(body),
                        (Some(Step::Next(m2)), kernel, target, path, sender),
                    ))
                }
            }
        },
    );
    routes::respond_stream(&ct, size, Body::from_stream(stream))
}

/// Where the file proxy's generator stands: a chunk to hand over, or the reply
/// it came in (whose `eof`/`next_offset` decide what follows).
enum Step {
    Yield(Value, Vec<u8>),
    Next(Value),
}

/// ONE file-proxy chunk, THROUGH THE BATCH and as the web agent (C37/C40):
/// python dispatches `kernel.send([{target, payload}])` under
/// `sender_context(web_agent_id)`, so the served agent's `callable_by` AND its
/// io leg both decide — an ordinary execution of the batch, taking its chunk back
/// raw.
async fn read_chunk(
    kernel: &Arc<fantastic_kernel::Kernel>,
    sender: &AgentId,
    target: &AgentId,
    path: &str,
    offset: Value,
) -> (Value, Option<Vec<u8>>) {
    let kernel2 = Arc::clone(kernel);
    let item = Item::new(
        target.clone(),
        json!({"type": "read_stream", "path": path, "offset": offset, "length": CHUNK}),
    )
    .with_raw_reply();
    let out = fantastic_kernel::send::with_sender(sender.clone(), async move {
        kernel2.send(vec![item]).await.remove(0)
    })
    .await;
    // python takes the chunk at the reply's `bytes` key; one anywhere else is
    // not a chunk
    (out.result, out.body.and_then(|b| b.into_at("bytes")))
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
    // SECRET MARKS: this kernel records the call as shown, exactly as for a local
    // caller; a malformed mark is the kernel's own refusal. Read AFTER the gate
    // (a sealed leg still answers 403 first) — python reads it where it builds
    // the item. `Err` = the header is not JSON (STRICT, D13: python
    // `strict_loads`).
    let secret_marks = || -> Result<Option<Value>, String> {
        header_str(SECRET_HEADER)
            .map(|raw| fantastic_io_bridge::codec::loads(&raw))
            .transpose()
    };
    let not_json = || {
        json_error(
            StatusCode::BAD_REQUEST,
            "web_rest: X-Fantastic-Secret is not valid JSON",
        )
    };

    if content_type.starts_with(OCTET_CT) {
        // python `json.loads(headers.get(...) or "{}")` — an EMPTY header value
        // is an empty envelope, not a parse error.
        let raw = header_str(PAYLOAD_HEADER).filter(|v| !v.is_empty());
        let payload: Value = match raw {
            // STRICT (D13): python `strict_loads` — what not every runtime can
            // hold is "not valid JSON".
            Some(s) => match fantastic_io_bridge::codec::loads(&s) {
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
        // python binds the body under the NAMED field (`payload[header or
        // "bytes"] = body`) — ONE LITERAL TOP-LEVEL KEY, never a path: `a.b` is
        // the key `"a.b"`, and a nested `a.b` the envelope already holds is left
        // as sent (a WS frame's `_binary_path` names the same key `["payload",
        // "a.b"]`). Each
        // handler reads its OWN key (`paste_image` reads `data`, `write_stream`
        // reads `bytes`). The kernel's binary channel carries the body under that
        // key (`Item::with_binary_key`), so the target finds it exactly where
        // python's dict holds it. The slot keeps its place (or is appended) —
        // overwriting any header value there, as python's assignment does — so
        // the call row lists the field where python's dict holds it. An absent
        // or empty header is `bytes`.
        let field = header_str(BYTES_FIELD_HEADER)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "bytes".to_string());
        obj.insert(field.clone(), Value::Null);
        let payload = Value::Object(obj);
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(denial) = rest_gate(&state, &owner, &target_id, verb, auth_token.as_deref()) {
            return denial;
        }
        let Ok(secret) = secret_marks() else {
            return not_json();
        };
        // THROUGH THE BATCH, with the header's execution id NAMING the execution
        // (A13.3): python builds `{target, payload+bytes}`, sets `item["id"]`
        // from `X-Fantastic-Execution-Id` and calls `kernel.send([item])`, so a
        // repeated id on the same target REPLAYS instead of re-running.
        let kernel = Arc::clone(&state.kernel);
        let out = fantastic_kernel::send::with_sender(owner.clone(), async move {
            let mut item = Item::new(target, payload).with_binary_key(field, body);
            if let Some(id) = exec_id {
                item = item.with_id(id);
            }
            if let Some(marks) = secret {
                item = item.with_secret(marks);
            }
            kernel.send(vec![item]).await.remove(0)
        })
        .await;
        return octet_or_json(out.result, out.body);
    }

    // ── JSON leg ────────────────────────────────────────────────────
    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        // CPython `json.loads(bytes)` DETECTS THE ENCODING first (A13.11), so a
        // UTF-8 BOM, a UTF-16 or a UTF-32 body all parse; `serde_json` reads
        // UTF-8 only and answered 400 for each. A body that will not decode at
        // all is a MALFORMED BODY — python catches its `UnicodeDecodeError`
        // beside `JSONDecodeError` and answers the same 400 (it used to raise an
        // uncaught 500, which this branch mirrored until parity 2026-09-30).
        let Some(text) = decode_json_bytes(&body) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "web_rest: request body is not valid JSON",
            );
        };
        // STRICT (D13): python `strict_loads(body)`.
        match fantastic_io_bridge::codec::loads(&text) {
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
    let Ok(secret) = secret_marks() else {
        return not_json();
    };
    let kernel = Arc::clone(&state.kernel);
    let out = fantastic_kernel::send::with_sender(owner.clone(), async move {
        let mut item = Item::new(target.clone(), payload);
        if let Some(id) = exec_id {
            item = item.with_id(id);
        }
        if let Some(marks) = secret {
            item = item.with_secret(marks);
        }
        // A BINARY-EDGE VERB RIDES THE BINARY CHANNEL even on the JSON leg
        // (A13.1): python's `read_stream` reply holds real bytes whatever leg
        // asked for it, and a `write_stream` whose `bytes` is a JSON string is
        // `missing_arg` with nothing written. Routing it here gives both.
        let binary_edge = item
            .payload
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|v| fantastic_io_bridge::codec::BINARY_EDGE_VERBS.contains(&v));
        if binary_edge {
            item = item.with_raw_reply();
        }
        kernel.send(vec![item]).await.remove(0)
    })
    .await;
    octet_or_json(out.result, out.body)
}

/// The ONE reply-shape decision, shared by both REST legs — python's
/// `_first_bytes_field`: a reply whose chunk sits at a TOP-LEVEL key is octet (a
/// 0-byte chunk included — an empty file is still a file), the header naming
/// THAT key (`bytes` for a `read_stream`, `x.y` for a peer's reply that put it
/// there); a reply with no chunk is JSON. Rust used to test
/// `!out_body.is_empty()`, which served an empty file as JSON, and always named
/// `bytes`.
fn octet_or_json(reply: Value, body: Option<Blob>) -> Response {
    let Some(body) = body else {
        return json_reply(reply);
    };
    let field = match (body.path.as_slice(), reply.is_object()) {
        ([fantastic_kernel::Step::Key(k)], true) => k.clone(),
        // python: no top-level bytes field ⇒ `JSONResponse(reply)`, which cannot
        // render bytes nested inside the reply.
        _ => return no_json_form(),
    };
    // The bytes value never travels twice: `X-Fantastic-Payload` is the reply
    // MINUS the field, and `X-Fantastic-Bytes-Field` names it.
    let mut head = reply;
    if let Some(m) = head.as_object_mut() {
        // SHIFT, not swap: `Map::remove` moves the LAST key into the hole, so
        // dropping the field reordered the header python writes in place.
        m.shift_remove(&field);
    }
    routes::respond(
        StatusCode::OK,
        &[
            (
                header::HeaderName::from_static(PAYLOAD_HEADER),
                py_json_dumps(&head, true, None),
            ),
            (header::HeaderName::from_static(BYTES_FIELD_HEADER), field),
        ],
        Some(OCTET_CT),
        body.bytes,
    )
}

/// python `_json_reply`'s refusal: a reply holding bytes anywhere but a
/// top-level key has no JSON form on this leg — a stated `transport_error`, 500.
fn no_json_form() -> Response {
    let r = fantastic_kernel::refusal::refusal_hint(
        fantastic_kernel::refusal::TRANSPORT_ERROR,
        "web_rest: the reply has no JSON representation on this leg",
        "this verb answers with in-process objects; call it in-process, not over HTTP",
    );
    routes::respond(
        StatusCode::INTERNAL_SERVER_ERROR,
        &[],
        Some("application/json"),
        r.to_string().into_bytes(),
    )
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

/// CPython `json.detect_encoding` + `b.decode(<enc>, "surrogatepass")` — the
/// first half of `json.loads(bytes)` (A13.11). `None` = the bytes do not decode,
/// which in python is an uncaught `UnicodeDecodeError` (a 500).
///
/// A HEADER goes through `json.loads(str)` instead, which does NOT detect an
/// encoding and REFUSES a leading BOM — so this is the body's path only.
///
/// RESIDUAL: python decodes with `surrogatepass`, so a lone surrogate survives
/// into the `str` it parses. A rust `String` cannot hold one, so such a body
/// reads as undecodable here instead of parsing.
fn decode_json_bytes(b: &[u8]) -> Option<String> {
    const BOM_UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];
    const BOM_UTF32_BE: &[u8] = &[0x00, 0x00, 0xFE, 0xFF];
    const BOM_UTF32_LE: &[u8] = &[0xFF, 0xFE, 0x00, 0x00];
    const BOM_UTF16_BE: &[u8] = &[0xFE, 0xFF];
    const BOM_UTF16_LE: &[u8] = &[0xFF, 0xFE];

    // python's ladder, in its order: a UTF-32 BOM before a UTF-16 one (the
    // UTF-32-LE BOM starts with the UTF-16-LE BOM), then UTF-8, then the
    // null-byte pattern of a BOM-less body.
    if b.starts_with(BOM_UTF32_BE) {
        return decode_utf32(&b[4..], true);
    }
    if b.starts_with(BOM_UTF32_LE) {
        return decode_utf32(&b[4..], false);
    }
    if b.starts_with(BOM_UTF16_BE) {
        return decode_utf16(&b[2..], true);
    }
    if b.starts_with(BOM_UTF16_LE) {
        return decode_utf16(&b[2..], false);
    }
    if b.starts_with(BOM_UTF8) {
        // `utf-8-sig` — the codec eats the BOM.
        return String::from_utf8(b[3..].to_vec()).ok();
    }
    if b.len() >= 4 {
        if b[0] == 0 {
            return if b[1] != 0 {
                decode_utf16(b, true)
            } else {
                decode_utf32(b, true)
            };
        }
        if b[1] == 0 {
            return if b[2] != 0 || b[3] != 0 {
                decode_utf16(b, false)
            } else {
                decode_utf32(b, false)
            };
        }
    } else if b.len() == 2 {
        if b[0] == 0 {
            return decode_utf16(b, true);
        }
        if b[1] == 0 {
            return decode_utf16(b, false);
        }
    }
    String::from_utf8(b.to_vec()).ok()
}

fn decode_utf16(b: &[u8], big_endian: bool) -> Option<String> {
    let (pairs, rest) = b.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs
        .iter()
        .map(|c| {
            if big_endian {
                u16::from_be_bytes(*c)
            } else {
                u16::from_le_bytes(*c)
            }
        })
        .collect();
    String::from_utf16(&units).ok()
}

fn decode_utf32(b: &[u8], big_endian: bool) -> Option<String> {
    let (quads, rest) = b.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    quads
        .iter()
        .map(|c| {
            let n = if big_endian {
                u32::from_be_bytes(*c)
            } else {
                u32::from_le_bytes(*c)
            };
            char::from_u32(n)
        })
        .collect()
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

/// The upgrade, as uvicorn's websockets server answers it: axum checks the
/// request (method, `Connection`, `Upgrade`, version, key — its own refusals),
/// then `Sec-WebSocket-Extensions` is negotiated as python's
/// `ServerPerMessageDeflateFactory()` does ([`ws_deflate::server_negotiate`]),
/// and the `101` goes out in uvicorn's header order (`Upgrade, Connection,
/// Sec-WebSocket-Accept, Sec-WebSocket-Extensions?, date, server`). The
/// upgraded socket runs [`ws::ws_loop`] over a [`ws_deflate::DeflateStream`]
/// carrying what was negotiated.
///
/// A header python cannot parse is its `400`, a compressor its zlib cannot
/// build (a 256-byte server window) its `500` — each with python's body.
async fn ws_upgrade(
    state: AppState,
    leg: AgentId,
    caps: Vec<String>,
    req: Request<Body>,
) -> Response {
    use fantastic_io_bridge::ws_deflate::{self, Refused};
    let host = AgentId::from(caps.first().map(String::as_str).unwrap_or(""));
    let (mut parts, _body) = req.into_parts();
    // the upgrade handle is shared (an `Arc`): axum takes its own copy to check
    // the request is upgradable, and this one carries the socket
    let on_upgrade = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();
    if let Err(rej) = axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &()).await
    {
        return rej.into_response();
    }
    let Some(on_upgrade) = on_upgrade else {
        // axum refuses a request with no upgrade handle before this
        return StatusCode::UPGRADE_REQUIRED.into_response();
    };
    let offers: Vec<String> = parts
        .headers
        .get_all(header::SEC_WEBSOCKET_EXTENSIONS)
        .iter()
        .map(|v| routes::latin1(v.as_bytes()))
        .collect();
    let offers: Vec<&str> = offers.iter().map(String::as_str).collect();
    let negotiated = match ws_deflate::server_negotiate(&offers) {
        Ok(n) => n,
        Err(Refused::BadRequest(body)) => return handshake_refused(StatusCode::BAD_REQUEST, body),
        Err(Refused::ServerError(body)) => {
            return handshake_refused(StatusCode::INTERNAL_SERVER_ERROR, body)
        }
    };
    let accept = parts
        .headers
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|k| tokio_tungstenite::tungstenite::handshake::derive_accept_key(k.as_bytes()));
    let params = negotiated.as_ref().map(|(_, p)| *p);
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        let Ok(io) =
            ws_deflate::DeflateStream::negotiated(hyper_util::rt::TokioIo::new(upgraded), params)
        else {
            return;
        };
        let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        ws::ws_loop(state, socket, host, leg).await;
    });
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = resp.headers_mut();
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Some(a) = accept.and_then(|a| HeaderValue::from_str(&a).ok()) {
        h.insert(header::SEC_WEBSOCKET_ACCEPT, a);
    }
    if let Some(v) = negotiated.and_then(|(answer, _)| HeaderValue::from_str(&answer).ok()) {
        h.insert(header::SEC_WEBSOCKET_EXTENSIONS, v);
    }
    if let Ok(v) = HeaderValue::from_str(&routes::http_date()) {
        h.insert(header::DATE, v);
    }
    h.insert(header::SERVER, HeaderValue::from_static("uvicorn"));
    resp
}

/// python's aborted handshake (`legacy/server.py`): the status, `Date`,
/// `Content-Length`, `Content-Type: text/plain`, `Connection: close` and the
/// body — no `server` header.
fn handshake_refused(status: StatusCode, body: String) -> Response {
    let len = body.len();
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&routes::http_date()) {
        h.insert(header::DATE, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    h.insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}
