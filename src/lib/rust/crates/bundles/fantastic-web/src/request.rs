//! The outbound `request` verb (issue #695): one HTTP call to a host the record's
//! `request_allow` names, the host's key read from a credential store at use.
//!
//! Nothing is kept: no cookies, no proxy, no cache. The only state is a short
//! in-memory ring of the latest outcomes the agent's view shows (never persisted,
//! never holding a header, a body or a key).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use fantastic_kernel::refusal::{
    error_text, refusal, refusal_hint, with, BAD_ARG, DISABLED, NOT_ALLOWED, STORE_REFUSED,
    TIMEOUT, TRANSPORT_ERROR,
};
use fantastic_kernel::{AgentId, Item, Kernel, Status};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, LOCATION};
use reqwest::{Method, Url};
use serde_json::{json, Map, Value};

use crate::verbs::Request;

/// The most reply-body bytes `request` returns; past it `truncated` is true.
pub const BODY_CAP: usize = 1_048_576;
/// Seconds a call may take when `timeout` is absent.
pub const DEFAULT_TIMEOUT: f64 = 30.0;
/// The most seconds `timeout` may ask for.
pub const MAX_TIMEOUT: f64 = 120.0;
/// Redirect hops followed (each one checked against the allowlist).
const MAX_REDIRECTS: usize = 5;
/// Outcomes the view keeps per agent.
const RECENT_MAX: usize = 20;
/// The credential field sent as `Authorization: Bearer <value>`.
const KEY_FIELD: &str = "api_key";
/// Headers the caller may not set: the key and the connection are the agent's.
const FORBIDDEN_HEADERS: [&str; 4] = ["authorization", "proxy-authorization", "cookie", "host"];

// ─── the allowlist ──────────────────────────────────────────────────

/// One `request_allow` entry, parsed.
struct Allow {
    scheme: String,
    host: String,
    port: Option<u16>,
    /// `*.host`: every subdomain of `host`, not `host` itself.
    wildcard: bool,
}

fn parse_allow(entry: &str) -> Option<Allow> {
    let entry = entry.trim();
    let (scheme_part, rest) = entry.split_once("://")?;
    let (wildcard, rest) = match rest.strip_prefix("*.") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let url = Url::parse(&format!("{scheme_part}://{rest}")).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return None;
    }
    Some(Allow {
        scheme: url.scheme().to_string(),
        host: url.host_str()?.to_string(),
        port: url.port_or_known_default(),
        wildcard,
    })
}

