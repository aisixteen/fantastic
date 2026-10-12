//! The ordered route table and starlette's matching rules.
//!
//! python mounts FOUR baked routes on one `FastAPI` app and then APPENDS each
//! child surface's `get_routes` specs (`web/tools.py::_mount_surface`). Every
//! request walks `app.routes` IN ORDER: the first path+method match handles it,
//! the first path-only match answers `405` with that route's `Allow`, and when
//! neither hits, starlette toggles the trailing slash and answers `307` if the
//! toggled path matches anything (`starlette/routing.py::Router.app`). A miss
//! is FastAPI's `{"detail":"Not Found"}`.
//!
//! axum's `Router` cannot express that: it matches per-path with its own
//! `MethodRouter` shadowing, has no ordered fallback and no slash toggle, and
//! caps request bodies at 2 MiB. So the host owns its matcher — one ordered
//! `Vec<Route>`, walked exactly as starlette walks it.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use fantastic_kernel::AgentId;

/// One path segment of a compiled route pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    /// A literal segment; compares exactly.
    Lit(String),
    /// `{name}` — starlette's `str` convertor, `[^/]+`.
    Param,
    /// `{name:path}` — starlette's `path` convertor, `.*`, always last.
    Rest,
}

/// What a matched route runs.
#[derive(Clone, Debug)]
pub(crate) enum Handler {
    /// `GET /_assets/favicon.png`.
    Favicon,
    /// `GET /{agent_id}/file/{path:path}` — the gated read_stream proxy.
    FileProxy,
    /// `POST /<rest_id>/{target_id}` — the REST verb channel.
    RestPost(AgentId),
    /// `GET /<rest_id>/_reflect`.
    RestReflectRoot(AgentId),
    /// `GET /<rest_id>/_reflect/{target_id}`.
    RestReflectTarget(AgentId),
    /// `WS /{host_id}/ws`, carrying the web_ws LEG that declared it — the
    /// record the frame gate reads and the identity every frame runs as (C40).
    Ws(AgentId),
}

/// One mounted route, in mount order.
#[derive(Clone, Debug)]
pub(crate) struct Route {
    /// `Some(method)` for an http route, `None` for a websocket route.
    /// starlette matches http routes only on an http scope and websocket
    /// routes only on a websocket scope, so the two never shadow each other.
    pub method: Option<Method>,
    /// The mounted path, python spelling (`/{host_id}/ws`).
    pub path: String,
    segs: Vec<Seg>,
    pub handler: Handler,
}

impl Route {
    pub(crate) fn http(method: Method, path: impl Into<String>, handler: Handler) -> Self {
        let path = path.into();
        Self {
            method: Some(method),
            segs: compile(&path),
            path,
            handler,
        }
    }

    pub(crate) fn websocket(path: impl Into<String>, handler: Handler) -> Self {
        let path = path.into();
        Self {
            method: None,
            segs: compile(&path),
            path,
            handler,
        }
    }

    /// Does the (already percent-decoded) path match this pattern? Returns the
    /// captured parameters in declaration order.
    pub(crate) fn captures(&self, path: &str) -> Option<Vec<String>> {
        match_segs(&self.segs, path)
    }
}

/// Compile a python/starlette path into segments. `{name}` → [`Seg::Param`],
/// `{name:path}` → [`Seg::Rest`], everything else a literal.
fn compile(path: &str) -> Vec<Seg> {
    path.split('/')
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                let inner = &s[1..s.len() - 1];
                if inner.split_once(':').map(|(_, k)| k) == Some("path") {
                    Seg::Rest
                } else {
                    Seg::Param
                }
            } else {
                Seg::Lit(s.to_string())
            }
        })
        .collect()
}

fn match_segs(segs: &[Seg], path: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut caps = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        match seg {
            Seg::Rest => {
                // `.*` after the preceding `/`: the separator must be present,
                // so `/x/file` does not match `/{id}/file/{path:path}` while
                // `/x/file/` does (with an empty capture).
                if parts.len() < i + 1 {
                    return None;
                }
                caps.push(parts[i..].join("/"));
                return Some(caps);
            }
            Seg::Param => {
                // `[^/]+` — at least one character.
                let p = parts.get(i)?;
                if p.is_empty() {
                    return None;
                }
                caps.push((*p).to_string());
            }
            Seg::Lit(lit) => {
                if parts.get(i) != Some(&lit.as_str()) {
                    return None;
                }
            }
        }
    }
    if parts.len() == segs.len() {
        Some(caps)
    } else {
        None
    }
}

