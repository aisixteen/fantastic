//! python `web/app.py` — the host's closed defaults: bind LOOPBACK unless the
//! record names a `host`, and the Host + Origin guard in front of every route.

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

/// The binds that answer the loopback spellings too: a loopback bind, and a
/// WILDCARD (`0.0.0.0`, `::`), which listens on every address, loopback included.
/// A browser cannot dial a wildcard, so the canvas is reached at `localhost` (a
/// container's port map, an `ssh -L`); a LAN address answers only itself.
pub(crate) fn answers_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0" | "::")
}

/// python `own_origins(rec)` — `http://<host>:<port>`, plus both loopback
/// spellings when it binds loopback (or a wildcard). NEVER derived from the request
/// Host header.
pub(crate) fn own_origins(rec: &Map<String, Value>) -> BTreeSet<String> {
    let host = effective_host(rec);
    let port = rec.get("port").map_or_else(|| "None".to_string(), py_str);
    let mut out = BTreeSet::from([format!("http://{host}:{port}")]);
    if answers_loopback(&host) {
        out.insert(format!("http://127.0.0.1:{port}"));
        out.insert(format!("http://localhost:{port}"));
        out.insert(format!("http://[::1]:{port}"));
    }
    out
}

/// python `own_authorities(rec)` — the Host values that ARE this web host:
/// `<host>:<port>` (an IPv6 literal in brackets), plus the three loopback
/// spellings when it binds loopback or a wildcard. Nothing else — a LAN name, a
/// tunnel name, another port are other authorities and are refused; a LAN bind
/// answers only its own `<host>:<port>`.
pub(crate) fn own_authorities(rec: &Map<String, Value>) -> BTreeSet<String> {
    let host = effective_host(rec);
    let port = rec.get("port").map_or_else(|| "None".to_string(), py_str);
    let own = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut out = BTreeSet::from([own]);
    if answers_loopback(&host) {
        out.insert(format!("127.0.0.1:{port}"));
        out.insert(format!("localhost:{port}"));
        out.insert(format!("[::1]:{port}"));
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

/// python `_OriginGuard` — two checks, `rec` read per request by the caller:
/// - HOST: EVERY request — GET/HEAD/OPTIONS, unsafe methods and the
///   websocket upgrade alike — whose `Host` header is not one of
///   `own_authorities` is refused. A DNS-rebinding page sends a same-origin GET
///   with no `Origin`, so the Origin check never sees it; its `Host` still names
///   the attacker's domain.
/// - ORIGIN: a websocket upgrade, or an HTTP request that is not
///   GET/HEAD/OPTIONS, from a FOREIGN browser origin is refused.
///
/// Either refusal is 403 with a coded body on HTTP, a refused upgrade on a
/// websocket, and logged. `None` ⇒ pass through.
pub(crate) fn guard(
    web_agent_id: &str,
    rec: &Map<String, Value>,
    req: &Request<Body>,
) -> Option<Response> {
    let websocket = routes::is_websocket(req);
    let kind = if websocket { "websocket" } else { "http" };
    let host = req
        .headers()
        .get("host")
        .map(|v| routes::latin1(v.as_bytes()))
        .unwrap_or_default();
    if !own_authorities(rec).contains(&host) {
        let path = routes::percent_decode(req.uri().path());
        println!(
            "  [web] {web_agent_id}: refused {kind} {path} for foreign host {}",
            py_repr(&host)
        );
        return Some(refuse(
            websocket,
            format!("host {} is not this server's own authority", py_repr(&host)),
            "address the host by its bound host:port (a loopback bind also answers \
localhost, 127.0.0.1 and [::1] on that port)",
        ));
    }
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
    let path = routes::percent_decode(req.uri().path());
    println!(
        "  [web] {web_agent_id}: refused {kind} {path} from foreign origin {}",
        py_repr(&origin)
    );
    Some(refuse(
        websocket,
        format!("origin {} is not allowed on this host", py_repr(&origin)),
        "add it to the web record's allowed_origins",
    ))
}

/// python `_OriginGuard._refuse` — a websocket: uvicorn's answer to a
/// `websocket.close` before accept (403, empty body); HTTP: 403 with the coded body.
fn refuse(websocket: bool, error: String, hint: &str) -> Response {
    if websocket {
        return routes::respond(StatusCode::FORBIDDEN, &[], None, Vec::new());
    }
    let body = py_json_dumps(
        &json!({"error": error, "reason": "unauthorized", "hint": hint}),
        true,
        None,
    );
    routes::respond(
        StatusCode::FORBIDDEN,
        &[],
        Some("application/json"),
        body.into_bytes(),
    )
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
        // a wildcard bind answers its own origin AND the loopback spellings
        assert_eq!(
            own_origins(&rec(json!({"host": "0.0.0.0", "port": 8088}))),
            BTreeSet::from([
                "http://0.0.0.0:8088".to_string(),
                "http://127.0.0.1:8088".to_string(),
                "http://localhost:8088".to_string(),
                "http://[::1]:8088".to_string(),
            ])
        );
        // a LAN address answers only itself
        assert_eq!(
            own_origins(&rec(json!({"host": "192.168.1.5", "port": 8088}))),
            BTreeSet::from(["http://192.168.1.5:8088".to_string()])
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
    fn own_authorities_are_the_bound_host_port_plus_loopback_spellings() {
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        let loopback = set(&["127.0.0.1:8088", "localhost:8088", "[::1]:8088"]);
        assert_eq!(own_authorities(&rec(json!({"port": 8088}))), loopback);
        assert_eq!(
            own_authorities(&rec(json!({"host": "localhost", "port": 8088}))),
            loopback
        );
        // a wildcard bind answers its own host:port and the loopback spellings
        let wildcard = set(&["0.0.0.0:8088", "127.0.0.1:8088", "localhost:8088", "[::1]:8088"]);
        assert_eq!(own_authorities(&rec(json!({"host": "0.0.0.0", "port": 8088}))), wildcard);
        // a LAN bind answers its own host:port and nothing else
        assert_eq!(
            own_authorities(&rec(json!({"host": "192.168.1.5", "port": 8088}))),
            set(&["192.168.1.5:8088"])
        );
        // an IPv6 wildcard is bracketed, as a Host header spells it, and answers loopback
        assert_eq!(
            own_authorities(&rec(json!({"host": "::", "port": 8088}))),
            set(&["[::]:8088", "127.0.0.1:8088", "localhost:8088", "[::1]:8088"])
        );
        // no Host header at all is not an authority
        assert!(!own_authorities(&rec(json!({"port": 8088}))).contains(""));
    }

    /// The guard on a bare request: `(status, body)` when refused, `None` passes.
    fn judge(r: &Map<String, Value>, method: &str, host: Option<&str>) -> Option<u16> {
        let mut b = Request::builder().method(method).uri("/x");
        if let Some(h) = host {
            b = b.header("host", h);
        }
        guard("web", r, &b.body(Body::empty()).unwrap()).map(|resp| resp.status().as_u16())
    }

    /// THE REBINDING READ: a same-origin GET carries no Origin, so only
    /// the Host names the attacker's domain — every method is judged on it.
    #[test]
    fn a_foreign_host_is_refused_on_every_method_with_no_origin() {
        let r = rec(json!({"port": 8088}));
        for method in ["GET", "HEAD", "OPTIONS", "POST"] {
            assert_eq!(
                judge(&r, method, Some("rebind.invalid:8088")),
                Some(403),
                "{method}"
            );
        }
        assert_eq!(judge(&r, "GET", None), Some(403), "no Host at all");
        for host in ["127.0.0.1:8088", "localhost:8088", "[::1]:8088"] {
            assert_eq!(judge(&r, "GET", Some(host)), None, "{host}");
        }
        for host in ["127.0.0.1:9999", "localhost", "127.0.0.1"] {
            assert_eq!(judge(&r, "GET", Some(host)), Some(403), "{host}");
        }
    }

    /// `0.0.0.0` publishes the port (a container's port map). It answers its own
    /// `0.0.0.0:<port>` and the loopback spellings on that port, so `localhost:<port>`
    /// works from the browser; a LAN name, a LAN address, another port are refused.
    #[test]
    fn a_published_bind_admits_its_own_host_port_and_loopback_on_it() {
        let r = rec(json!({"host": "0.0.0.0", "port": 8088}));
        for host in ["0.0.0.0:8088", "localhost:8088", "127.0.0.1:8088", "[::1]:8088"] {
            assert_eq!(judge(&r, "GET", Some(host)), None, "{host}");
        }
        for host in [
            "localhost:9999",
            "127.0.0.1:52999",
            "192.168.1.5:8088",
            "box.lan:8088",
        ] {
            assert_eq!(judge(&r, "GET", Some(host)), Some(403), "{host}");
        }
    }

    /// A browser at `localhost:<port>` on a wildcard bind is an own origin, so its
    /// websocket upgrade (a gated request) passes; a foreign origin still does not.
    #[test]
    fn a_wildcard_bind_admits_a_localhost_browser_origin_on_websockets() {
        let r = rec(json!({"host": "0.0.0.0", "port": 8088}));
        assert!(origin_allowed(&r, Some("http://localhost:8088")));
        assert!(origin_allowed(&r, Some("http://127.0.0.1:8088")));
        assert!(!origin_allowed(&r, Some("http://localhost:52999")));
        assert!(!origin_allowed(&r, Some("http://192.168.1.5:8088")));
        // a LAN bind keeps its own origin only
        let lan = rec(json!({"host": "192.168.1.5", "port": 8088}));
        assert!(origin_allowed(&lan, Some("http://192.168.1.5:8088")));
        assert!(!origin_allowed(&lan, Some("http://localhost:8088")));
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