/// The record's `request_allow`, as strings.
pub(crate) fn allow_list(rec: &Map<String, Value>) -> Vec<String> {
    rec.get("request_allow")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `url`'s scheme, host and port match an entry of `allow` exactly.
fn allowed(url: &Url, allow: &[String]) -> bool {
    let (Some(host), port) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    allow.iter().filter_map(|e| parse_allow(e)).any(|a| {
        a.scheme == url.scheme()
            && a.port == port
            && if a.wildcard {
                host.len() > a.host.len() + 1 && host.ends_with(&format!(".{}", a.host))
            } else {
                a.host == host
            }
    })
}

/// `host` or `host:port` (a non-default port) — what a credential ref is bound to.
fn netloc(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_lowercase();
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

// ─── the credential ─────────────────────────────────────────────────

fn store_ids(kernel: &Kernel) -> Vec<AgentId> {
    let mut ids: Vec<AgentId> = kernel
        .agents
        .iter()
        .filter(|e| {
            e.value().handler_module.as_deref() == Some(fantastic_credential_store::HANDLER_MODULE)
        })
        .map(|e| e.key().clone())
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ids
}

/// The `(store, ref)` bound to `netloc` that holds an `api_key`: stores in id
/// order, refs in the store's order, the first match. A store that refuses the
/// `list` (this agent is not in its `callable_by`) is passed over.
async fn find_ref(kernel: &Arc<Kernel>, netloc: &str) -> Option<(String, String)> {
    for store in store_ids(kernel) {
        let out = kernel
            .send(vec![Item::new(store.clone(), json!({"type": "list"}))])
            .await
            .remove(0);
        if out.status != Status::Completed {
            continue;
        }
        let Some(refs) = out.result.get("refs").and_then(Value::as_array) else {
            continue;
        };
        for row in refs {
            let bound = row.get("host").and_then(Value::as_str).unwrap_or("");
            let has_key = match row.get("fields") {
                Some(Value::Array(a)) => a.iter().any(|f| f.as_str() == Some(KEY_FIELD)),
                Some(Value::Object(m)) => m.contains_key(KEY_FIELD),
                _ => false,
            };
            if has_key && bound == netloc {
                if let Some(r) = row.get("ref").and_then(Value::as_str) {
                    return Some((store.as_str().to_string(), r.to_string()));
                }
            }
        }
    }
    None
}

/// The credential ref names (`store/ref`, never a value) for each host of the
/// allowlist — what the view shows.
pub(crate) async fn bound_refs(kernel: &Arc<Kernel>, allow: &[String]) -> Value {
    let mut out = Map::new();
    for entry in allow {
        let Some(a) = parse_allow(entry) else {
            continue;
        };
        let loc = match (a.wildcard, a.port) {
            (true, _) => {
                out.insert(entry.clone(), Value::Null);
                continue;
            }
            (false, Some(p)) if Some(p) != default_port(&a.scheme) => format!("{}:{p}", a.host),
            _ => a.host.clone(),
        };
        let name = find_ref(kernel, &loc.to_lowercase())
            .await
            .map(|(s, r)| format!("{s}/{r}"));
        out.insert(entry.clone(), name.map_or(Value::Null, Value::String));
    }
    Value::Object(out)
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// Whether a credential may travel to `url`: only over https, or to a loopback
/// host (127.0.0.1, ::1, localhost). Over plain http to any other host the call
/// goes WITHOUT the credential (it is not read from the store at all).
fn may_carry_key(url: &Url) -> bool {
    url.scheme() == "https"
        || matches!(
            url.host_str().map(str::to_ascii_lowercase).as_deref(),
            Some("127.0.0.1" | "[::1]" | "localhost")
        )
}

/// The key for `url`'s host, read from the store at use; `None` when no ref is
/// bound to the host or the scheme may not carry it. A store that answers without the key is a refusal naming
/// the ref, never a value.
async fn secret_for(kernel: &Arc<Kernel>, url: &Url) -> Result<Option<String>, Value> {
    let loc = netloc(url);
    if !may_carry_key(url) {
        return Ok(None);
    }
    let Some((store, r)) = find_ref(kernel, &loc).await else {
        return Ok(None);
    };
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(store.as_str()),
            json!({"type": "get", "ref": r, "host": loc}),
        )])
        .await
        .remove(0);
    let key = out
        .result
        .get("fields")
        .and_then(|f| f.get(KEY_FIELD))
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty());
    match (out.status == Status::Completed, key) {
        (true, Some(k)) if HeaderValue::from_str(&format!("Bearer {k}")).is_ok() => {
            Ok(Some(k.to_string()))
        }
        _ => {
            let why = error_text(&out.result)
                .unwrap_or_else(|| format!("{store}/{r} holds no usable {KEY_FIELD}"));
            let mut e = refusal(
                STORE_REFUSED,
                format!("request: credential {store}/{r} for {loc}: {why}"),
            );
            with(&mut e, "store_error", json!(why));
            if let Some(reason) = out.result.get("reason") {
                with(&mut e, "store_reason", reason.clone());
            }
            Err(e)
        }
    }
}

// ─── the recent ring ────────────────────────────────────────────────

fn ring() -> &'static Mutex<HashMap<AgentId, VecDeque<Value>>> {
    static RING: OnceLock<Mutex<HashMap<AgentId, VecDeque<Value>>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One finished call's row: method, host, status (null when it failed), time.
fn remember(agent: &AgentId, method: &str, host: &str, status: Option<u16>) {
    let mut ring = ring().lock().expect("request ring poisoned");
    let rows = ring.entry(agent.clone()).or_default();
    rows.push_front(json!({"method": method, "host": host, "status": status, "at": now_secs()}));
    rows.truncate(RECENT_MAX);
}

