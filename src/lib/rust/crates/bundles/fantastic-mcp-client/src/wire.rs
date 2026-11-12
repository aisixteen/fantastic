//! The MCP wire — Streamable HTTP, protocol 2026-07-28 ([`PROTOCOL`]): every JSON-RPC
//! request is its own POST to the server's one endpoint, carrying its version,
//! identity and capabilities in `params._meta` (no handshake, no session) and
//! mirrored into headers: `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` (a
//! `tools/call`'s tool), `Mcp-Param-<name>` (each `x-mcp-header` argument). The answer
//! is one JSON object or an SSE stream scoped to the request (its notifications,
//! then the response). Another protocol is refused, naming what the server speaks.
//!
//! Every failure is a reply the caller reads: [`McpError`] holds a refusal from
//! the kernel's closed set (`mcp_error` carries a JSON-RPC error the server raised).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use fantastic_bundle::{py_repr, py_repr_value};
use fantastic_kernel::refusal::{
    refusal, refusal_hint, with, BAD_ARG, NOT_CONNECTED, TRANSPORT_CLOSED, TRANSPORT_ERROR,
    UNAUTHORIZED, UNHANDLED,
};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Map, Value};

/// The protocol this client speaks: per-request `_meta`, no handshake.
pub const PROTOCOL: &str = "2026-07-28";

/// How this client names itself (`io.modelcontextprotocol/clientInfo`).
pub const CLIENT_NAME: &str = "aisixteen-fantastic";
/// Its version.
pub const CLIENT_VERSION: &str = "0.1.0";
/// A connect that does not happen fails; a call itself is bounded by its caller's
/// own item timeout (the kernel never defaults one), so there is no read clock.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SAFE_INT: i64 = (1 << 53) - 1;
const TCHAR: &str = "!#$%&'*+-.^_`|~";

static IDS: AtomicU64 = AtomicU64::new(1);

/// A call that did not produce a result — `.0` is what the caller gets.
#[derive(Debug, Clone, PartialEq)]
pub struct McpError(pub Value);

fn fail(r: Value) -> McpError {
    McpError(r)
}

// ─── header values ───────────────────────────────────────────────────

/// A value as an MCP header carries it: as is when it is plain visible ASCII
/// (spaces inside), else the `=?base64?…?=` sentinel of its UTF-8 — and a plain
/// value shaped like the sentinel is encoded too, so it cannot be misread.
#[must_use]
pub fn header_value(v: &str) -> String {
    let plain = v == v.trim_matches(|c| c == ' ' || c == '\t')
        && v.chars()
            .all(|c| c == ' ' || c == '\t' || ('!'..='~').contains(&c))
        && !(v.starts_with("=?base64?") && v.ends_with("?="));
    if plain {
        return v.to_string();
    }
    format!(
        "=?base64?{}?=",
        base64::engine::general_purpose::STANDARD.encode(v.as_bytes())
    )
}

/// One mirrored argument: the header name and the `properties` path to it.
pub type Mirror = (String, Vec<String>);

/// The `x-mcp-header` annotations of a tool's `inputSchema` → `[(name, path)]`,
/// or why the tool must be excluded. Legal only on a primitive property
/// (integer/string/boolean) reached through `properties` keys alone; names are
/// non-empty RFC 9110 tokens, unique ignoring case.
///
/// # Errors
/// The reason the tool is excluded.
pub fn mirrors(schema: &Value) -> Result<Vec<Mirror>, String> {
    fn walk(node: &Value, path: &[String], out: &mut Vec<(Value, Vec<String>)>) {
        let Some(props) = node.get("properties").and_then(Value::as_object) else {
            return;
        };
        for (key, sub) in props {
            let mut p = path.to_vec();
            p.push(key.clone());
            if let Some(h) = sub.as_object().and_then(|m| m.get("x-mcp-header")) {
                out.push((h.clone(), p.clone()));
            }
            walk(sub, &p, out);
        }
    }
    fn count_all(node: &Value) -> usize {
        match node {
            Value::Object(m) => {
                usize::from(m.contains_key("x-mcp-header"))
                    + m.values().map(count_all).sum::<usize>()
            }
            Value::Array(a) => a.iter().map(count_all).sum(),
            _ => 0,
        }
    }
    let mut found = Vec::new();
    walk(schema, &[], &mut found);
    if count_all(schema) != found.len() {
        return Err("an x-mcp-header outside a chain of `properties` keys".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (name, path) in found {
        let ok = name.as_str().filter(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || TCHAR.contains(c))
        });
        let Some(n) = ok else {
            return Err(format!(
                "x-mcp-header {} is not an HTTP header token",
                py_repr_value(&name)
            ));
        };
        if !seen.insert(n.to_lowercase()) {
            return Err(format!("x-mcp-header {} is not unique", py_repr(n)));
        }
        let mut node = schema;
        for key in &path {
            node = &node["properties"][key];
        }
        let ty = node.get("type").cloned().unwrap_or(Value::Null);
        if !matches!(ty.as_str(), Some("integer" | "string" | "boolean")) {
            return Err(format!(
                "x-mcp-header {} is on a {} property — only integer, string or boolean",
                py_repr(n),
                py_repr_value(&ty)
            ));
        }
        out.push((n.to_string(), path));
    }
    Ok(out)
}