/// The live ordered route table: python's four baked routes, then each mounted
/// child's specs in mount order (`web/tools.py` keeps `routes_by_child` beside
/// the one `app.routes` list; unmount removes that child's entries) — then
/// ORDERED, a literal path before a pattern (see [`RouteTable::order`]).
#[derive(Default)]
pub(crate) struct RouteTable {
    base: Vec<Route>,
    children: Vec<(AgentId, Vec<Route>)>,
    /// What resolution walks: `base` + `children`, ordered. Python's
    /// `app.router.routes` after `_order_routes`.
    live: Vec<Route>,
}

impl RouteTable {
    /// python `make_app` — the baked routes, in FastAPI's declaration order. No
    /// `/`: the host renders nothing of its own, so an undeclared `/` is a 404.
    pub(crate) fn base() -> Self {
        let mut t = Self {
            base: vec![
                Route::http(Method::GET, "/_assets/favicon.png", Handler::Favicon),
                Route::http(
                    Method::GET,
                    "/{agent_id}/file/{path:path}",
                    Handler::FileProxy,
                ),
            ],
            children: Vec::new(),
            live: Vec::new(),
        };
        t.order();
        t
    }

    /// Every route, in the order resolution walks them (first match wins).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Route> {
        self.live.iter()
    }

    /// A LITERAL PATH BEATS A PATTERN — python `_order_routes`. Routing is
    /// first-match-wins in list order, and legs append their routes in MOUNT
    /// order, which on a hydrated host is the children's folder order. A `web_ws`
    /// leg declares the catch-all `/{host_id}/ws`; mounted before a leg that
    /// declares the literal `/web_ui/ws`, it swallowed it. So the live table is
    /// ordered by how many PARAMETERS a path has (python counts its `{`) — fewer
    /// first; equal counts keep their mount order (`sort_by_key` is stable).
    fn order(&mut self) {
        let mut all: Vec<Route> = self
            .base
            .iter()
            .chain(self.children.iter().flat_map(|(_, r)| r.iter()))
            .cloned()
            .collect();
        all.sort_by_key(|r| r.path.matches('{').count());
        self.live = all;
    }

    /// The paths currently mounted for one child (python's
    /// `handle.routes_by_child[child_id]`).
    pub(crate) fn paths_for(&self, child: &AgentId) -> Vec<String> {
        self.children
            .iter()
            .find(|(c, _)| c == child)
            .map(|(_, r)| r.iter().map(|x| x.path.clone()).collect())
            .unwrap_or_default()
    }

    /// The mounted child ids, in mount order (python's
    /// `list(handle.routes_by_child.keys())`).
    pub(crate) fn child_ids(&self) -> Vec<String> {
        self.children.iter().map(|(c, _)| c.0.clone()).collect()
    }

    /// Replace one child's routes; an empty `routes` mounts nothing (python
    /// only records a child that returned at least one mountable spec).
    pub(crate) fn mount(&mut self, child: AgentId, routes: Vec<Route>) {
        self.children.retain(|(c, _)| *c != child);
        if !routes.is_empty() {
            self.children.push((child, routes));
        }
        self.order();
    }

    /// python `_unmount_surface` — drop every route belonging to a child.
    pub(crate) fn unmount(&mut self, child: &AgentId) {
        self.children.retain(|(c, _)| c != child);
        self.order();
    }
}

