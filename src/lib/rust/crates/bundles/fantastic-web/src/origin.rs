//! python `web/app.py` — the host's two closed defaults: bind LOOPBACK unless the
//! record names a `host`, and the Origin guard in front of every route.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use fantastic_bundle::{py_json_dumps, py_repr, py_str, py_truthy};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

use crate::routes;

/// python `DEFAULT_HOST` — the web host binds LOOPBACK unless its record names a
/// `host`. `0.0.0.0` is an explicit choice, never a fallback.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// python `rec.get("host") or DEFAULT_HOST`.
pub(crate) fn effective_host(rec: &Map<String, Value>) -> String {
    match rec.get("host") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => DEFAULT_HOST.to_string(),
    }
}

/// python `own_origins(rec)` — `http://<host>:<port>`, plus both loopback
/// spellings when it binds loopback. NEVER derived from the request Host header.
pub(crate) fn own_origins(rec: &Map<String, Value>) -> BTreeSet<String> {
    let host = effective_host(rec);
    let port = rec.get("port").map_or_else(|| "None".to_string(), py_str);
    let mut out = BTreeSet::from([format!("http://{host}:{port}")]);
    if matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1") {
        out.insert(format!("http://127.0.0.1:{port}"));
        out.insert(format!("http://localhost:{port}"));
        out.insert(format!("http://[::1]:{port}"));
    }
    out
}

/// The record's `allowed_origins`: a list of exact strings; a bare string counts
/// as a one-item list (`k=v` on the CLI yields a string).
pub(crate) fn extra_origins(rec: &Map<String, Value>) -> Vec<String> {
    match rec.get("allowed_origins") {
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// python `origin_allowed(rec, origin)` — no Origin ⇒ a non-browser client,
/// allowed (the leg's `ingress_rule` still gates it); else own or listed.
pub(crate) fn origin_allowed(rec: &Map<String, Value>, origin: Option<&str>) -> bool {
    let Some(origin) = origin.filter(|o| !o.is_empty()) else {
        return true;
    };
    own_origins(rec).contains(origin) || extra_origins(rec).iter().any(|o| o == origin)
}

/// Reflect's `allowed_origins`: own origins sorted, then the record's extra ones.
pub(crate) fn reflect_origins(rec: &Map<String, Value>) -> Vec<String> {
    let own = own_origins(rec);
    let mut out: Vec<String> = own.iter().cloned().collect();
    out.extend(extra_origins(rec).into_iter().filter(|o| !own.contains(o)));
    out
}

/// python `_OriginGuard` — a websocket upgrade, or an HTTP request that is not
/// GET/HEAD/OPTIONS, from a FOREIGN browser origin is refused (403 with a coded
/// body on HTTP, a refused upgrade on a websocket) and logged. `rec` is read
/// per request by the caller. `None` ⇒ pass through.
pub(crate) fn guard(
    web_agent_id: &str,
    rec: &Map<String, Value>,
    req: &Request<Body>,
) -> Option<Response> {
    let websocket = routes::is_websocket(req);
    let gated = websocket || !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !gated {
        return None;
    }
    let origin = req
        .headers()
        .get("origin")
        .map(|v| routes::latin1(v.as_bytes()))
        .filter(|o| !o.is_empty());
    if origin_allowed(rec, origin.as_deref()) {
        return None;
    }
    let origin = origin.unwrap_or_default();
    let kind = if websocket { "websocket" } else { "http" };
    let path = routes::percent_decode(req.uri().path());
    println!(
        "  [web] {web_agent_id}: refused {kind} {path} from foreign origin {}",
        py_repr(&origin)
    );
    if websocket {
        // uvicorn's answer to a `websocket.close` before accept: 403, empty body.
        return Some(routes::respond(
            StatusCode::FORBIDDEN,
            &[],
            None,
            Vec::new(),
        ));
    }
    let body = py_json_dumps(
        &json!({
            "error": format!("origin {} is not allowed on this host", py_repr(&origin)),
            "reason": "unauthorized",
            "hint": "add it to the web record's allowed_origins",
        }),
        true,
        None,
    );
    Some(routes::respond(
        StatusCode::FORBIDDEN,
        &[],
        Some("application/json"),
        body.into_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn loopback_is_the_default_and_its_spellings_are_own() {
        assert_eq!(DEFAULT_HOST, "127.0.0.1");
        let own = own_origins(&rec(json!({"port": 8088})));
        assert!(own.contains("http://127.0.0.1:8088") && own.contains("http://localhost:8088"));
        assert!(own.contains("http://[::1]:8088"));
        assert_eq!(
            own_origins(&rec(json!({"host": "0.0.0.0", "port": 8088}))),
            BTreeSet::from(["http://0.0.0.0:8088".to_string()])
        );
    }

    #[test]
    fn origin_rule() {
        let r = rec(json!({"port": 8088}));
        assert!(origin_allowed(&r, None));
        assert!(origin_allowed(&r, Some("http://localhost:8088")));
        assert!(!origin_allowed(&r, Some("http://evil.example:8088")));
        assert!(!origin_allowed(&r, Some("http://localhost:9999")));
        let listed = rec(json!({"port": 8088, "allowed_origins": ["https://ui.example"]}));
        assert!(origin_allowed(&listed, Some("https://ui.example")));
        let bare = rec(json!({"port": 8088, "allowed_origins": "https://ui.example"}));
        assert!(origin_allowed(&bare, Some("https://ui.example")));
    }

    #[test]
    fn reflect_lists_own_sorted_then_extra() {
        let r = rec(
            json!({"port": 1, "allowed_origins": ["https://ui.example", "http://localhost:1"]}),
        );
        assert_eq!(
            reflect_origins(&r),
            vec![
                "http://127.0.0.1:1",
                "http://[::1]:1",
                "http://localhost:1",
                "https://ui.example"
            ]
        );
    }
}