/// `Mcp-Param-<name>` for each mirrored argument present (and not null).
///
/// # Errors
/// An integer outside the range an MCP header carries.
pub fn param_headers(
    found: &[Mirror],
    arguments: &Value,
) -> Result<Vec<(String, String)>, McpError> {
    let mut out = Vec::new();
    for (name, path) in found {
        let mut node = Some(arguments);
        for key in path {
            node = node.and_then(|n| n.as_object()).and_then(|m| m.get(key));
        }
        let Some(v) = node.filter(|v| !v.is_null()) else {
            continue;
        };
        let text = match v {
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Number(n) if n.is_i64() || n.is_u64() => {
                let big = n.as_i64().is_none_or(|i| i.abs() > SAFE_INT);
                if big {
                    let mut r = refusal(
                        BAD_ARG,
                        format!(
                            "{}={n} is outside the integers an MCP header carries",
                            path.join(".")
                        ),
                    );
                    with(&mut r, "arg", json!(path[0]));
                    return Err(fail(r));
                }
                n.to_string()
            }
            Value::String(s) => s.clone(),
            other => fantastic_ai_core::helpers::shown(other),
        };
        out.push((format!("Mcp-Param-{name}"), header_value(&text)));
    }
    Ok(out)
}

// ─── one request ─────────────────────────────────────────────────────

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL,
        "io.modelcontextprotocol/clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

fn coded(code: &str, error: String, mcp_code: &Value) -> Value {
    let mut r = refusal(code, error);
    with(&mut r, "mcp_code", mcp_code.clone());
    r
}

/// A JSON-RPC error the server answered, as the caller reads it.
fn protocol_error(url: &str, err: &Value) -> McpError {
    let Some(e) = err.as_object() else {
        return fail(refusal(
            TRANSPORT_ERROR,
            format!("mcp {url}: a malformed error {}", py_repr_value(err)),
        ));
    };
    let code = e.get("code").cloned().unwrap_or(Value::Null);
    let msg = e.get("message").map(fantastic_ai_core::helpers::shown).unwrap_or_default();
    let data = e.get("data");
    match code.as_i64() {
        Some(-32602) => fail(coded(BAD_ARG, format!("mcp {url}: {msg}"), &code)),
        Some(-32601) => fail(coded(UNHANDLED, format!("mcp {url}: {msg}"), &code)),
        Some(-32022) => {
            let supported = data
                .and_then(|d| d.get("supported"))
                .cloned()
                .unwrap_or(Value::Null);
            let mut r = coded(
                TRANSPORT_ERROR,
                format!(
                    "mcp {url}: the server speaks {} — this client speaks {PROTOCOL}",
                    py_repr_value(&supported)
                ),
                &code,
            );
            with(&mut r, "supported", supported);
            fail(r)
        }
        Some(-32021) => {
            let need = data
                .and_then(|d| d.get("requiredCapabilities"))
                .cloned()
                .unwrap_or(Value::Null);
            fail(coded(
                TRANSPORT_ERROR,
                format!(
                    "mcp {url}: the server needs client capabilities {} this client does not \
                     offer",
                    py_repr_value(&need)
                ),
                &code,
            ))
        }
        Some(-32020) => {
            let mut r = refusal_hint(
                TRANSPORT_ERROR,
                format!("mcp {url}: header mismatch — {msg}"),
                "the tool's inputSchema may have changed: boot re-lists the tools",
            );
            with(&mut r, "mcp_code", code);
            fail(r)
        }
        _ => {
            let mut r = refusal(
                TRANSPORT_ERROR,
                format!("mcp {url}: error {}: {msg}", py_repr_value(&code)),
            );
            with(&mut r, "mcp_error", err.clone());
            fail(r)
        }
    }
}