/// One route resolution, starlette's outcome set.
pub(crate) enum Resolved<'a> {
    /// `Match.FULL` — this route handles the request.
    Full(&'a Route, Vec<String>),
    /// `Match.PARTIAL` — the path matched, the method did not: `405` + `Allow`.
    MethodNotAllowed(&'a Method),
    /// Neither: the caller tries the trailing-slash toggle, then `404`.
    None,
}

/// starlette `Router.app`: first FULL wins, else the FIRST partial, else none.
pub(crate) fn resolve<'a>(
    table: &'a RouteTable,
    path: &str,
    method: &Method,
    websocket: bool,
) -> Resolved<'a> {
    let mut partial: Option<&Method> = None;
    for route in table.iter() {
        // An http route never matches a websocket scope and vice versa.
        let Some(route_method) = route.method.as_ref() else {
            if websocket {
                if let Some(caps) = route.captures(path) {
                    return Resolved::Full(route, caps);
                }
            }
            continue;
        };
        if websocket {
            continue;
        }
        let Some(caps) = route.captures(path) else {
            continue;
        };
        if route_method == method {
            return Resolved::Full(route, caps);
        }
        if partial.is_none() {
            partial = Some(route_method);
        }
    }
    match partial {
        Some(m) => Resolved::MethodNotAllowed(m),
        None => Resolved::None,
    }
}

/// Does ANY route match the toggled path (starlette checks `!= Match.NONE`,
/// so a method mismatch still redirects)?
pub(crate) fn any_match(table: &RouteTable, path: &str, websocket: bool) -> bool {
    table
        .iter()
        .any(|r| r.method.is_some() != websocket && r.captures(path).is_some())
}

// ─── responses ──────────────────────────────────────────────────────
//
// uvicorn writes `date` then its `encoded_headers` (`server: uvicorn`) ahead of
// the response's own (`server.py:248`, `config.py:429`), and starlette's
// `init_headers` emits the handler's explicit headers, THEN `content-length`,
// THEN `content-type` (`responses.py:55-81`). `http::HeaderMap` iterates in
// insertion order, so building the map in that order puts the wire block in
// python's order.

/// RFC 7231 IMF-fixdate, uvicorn's `date` value.
pub(crate) fn http_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let wd = WD[days.rem_euclid(7) as usize];
    format!(
        "{wd}, {d:02} {mon} {y:04} {h:02}:{mi:02}:{s:02} GMT",
        mon = MO[(m - 1) as usize]
    )
}

/// Build a response with python's header block: `date`, `server`, the
/// handler's own headers, `content-length`, `content-type`.
pub(crate) fn respond(
    status: StatusCode,
    extras: &[(HeaderName, String)],
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&http_date()) {
        headers.insert(header::DATE, v);
    }
    headers.insert(header::SERVER, HeaderValue::from_static("uvicorn"));
    for (name, value) in extras {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(name.clone(), v);
        }
    }
    // starlette skips content-length on 1xx/204/304 exactly as here.
    let sizeless =
        status.as_u16() < 200 || status == StatusCode::NO_CONTENT || status.as_u16() == 304;
    if !sizeless {
        if let Ok(v) = HeaderValue::from_str(&body.len().to_string()) {
            headers.insert(header::CONTENT_LENGTH, v);
        }
    }
    if let Some(ct) = content_type {
        if let Ok(v) = HeaderValue::from_str(ct) {
            headers.insert(header::CONTENT_TYPE, v);
        }
    }
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

/// A streaming response (the file proxy): same header block, no `content-length`
/// unless the size is known.
pub(crate) fn respond_stream(
    content_type: &str,
    content_length: Option<u64>,
    body: Body,
) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&http_date()) {
        headers.insert(header::DATE, v);
    }
    headers.insert(header::SERVER, HeaderValue::from_static("uvicorn"));
    if let Some(n) = content_length {
        if let Ok(v) = HeaderValue::from_str(&n.to_string()) {
            headers.insert(header::CONTENT_LENGTH, v);
        }
    }
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    let mut resp = Response::new(body);
    *resp.headers_mut() = headers;
    resp
}

/// FastAPI's `http_exception_handler` body for a status with no route.
pub(crate) fn not_found() -> Response {
    respond(
        StatusCode::NOT_FOUND,
        &[],
        Some("application/json"),
        br#"{"detail":"Not Found"}"#.to_vec(),
    )
}

/// starlette's `Route.handle` 405 → FastAPI's JSON body + the route's `Allow`.
pub(crate) fn method_not_allowed(allow: &Method) -> Response {
    respond(
        StatusCode::METHOD_NOT_ALLOWED,
        &[(header::ALLOW, allow.as_str().to_string())],
        Some("application/json"),
        br#"{"detail":"Method Not Allowed"}"#.to_vec(),
    )
}