/// The latest outcomes of `agent`, newest first.
pub(crate) fn recent(agent: &AgentId) -> Value {
    let ring = ring().lock().expect("request ring poisoned");
    Value::Array(
        ring.get(agent)
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default(),
    )
}

/// Forget `agent`'s ring (the agent is gone).
pub(crate) fn forget(agent: &AgentId) {
    ring().lock().expect("request ring poisoned").remove(agent);
}

// ─── the call ───────────────────────────────────────────────────────

fn bad(arg: &str, msg: impl Into<String>, hint: &str) -> Value {
    let mut e = refusal_hint(BAD_ARG, msg, hint);
    with(&mut e, "arg", json!(arg));
    e
}

fn not_allowed(url: &Url, why: &str) -> Value {
    let mut e = refusal_hint(
        NOT_ALLOWED,
        format!(
            "request: host {} is not in request_allow ({why})",
            netloc(url)
        ),
        "name the host in the web agent's request_allow as scheme://host[:port]",
    );
    with(&mut e, "host", json!(netloc(url)));
    e
}

/// Replace every occurrence of `needle` in `hay` with `[secret]`.
fn redact(hay: &[u8], needle: &[u8]) -> Vec<u8> {
    if needle.is_empty() || hay.len() < needle.len() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(b"[secret]");
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// The body to return: the key redacted FIRST, then the cap applied. `more` says
/// the reader stopped with bytes left. Returns `(body, truncated)`; a partial key
/// left at the very end of a cut body is dropped.
fn finish_body(raw: Vec<u8>, more: bool, key: Option<&[u8]>) -> (Vec<u8>, bool) {
    let mut body = key.map_or(raw.clone(), |k| redact(&raw, k));
    let mut truncated = more;
    if body.len() > BODY_CAP {
        body.truncate(BODY_CAP);
        truncated = true;
    }
    if let (true, Some(k)) = (truncated, key) {
        for n in (1..k.len().min(body.len() + 1)).rev() {
            if body.ends_with(&k[..n]) {
                body.truncate(body.len() - n);
                break;
            }
        }
    }
    (body, truncated)
}

/// A cut inside a multi-byte character stays text: back to the last whole one.
fn whole_chars(buf: &mut Vec<u8>) {
    if let Err(e) = std::str::from_utf8(buf) {
        if e.error_len().is_none() {
            buf.truncate(e.valid_up_to());
        }
    }
}

/// What a finished transport error says, with nothing from the request in it.
fn transport(e: &reqwest::Error) -> Value {
    if e.is_timeout() {
        return refusal_hint(
            TIMEOUT,
            "request: the host did not answer in time",
            "raise timeout (at most 120 seconds) or retry",
        );
    }
    let what = if e.is_connect() {
        "could not connect to the host"
    } else if e.is_redirect() {
        "too many redirects"
    } else if e.is_body() || e.is_decode() {
        "the reply body could not be read"
    } else {
        "the call failed in transit"
    };
    refusal(TRANSPORT_ERROR, format!("request: {what}"))
}

fn parse_method(m: &str) -> Option<Method> {
    match m.to_ascii_uppercase().as_str() {
        "GET" => Some(Method::GET),
        "HEAD" => Some(Method::HEAD),
        "POST" => Some(Method::POST),
        "PUT" => Some(Method::PUT),
        "PATCH" => Some(Method::PATCH),
        "DELETE" => Some(Method::DELETE),
        "OPTIONS" => Some(Method::OPTIONS),
        _ => None,
    }
}

/// The checked inputs of one call.
struct Plan {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    json_body: bool,
    timeout: Duration,
}

fn plan(a: &Request, allow: &[String]) -> Result<Plan, Value> {
    if allow.iter().all(|e| parse_allow(e).is_none()) {
        return Err(refusal_hint(
            DISABLED,
            "request: this web agent's record names no request_allow host",
            "update_agent the web agent with request_allow=[\"https://host\"]",
        ));
    }
    let Some(method) = parse_method(&a.method) else {
        return Err(bad(
            "method",
            format!(
                "request: method {:?} is not one of GET HEAD POST PUT PATCH DELETE OPTIONS",
                a.method
            ),
            "use one of GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS",
        ));
    };
    let mut url = Url::parse(&a.url).map_err(|_| {
        bad(
            "url",
            "request: url is not an absolute http(s) URL",
            "give scheme://host[:port]/path",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(bad(
            "url",
            "request: url must be http(s) with a host and no user information",
            "give scheme://host[:port]/path",
        ));
    }
    if !allowed(&url, allow) {
        return Err(not_allowed(
            &url,
            "scheme, host and port must match exactly",
        ));
    }
    if let Some(q) = &a.query {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in q {
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => {
                    return Err(bad(
                        "query",
                        format!(
                            "request: query value of {k:?} must be text, a number or a boolean"
                        ),
                        "pass scalar query values",
                    ))
                }
            };
            pairs.append_pair(k, &text);
        }
    }
    let mut headers = HeaderMap::new();
    for (k, v) in a.headers.iter().flatten() {
        if FORBIDDEN_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
            return Err(bad(
                "headers",
                format!("request: header {k:?} is the agent's, not the caller's"),
                "the host's credential supplies the Authorization header; drop it",
            ));
        }
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) else {
            return Err(bad(
                "headers",
                format!("request: header {k:?} is not a legal header"),
                "use plain header names and text values",
            ));
        };
        headers.insert(name, value);
    }
    let (body, json_body) = match (&a.json, &a.body) {
        (Some(_), Some(_)) => {
            return Err(bad(
                "body",
                "request: give json or body, not both",
                "drop one of them",
            ))
        }
        (Some(j), None) => (Some(serde_json::to_vec(j).unwrap_or_default()), true),
        (None, Some(b)) => (Some(b.clone().into_bytes()), false),
        (None, None) => (None, false),
    };
    let secs = a.timeout.unwrap_or(DEFAULT_TIMEOUT);
    if !(0.001..=MAX_TIMEOUT).contains(&secs) {
        return Err(bad(
            "timeout",
            format!("request: timeout must be between 0.001 and {MAX_TIMEOUT} seconds"),
            "give seconds",
        ));
    }
    Ok(Plan {
        method,
        url,
        headers,
        body,
        json_body,
        timeout: Duration::from_secs_f64(secs),
    })
}