fn settle(url: &str, msg: &Value) -> Result<Map<String, Value>, McpError> {
    let Some(m) = msg.as_object() else {
        return Err(fail(refusal(
            TRANSPORT_ERROR,
            format!("mcp {url}: not a JSON-RPC message"),
        )));
    };
    if let Some(err) = m.get("error") {
        return Err(protocol_error(url, err));
    }
    let Some(result) = m.get("result").and_then(Value::as_object) else {
        return Err(fail(refusal(
            TRANSPORT_ERROR,
            format!("mcp {url}: a response with no result"),
        )));
    };
    let rtype = result
        .get("resultType")
        .cloned()
        .unwrap_or_else(|| json!("complete"));
    if !matches!(rtype.as_str(), Some("complete" | "input_required")) {
        return Err(fail(refusal(
            TRANSPORT_ERROR,
            format!("mcp {url}: unknown resultType {}", py_repr_value(&rtype)),
        )));
    }
    Ok(result.clone())
}

/// What a request carries besides its method and params.
#[derive(Default)]
pub struct Opts<'a> {
    /// The Bearer key, or `None` for a keyless server.
    pub key: Option<&'a str>,
    /// Extra headers (`Mcp-Param-*`) — a modern request only.
    pub headers: Vec<(String, String)>,
}

fn header_map(url: &str, pairs: &[(String, String)]) -> Result<HeaderMap, McpError> {
    let mut head = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
            fail(refusal(
                TRANSPORT_ERROR,
                format!("mcp {url}: header {k}: {e}"),
            ))
        })?;
        let value = HeaderValue::from_str(v).map_err(|e| {
            fail(refusal(
                TRANSPORT_ERROR,
                format!("mcp {url}: header {k}: {e}"),
            ))
        })?;
        head.insert(name, value);
    }
    Ok(head)
}

fn base_headers() -> Vec<(String, String)> {
    vec![
        (
            "Accept".into(),
            "application/json, text/event-stream".into(),
        ),
        ("Content-Type".into(), "application/json".into()),
    ]
}

fn client(url: &str) -> Result<reqwest::Client, McpError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| fail(refusal(TRANSPORT_ERROR, format!("mcp {url}: {e}"))))
}

fn send_error(url: &str, e: &reqwest::Error) -> McpError {
    if e.is_connect() {
        fail(refusal(
            NOT_CONNECTED,
            format!("mcp {url}: cannot connect: {e}"),
        ))
    } else {
        fail(refusal(TRANSPORT_ERROR, format!("mcp {url}: {e}")))
    }
}

fn next_id() -> String {
    IDS.fetch_add(1, Ordering::Relaxed).to_string()
}