/// starlette's `redirect_slashes` answer: `307` to the toggled path, the
/// `Host` header carried through, `content-length: 0`.
pub(crate) fn redirect_slash(headers: &HeaderMap, toggled: &str, query: Option<&str>) -> Response {
    let host = headers
        .get(header::HOST)
        .map(|v| latin1(v.as_bytes()))
        .unwrap_or_default();
    let mut url = if host.is_empty() {
        toggled.to_string()
    } else {
        format!("http://{host}{toggled}")
    };
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    respond(
        StatusCode::TEMPORARY_REDIRECT,
        &[(header::LOCATION, quote_location(&url))],
        None,
        Vec::new(),
    )
}

/// starlette's `RedirectResponse` quoting (`responses.py:204-213`).
fn quote_location(url: &str) -> String {
    const SAFE: &str = ":/%#?=@[]!$&'()*+,;";
    let mut out = String::with_capacity(url.len());
    for b in url.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || "_.-~".contains(c) || SAFE.contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// starlette decodes every request header value as latin-1
/// (`datastructures.py:546-550`) — byte for byte, never UTF-8.
pub(crate) fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// uvicorn percent-decodes the request path ONCE before the ASGI scope
/// (`h11_impl.py:201`); the query string is never part of the match.
pub(crate) fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// starlette `request.query_params.get(key)` — `parse_qsl(qs,
/// keep_blank_values=True)` splitting on `&` ONLY (`;` is part of a value), `+`
/// as space, `%XX` decoded, and the LAST value for a repeated key (the multidict's
/// `get`).
pub(crate) fn query_get(query: &str, key: &str) -> Option<String> {
    let mut found = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(&k.replace('+', " ")) == key {
            found = Some(percent_decode(&v.replace('+', " ")));
        }
    }
    found
}

/// Is this request an ASGI `websocket` scope? uvicorn hands h11 the upgrade
/// only for a `GET` carrying `Upgrade: websocket`.
pub(crate) fn is_websocket(req: &Request<Body>) -> bool {
    req.method() == Method::GET
        && req
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The percent-decoded path plus the raw query string.
pub(crate) fn split_target(req: &Request<Body>) -> (String, String) {
    (
        percent_decode(req.uri().path()),
        req.uri().query().unwrap_or("").to_string(),
    )
}

/// The `_reflect` payload python composes from the query flags
/// (`web_rest/tools.py::_reflect_payload`).
pub(crate) fn reflect_payload(query: &str) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("type".into(), serde_json::json!("reflect"));
    if let Some(rm) = query_get(query, "readme") {
        if matches!(rm.as_str(), "1" | "true" | "yes") {
            out.insert("readme".into(), serde_json::json!(true));
        } else if !rm.is_empty() && !matches!(rm.as_str(), "0" | "false" | "no") {
            out.insert("readme".into(), serde_json::json!(rm));
        }
    }
    for key in ["tree", "bundles"] {
        if let Some(v) = query_get(query, key) {
            if matches!(v.as_str(), "all" | "ids" | "none") {
                out.insert(key.into(), serde_json::json!(v));
            }
        }
    }
    serde_json::Value::Object(out)
}

/// Build the routes one child's `get_routes` reply declares. python mounts by
/// `(kind, method, path)`; a spec without a usable path is skipped exactly as
/// `_mount_surface` skips one without a callable endpoint.
pub(crate) fn routes_from_spec(child: &AgentId, reply: &serde_json::Value) -> Vec<Route> {
    let Some(specs) = reply.get("routes").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for spec in specs {
        let kind = spec.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let Some(path) = spec.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        if kind == "websocket" {
            out.push(Route::websocket(path, Handler::Ws(child.clone())));
            continue;
        }
        let method = spec
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let Ok(method) = Method::from_bytes(method.as_bytes()) else {
            continue;
        };
        let handler = if method == Method::POST {
            Handler::RestPost(child.clone())
        } else if path.ends_with("/_reflect") {
            Handler::RestReflectRoot(child.clone())
        } else if path.contains("/_reflect/") {
            Handler::RestReflectTarget(child.clone())
        } else {
            // python skips an unmountable route SILENTLY (`web/tools.py:66-81`).
            continue;
        };
        out.push(Route::http(method, path, handler));
    }
    out
}