/// Serve one `request`.
pub(crate) async fn serve(
    agent: &AgentId,
    a: &Request,
    rec: &Map<String, Value>,
    kernel: &Arc<Kernel>,
) -> Value {
    let allow = allow_list(rec);
    let plan = match plan(a, &allow) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let method = plan.method.to_string();
    let host = netloc(&plan.url);
    let deadline = plan.timeout;
    let run = run(plan, &allow, kernel);
    let (out, last_host, status) = match tokio::time::timeout(deadline, run).await {
        Ok(r) => r,
        Err(_) => (
            refusal_hint(
                TIMEOUT,
                "request: the host did not answer in time",
                "raise timeout (at most 120 seconds) or retry",
            ),
            host.clone(),
            None,
        ),
    };
    remember(agent, &method, &last_host, status);
    // WHAT CHANGED, PUBLISHED on the agent's own inbox: its view re-reads `recent_requests`
    kernel
        .emit(agent, json!({"type": "request_served", "method": method, "host": last_host, "status": status}))
        .await;
    out
}

/// The hops: `(reply or refusal, the last host asked, its status)`.
async fn run(plan: Plan, allow: &[String], kernel: &Arc<Kernel>) -> (Value, String, Option<u16>) {
    let Plan {
        mut method,
        mut url,
        headers,
        mut body,
        json_body,
        timeout,
    } = plan;
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return (
                refusal(TRANSPORT_ERROR, "request: no HTTP client could be built"),
                netloc(&url),
                None,
            )
        }
    };
    let mut hops = 0;
    loop {
        let host = netloc(&url);
        let secret = match secret_for(kernel, &url).await {
            Ok(s) => s,
            Err(e) => return (e, host, None),
        };
        let mut req = client
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        if let Some(b) = &body {
            if json_body && !headers.contains_key(CONTENT_TYPE) {
                req = req.header(CONTENT_TYPE, "application/json");
            }
            req = req.body(b.clone());
        }
        if let Some(k) = &secret {
            if let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {k}")) {
                v.set_sensitive(true);
                req = req.header(AUTHORIZATION, v);
            }
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return (transport(&e), host, None),
        };
        let status = resp.status();
        if status.is_redirection() {
            if let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) {
                hops += 1;
                let Ok(next) = url.join(loc) else {
                    return (
                        bad(
                            "url",
                            "request: a redirect named an unusable location",
                            "check the host",
                        ),
                        host,
                        Some(status.as_u16()),
                    );
                };
                if hops > MAX_REDIRECTS {
                    return (
                        refusal(TRANSPORT_ERROR, "request: too many redirects"),
                        host,
                        Some(status.as_u16()),
                    );
                }
                if !allowed(&next, allow) {
                    return (
                        not_allowed(&next, "a redirect left the allowlist"),
                        host,
                        Some(status.as_u16()),
                    );
                }
                let keeps = matches!(status.as_u16(), 307 | 308);
                if !keeps && !matches!(method, Method::GET | Method::HEAD) {
                    method = Method::GET;
                    body = None;
                }
                url = next;
                continue;
            }
        }
        // read up to the cap plus a key's length less one, so a key straddling the
        // cap is whole when it is redacted; the cut comes after (see `finish_body`)
        let limit = BODY_CAP + secret.as_deref().map_or(0, |k| k.len().saturating_sub(1));
        let mut buf: Vec<u8> = Vec::new();
        let mut more = false;
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    if buf.len() + c.len() > limit {
                        buf.extend_from_slice(&c[..limit - buf.len()]);
                        more = true;
                        break;
                    }
                    buf.extend_from_slice(&c);
                }
                Ok(None) => break,
                Err(e) => return (transport(&e), host, Some(status.as_u16())),
            }
        }
        let secret_bytes = secret.as_deref().map(str::as_bytes);
        let mut out = Map::new();
        out.insert("status".into(), json!(status.as_u16()));
        let mut hdrs = Map::new();
        for (k, v) in resp.headers() {
            if k.as_str() == "set-cookie" {
                continue;
            }
            let mut text = String::from_utf8_lossy(v.as_bytes()).into_owned();
            if let Some(s) = secret_bytes {
                text = String::from_utf8_lossy(&redact(text.as_bytes(), s)).into_owned();
            }
            hdrs.insert(k.as_str().to_string(), json!(text));
        }
        let is_json = hdrs
            .get("content-type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.to_ascii_lowercase().contains("json"));
        out.insert("headers".into(), Value::Object(hdrs));
        let (mut buf, truncated) = finish_body(buf, more, secret_bytes);
        if truncated {
            whole_chars(&mut buf);
        }
        match String::from_utf8(buf) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) if is_json && !truncated => out.insert("json".into(), v),
                _ => out.insert("text".into(), Value::String(text)),
            },
            Err(e) => out.insert(
                "bytes".into(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(e.into_bytes())),
            ),
        };
        out.insert("truncated".into(), json!(truncated));
        return (Value::Object(out), host, Some(status.as_u16()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    #[test]
    fn the_allowlist_matches_scheme_host_and_port_exactly() {
        let allow = vec![
            "https://api.example.com".to_string(),
            "http://127.0.0.1:8080".to_string(),
        ];
        assert!(allowed(&url("https://api.example.com/v1/x?y=1"), &allow));
        assert!(allowed(&url("https://api.example.com:443/"), &allow));
        assert!(allowed(&url("http://127.0.0.1:8080/a"), &allow));
        assert!(!allowed(&url("http://api.example.com/"), &allow), "scheme");
        assert!(
            !allowed(&url("https://api.example.com:8443/"), &allow),
            "port"
        );
        assert!(
            !allowed(&url("https://api.example.com.evil.test/"), &allow),
            "suffix"
        );
        assert!(
            !allowed(&url("https://evil.test/@api.example.com"), &allow),
            "path trick"
        );
        assert!(!allowed(&url("http://127.0.0.1:9/"), &allow));
    }

    #[test]
    fn a_wildcard_admits_subdomains_only() {
        let allow = vec!["https://*.example.com".to_string()];
        assert!(allowed(&url("https://a.example.com/"), &allow));
        assert!(allowed(&url("https://a.b.example.com/"), &allow));
        assert!(!allowed(&url("https://example.com/"), &allow));
        assert!(!allowed(&url("https://badexample.com/"), &allow));
        assert!(!allowed(&url("http://a.example.com/"), &allow));
    }

    #[test]
    fn a_malformed_entry_matches_nothing() {
        for e in [
            "example.com",
            "ftp://example.com",
            "https://example.com/path",
            "https://u:p@example.com",
            "",
        ] {
            assert!(parse_allow(e).is_none(), "{e}");
        }
    }

    #[test]
    fn redaction_replaces_every_occurrence() {
        assert_eq!(
            redact(b"a SECRET b SECRET", b"SECRET"),
            b"a [secret] b [secret]"
        );
        assert_eq!(redact(b"none", b"SECRET"), b"none");
    }

    #[test]
    fn a_key_straddling_the_cap_is_redacted_before_the_cut() {
        let key = b"SECRETKEY12345";
        let mut raw = vec![b'x'; BODY_CAP - 5];
        raw.extend_from_slice(key); // starts 5 bytes before the cap
        raw.extend_from_slice(b"tail");
        let (body, truncated) = finish_body(raw, true, Some(key));
        assert!(truncated);
        assert!(body.len() <= BODY_CAP);
        let text = String::from_utf8(body).unwrap();
        assert!(!text.contains("SECRET") && !text.contains("12345"), "leak");
        assert!(
            text.ends_with("[secr"),
            "redacted whole, then cut: {}",
            &text[BODY_CAP - 8..]
        );
        // an uncut partial key at the very end of a cut body is dropped too
        let mut raw = vec![b'x'; BODY_CAP - 3];
        raw.extend_from_slice(&key[..8]);
        let (body, _) = finish_body(raw, true, Some(key));
        assert_eq!(body.len(), BODY_CAP - 3, "the partial key at the end is dropped");
        assert!(body.iter().all(|&b| b == b'x'));
    }

    #[test]
    fn a_cut_inside_a_multi_byte_character_stays_text() {
        let mut buf = "ab\u{e9}".as_bytes().to_vec(); // 'é' is two bytes
        buf.pop(); // cut inside it
        whole_chars(&mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "ab");
        let mut whole = "ab\u{e9}".as_bytes().to_vec();
        whole_chars(&mut whole);
        assert_eq!(String::from_utf8(whole).unwrap(), "ab\u{e9}");
    }

    #[test]
    fn a_credential_goes_over_https_or_loopback_only() {
        assert!(may_carry_key(&url("https://api.example.com/")));
        assert!(may_carry_key(&url("http://127.0.0.1:8080/")));
        assert!(may_carry_key(&url("http://[::1]:8080/")));
        assert!(may_carry_key(&url("http://localhost:8080/")));
        assert!(!may_carry_key(&url("http://api.example.com/")));
        assert!(!may_carry_key(&url("http://10.0.0.5:8080/")));
    }

    #[test]
    fn the_ring_keeps_the_latest_first_and_is_bounded() {
        let id = AgentId::from("ring_test_agent");
        for i in 0..(RECENT_MAX + 5) {
            remember(&id, "GET", "h", Some(200 + u16::try_from(i).unwrap()));
        }
        let rows = recent(&id);
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), RECENT_MAX);
        assert_eq!(rows[0]["status"], json!(200 + RECENT_MAX as u64 + 4));
        forget(&id);
        assert_eq!(recent(&id), json!([]));
    }
}