/// Ask the server who it is (`server/discover`) and check it speaks [`PROTOCOL`];
/// → `{server, instructions, capabilities}`.
///
/// # Errors
/// [`McpError`] — the reply the caller reads.
pub async fn connect(url: &str, key: Option<&str>) -> Result<Map<String, Value>, McpError> {
    let disc = rpc(
        url,
        "server/discover",
        Map::new(),
        Opts {
            key,
            headers: vec![],
        },
        |_| async {},
    )
    .await?;
    let versions = disc
        .get("supportedVersions")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let names: Vec<&str> = versions
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !names.contains(&PROTOCOL) {
        return Err(fail(refusal(
            TRANSPORT_ERROR,
            format!(
                "mcp {url}: the server speaks {} — this client speaks {PROTOCOL}",
                py_repr_value(&versions)
            ),
        )));
    }
    let server = disc
        .get("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/serverInfo"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("server".into(), server);
    out.insert(
        "instructions".into(),
        disc.get("instructions").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "capabilities".into(),
        disc.get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    Ok(out)
}

/// POST one request; → its `result`, or [`McpError`]. Notifications on an SSE answer
/// go to `on_note` as they arrive. `opts.headers` are the mirrored `Mcp-Param-*`.
///
/// # Errors
/// [`McpError`] — the reply the caller reads.
pub async fn rpc<F, Fut>(
    url: &str,
    method: &str,
    params: Map<String, Value>,
    opts: Opts<'_>,
    mut on_note: F,
) -> Result<Map<String, Value>, McpError>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = ()>,
{
    let rid = next_id();
    let mut p = params;
    p.insert("_meta".into(), meta());
    let name = p.get("name").and_then(Value::as_str).map(str::to_string);
    let body = json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": p});
    let mut h = base_headers();
    h.push(("MCP-Protocol-Version".into(), PROTOCOL.into()));
    h.push(("Mcp-Method".into(), method.into()));
    if method == "tools/call" {
        h.push((
            "Mcp-Name".into(),
            header_value(name.as_deref().unwrap_or("")),
        ));
    }
    h.extend(opts.headers.iter().cloned());
    if let Some(k) = opts.key {
        h.push(("Authorization".into(), format!("Bearer {k}")));
    }
    let resp = client(url)?
        .post(url)
        .headers(header_map(url, &h)?)
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| send_error(url, &e))?;
    read(url, &rid, resp, opts.key, &mut on_note).await
}

async fn read<F, Fut>(
    url: &str,
    rid: &str,
    resp: reqwest::Response,
    key: Option<&str>,
    on_note: &mut F,
) -> Result<Map<String, Value>, McpError>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = ()>,
{
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        let why = if key.is_some() {
            "refused the key"
        } else {
            "needs a key"
        };
        let mut r = refusal_hint(
            UNAUTHORIZED,
            format!("mcp {url}: HTTP {status} — the server {why}"),
            "the key is a credential-store ref: credentials={store, ref}, field api_key",
        );
        with(&mut r, "http_status", json!(status));
        return Err(fail(r));
    }
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let wire_err = |e: reqwest::Error| fail(refusal(TRANSPORT_ERROR, format!("mcp {url}: {e}")));
    if status >= 400 {
        let raw = resp.bytes().await.map_err(wire_err)?;
        let msg = serde_json::from_slice::<Value>(&raw).ok();
        if let Some(err) = msg
            .as_ref()
            .and_then(|m| m.get("error"))
            .filter(|e| e.is_object())
        {
            return Err(protocol_error(url, err));
        }
        let text: String = String::from_utf8_lossy(&raw).chars().take(300).collect();
        let mut r = refusal(TRANSPORT_ERROR, format!("mcp {url}: HTTP {status}: {text}"));
        with(&mut r, "http_status", json!(status));
        return Err(fail(r));
    }
    if ctype == "application/json" {
        let raw = resp.bytes().await.map_err(wire_err)?;
        let msg: Value = serde_json::from_slice(&raw).map_err(|e| {
            fail(refusal(
                TRANSPORT_ERROR,
                format!("mcp {url}: the answer is not JSON: {e}"),
            ))
        })?;
        return settle(url, &msg);
    }
    if ctype == "text/event-stream" {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut data: Vec<String> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(wire_err)?);
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&raw[..raw.len() - 1]);
                let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
                if line.starts_with(':') {
                    continue; // an SSE comment (keep-alive)
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    continue;
                }
                if line.is_empty() && !data.is_empty() {
                    let event = data.join("\n");
                    data.clear();
                    let msg: Value = serde_json::from_str(&event).map_err(|e| {
                        fail(refusal(
                            TRANSPORT_ERROR,
                            format!("mcp {url}: an SSE event is not JSON: {e}"),
                        ))
                    })?;
                    if msg.is_object() && msg.get("id").is_none() && msg.get("method").is_some() {
                        on_note(msg).await;
                        continue;
                    }
                    if msg.get("id").and_then(Value::as_str) == Some(rid) {
                        return settle(url, &msg);
                    }
                }
            }
        }
        return Err(fail(refusal(
            TRANSPORT_CLOSED,
            format!("mcp {url}: the stream ended before the response"),
        )));
    }
    let shown = if ctype.is_empty() {
        "none".to_string()
    } else {
        ctype
    };
    Err(fail(refusal(
        TRANSPORT_ERROR,
        format!("mcp {url}: unexpected content-type {}", py_repr(&shown)),
    )))
}
