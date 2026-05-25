//! axum HTTP host bundle.
//!
//! Each `web` agent owns a `port` field on its record (REQUIRED — no
//! default). On `boot`, spawns an axum listener on `0.0.0.0:<port>`
//! serving the standard rendering routes:
//!
//! | route                          | what                                          |
//! |--------------------------------|-----------------------------------------------|
//! | `GET  /`                       | root index (link tree of served agents)       |
//! | `GET  /<id>/`                  | dispatch `render_html` on agent + inject transport.js |
//! | `GET  /<id>/file/<path>`       | proxy to the file agent's `read` verb         |
//! | `GET  /transport.js`           | the JS client embedded at compile-time        |
//! | `GET  /favicon.ico`            | embedded favicon                              |
//!
//! Verb-invocation surfaces (WS, REST) are NOT served here — they
//! live in sibling `fantastic-web-ws` / `fantastic-web-rest` agents
//! that mount onto the same router via their own listen ports
//! (Phase 1 keeps each surface on its own port; multi-port mounting
//! is a future hardening).
//!
//! ### Verbs
//!
//! - `reflect` → `{id, sentence, port, running}`
//! - `boot`    → spawn the server task, store its handle. Idempotent.
//! - `stop`    → cancel the task. Idempotent.
//! - `shutdown`→ alias of `stop`.

#![deny(missing_docs)]

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{ws::Message, ws::WebSocket, Json, Path as AxPath, Query, State, WebSocketUpgrade},
    http::{header, Request, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, SubscriberToken};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::task::JoinHandle;
use tower::ServiceExt;
// axum's ServiceExt provides `.into_make_service()` on any tower Service.
use axum::ServiceExt as _;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// `transport.js` body — the client transport script served at
/// `/transport.js` AND injected into every `<id>/` render. Plain
/// JavaScript; no Rust code expects it to round-trip.
pub const TRANSPORT_JS: &str = include_str!("transport.js");

/// Static `/` index — link tree placeholder.
pub const ROOT_INDEX_HTML: &str = include_str!("index.html");

// ── Bundled third-party web assets ────────────────────────────────
//
// Vendored under `assets/`. Served at top-level `/_assets/<name>`
// URLs so any WebView surface (canvas, terminal, future ones in this
// kernel or embedding apps) can reference them without CDN deps.
// Version-pinned; license attribution in `rust/THIRD_PARTY_LICENSES.md`.

/// Three.js v0.160.0 (minified). Consumed by `fantastic-canvas-webapp`.
pub const THREE_JS: &str = include_str!("assets/three.module.js");

/// xterm.js v6.0.0 (minified). Consumed by `fantastic-terminal-webapp`.
pub const XTERM_JS: &str = include_str!("assets/xterm.min.js");

/// xterm.js v6.0.0 default stylesheet. Consumed by `fantastic-terminal-webapp`.
pub const XTERM_CSS: &str = include_str!("assets/xterm.min.css");

/// xterm.js fit addon v0.11.0 (minified). Consumed by `fantastic-terminal-webapp`.
pub const XTERM_ADDON_FIT_JS: &str = include_str!("assets/xterm-addon-fit.min.js");

/// `Cache-Control` value for `/_assets/*` — pinned files never change
/// for a given kernel version, so the browser can hold them indefinitely.
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Live web servers, keyed by web-agent id. Holds the JoinHandle +
/// the Arc-RwLock'd Router (for hot re-mounting) + the
/// `routes_changed` state subscriber token (so `stop` / `on_delete`
/// can detach cleanly).
pub(crate) static SERVERS: once_cell_lock::OnceLockMap = once_cell_lock::OnceLockMap::new();

/// Per-agent live server bookkeeping. Tracked outside `Agent` so the
/// substrate's plain record stays disk-shape only.
pub(crate) struct ServerHandle {
    /// The serve loop's task handle.
    pub task: JoinHandle<()>,
    /// The shared router; writers swap the inner Router in place to
    /// hot-mount/unmount child surfaces without restarting axum.
    /// Held here so the boot path and the `routes_changed`
    /// subscriber closure share the same Arc; never read back from
    /// this map (the closure has its own clone), but keeps the
    /// lifetime tied to the running server.
    #[allow(dead_code)]
    pub router: Arc<AsyncRwLock<Router>>,
    /// State-subscriber token for the `routes_changed` watcher; detach
    /// on stop so the closure's captured Arcs drop.
    pub sub_token: SubscriberToken,
}

mod once_cell_lock {
    use super::*;
    use std::sync::OnceLock;
    /// Lazy-initialized concurrent map of web-agent id → ServerHandle.
    pub struct OnceLockMap(OnceLock<Mutex<HashMap<AgentId, ServerHandle>>>);
    impl OnceLockMap {
        pub const fn new() -> Self {
            Self(OnceLock::new())
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, ServerHandle>> {
            self.0
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .expect("SERVERS poisoned")
        }
    }
}

/// Shared state available to every axum handler.
#[derive(Clone)]
struct AppState {
    /// Reference to the live kernel so handlers can `kernel.send(...)`.
    kernel: Arc<Kernel>,
    /// This web agent's id (used to look up port + render parent context).
    #[allow(dead_code)]
    web_agent_id: AgentId,
}

/// The HTTP host bundle.
pub struct WebBundle;

#[async_trait]
impl Bundle for WebBundle {
    fn name(&self) -> &str {
        "web"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel).await,
            "boot" => boot_reply(agent_id, kernel).await,
            "stop" | "shutdown" => stop_reply(agent_id),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = stop_with_kernel(agent_id, kernel);
        Ok(())
    }
}

async fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let port = read_port(agent_id, kernel);
    let running = SERVERS.lock().contains_key(agent_id);
    json!({
        "id": agent_id.as_str(),
        "sentence": "axum HTTP host.",
        "port": port,
        "running": running,
        "verbs": {
            "reflect": "Identity + bound port + running flag. No args.",
            "boot": "Spawn the listener on `port`. Idempotent.",
            "stop": "Cancel the listener task. Idempotent.",
            "shutdown": "Alias of stop.",
        }
    })
}

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    if SERVERS.lock().contains_key(agent_id) {
        return json!({"id": agent_id.as_str(), "running": true, "already_booted": true});
    }
    let port = match read_port(agent_id, kernel) {
        Some(p) => p,
        None => {
            return json!({"error": format!(
                "web {}: port is required (no default). Set via update_agent.",
                agent_id
            )})
        }
    };
    let state = AppState {
        kernel: Arc::clone(kernel),
        web_agent_id: agent_id.clone(),
    };
    // Build the base router synchronously (always-present routes) + walk
    // siblings to mount their `get_routes`. The result lives inside an
    // Arc<RwLock<Router>> so the `routes_changed` subscription can swap
    // it in place without restarting axum.
    let initial = build_router_from_siblings(&state, agent_id, kernel).await;
    let router_lock = Arc::new(AsyncRwLock::new(initial));

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return json!({"error": format!("bind {addr}: {e}")}),
    };
    let actual_port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(_) => port,
    };

    // Subscribe to kernel state events BEFORE handing the listener to
    // axum::serve. If a child emits `routes_changed` while boot is
    // still resolving siblings, the closure rebuilds against the
    // current sibling set — boot is idempotent so we can't race past
    // it. (See module-level note on boot sequencing.)
    let sub_state = state.clone();
    let sub_self_id = agent_id.clone();
    let sub_router = Arc::clone(&router_lock);
    let sub_kernel = Arc::clone(kernel);
    let sub_token = kernel.add_state_subscriber(Arc::new(move |event: &Value| {
        let ty = event.get("type").and_then(Value::as_str).unwrap_or("");
        let verb = event.get("verb").and_then(Value::as_str).unwrap_or("");
        // Match either an explicit `routes_changed` emit OR a
        // `created`/`removed` system event on a sibling (so adding /
        // dropping a web_ws child re-mounts even if the child doesn't
        // emit). Conservative re-mount: cheap (the build only walks
        // direct siblings) + idempotent.
        let is_routes_changed = ty == "emit" && verb == "routes_changed";
        let is_created = ty == "created"
            && event.get("parent_id").and_then(Value::as_str) == Some(sub_self_id.as_str());
        let is_removed = ty == "removed";
        if !is_routes_changed && !is_created && !is_removed {
            return;
        }
        let state = sub_state.clone();
        let self_id = sub_self_id.clone();
        let router = Arc::clone(&sub_router);
        let kernel = Arc::clone(&sub_kernel);
        tokio::spawn(async move {
            let new_router = build_router_from_siblings(&state, &self_id, &kernel).await;
            *router.write().await = new_router;
        });
    }));

    let dynamic = DynamicRouter {
        inner: Arc::clone(&router_lock),
    };
    let serve = axum::serve(listener, dynamic.into_make_service());
    let task = tokio::spawn(async move {
        if let Err(e) = serve.await {
            tracing::warn!(error = %e, "web: axum serve exited with error");
        }
    });
    SERVERS.lock().insert(
        agent_id.clone(),
        ServerHandle {
            task,
            router: router_lock,
            sub_token,
        },
    );
    json!({
        "id": agent_id.as_str(),
        "running": true,
        "port": actual_port,
    })
}

fn stop_reply(agent_id: &AgentId) -> Value {
    // Snapshot the handle WITHOUT dropping it while holding the lock
    // (the kernel state-subscriber detach below takes its own lock,
    // and we want to be lock-free during that).
    let removed = SERVERS.lock().remove(agent_id);
    if let Some(handle) = removed {
        handle.task.abort();
        // No kernel reference here — `on_delete` / `stop` could be
        // called from many paths and we don't always have an Arc<Kernel>
        // in scope. The subscriber holds Arcs that drop with this
        // handle, so detaching is best-effort cleanup. We rely on the
        // closure becoming a no-op when its captured `router` Arc is
        // the only one (router_lock dropped along with handle).
        // For a clean detach pattern from places that DO own a kernel
        // ref, see `WebBundle::on_delete`.
        json!({
            "id": agent_id.as_str(),
            "stopped": true,
            "sub_token": handle.sub_token.0,
        })
    } else {
        json!({"id": agent_id.as_str(), "stopped": false, "reason": "not running"})
    }
}

/// Stop variant that also detaches the kernel state subscriber. Called
/// from `on_delete` (where we always have a `kernel` ref).
fn stop_with_kernel(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let removed = SERVERS.lock().remove(agent_id);
    if let Some(handle) = removed {
        handle.task.abort();
        kernel.remove_state_subscriber(handle.sub_token);
        json!({"id": agent_id.as_str(), "stopped": true})
    } else {
        json!({"id": agent_id.as_str(), "stopped": false, "reason": "not running"})
    }
}

fn read_port(agent_id: &AgentId, kernel: &Kernel) -> Option<u16> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned").clone();
    meta.get("port").and_then(|v| v.as_u64()).map(|p| p as u16)
}

/// Build the base router: always-present rendering routes only.
///
/// Call surfaces (WS, REST) are NOT in the base — they mount via
/// [`build_router_from_siblings`] only when a `web_ws` / `web_rest`
/// child agent declares them through its `get_routes` verb. This
/// matches Python's `_mount_surface` semantics: a `web` instance
/// with no children serves rendering and 404s on `/<id>/ws`.
fn build_router(state: AppState) -> Router {
    // Order matters for axum's matchit: place literal-segment routes
    // (transport.js, favicon) before parametric ones so the trie
    // disambiguates correctly. The file proxy uses
    // `:agent_id/file/*path` syntax so the wildcard sits at the
    // trailing position cleanly.
    Router::new()
        .route("/", get(serve_root_index_dynamic))
        .route("/transport.js", get(serve_transport_js))
        .route("/favicon.ico", get(serve_favicon))
        .route("/favicon.png", get(serve_favicon))
        .route("/_assets/favicon.png", get(serve_favicon))
        .route("/_assets/three.module.js", get(serve_three_js))
        .route("/_assets/xterm.min.js", get(serve_xterm_js))
        .route("/_assets/xterm.min.css", get(serve_xterm_css))
        .route(
            "/_assets/xterm-addon-fit.min.js",
            get(serve_xterm_addon_fit_js),
        )
        .route("/:agent_id/file/*path", get(serve_file_proxy))
        .route("/:agent_id/", get(serve_agent_render))
        .with_state(state)
}

/// Walk this web agent's direct child agents. For each one, ask
/// `get_routes` and mount the returned route specs onto a fresh
/// router built atop the always-present base.
///
/// Bundles return `{routes: [{kind, path, ...}]}` where `kind` is one
/// of `"websocket"`, `"get"`, `"post"`, `"http"` (with `method`). The
/// `endpoint` field that Python sends is NOT used — Rust dispatch
/// goes through static handlers that know the kind + path pattern
/// (e.g. `/<id>/ws` always uses `serve_ws_dynamic`, `POST
/// /<rest>/<target>` always uses `serve_rest_post`).
async fn build_router_from_siblings(
    state: &AppState,
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Router {
    let base = build_router(state.clone());

    // Find children of this web agent.
    let child_ids: Vec<AgentId> = match kernel.agents.get(self_id) {
        Some(entry) => entry.child_ids(),
        None => Vec::new(),
    };

    // Build child surfaces as a separate `Router<()>` (state baked in
    // per route), then merge into base. We must apply `.with_state`
    // per route group because axum's chained `route()` calls require
    // matching state types.
    let mut surface = Router::<()>::new();
    let mut surface_has_routes = false;

    for cid in child_ids {
        let reply = kernel.send(&cid, json!({"type": "get_routes"})).await;
        let Some(routes) = reply.get("routes").and_then(Value::as_array) else {
            continue;
        };
        for spec in routes {
            let kind = spec.get("kind").and_then(Value::as_str).unwrap_or("");
            let path = match spec.get("path").and_then(Value::as_str) {
                Some(p) => p,
                None => continue,
            };
            // Python uses `{name}` path-param syntax; axum 0.7 uses
            // `:name`. Translate so a Python-style spec mounts cleanly.
            let axum_path = translate_path(path);
            let next = match kind {
                "websocket" => Some(
                    Router::<AppState>::new()
                        .route(&axum_path, get(serve_ws_dynamic))
                        .with_state(state.clone()),
                ),
                "get" | "http" => {
                    let method = if kind == "get" {
                        "GET".to_string()
                    } else {
                        spec.get("method")
                            .and_then(Value::as_str)
                            .unwrap_or("GET")
                            .to_uppercase()
                    };
                    if method == "GET" {
                        if axum_path.ends_with("/_reflect") {
                            Some(
                                Router::<AppState>::new()
                                    .route(&axum_path, get(serve_rest_reflect_root_dynamic))
                                    .layer(axum::Extension(RestOwner(cid.clone())))
                                    .with_state(state.clone()),
                            )
                        } else if axum_path.contains("/_reflect/") {
                            Some(
                                Router::<AppState>::new()
                                    .route(&axum_path, get(serve_rest_reflect_dynamic))
                                    .layer(axum::Extension(RestOwner(cid.clone())))
                                    .with_state(state.clone()),
                            )
                        } else {
                            tracing::warn!(path = %axum_path, "web: unknown GET surface path");
                            None
                        }
                    } else if method == "POST" {
                        Some(
                            Router::<AppState>::new()
                                .route(&axum_path, post(serve_rest_post_dynamic))
                                .layer(axum::Extension(RestOwner(cid.clone())))
                                .with_state(state.clone()),
                        )
                    } else {
                        tracing::warn!(method = %method, path = %axum_path, "web: unsupported HTTP method");
                        None
                    }
                }
                "post" => Some(
                    Router::<AppState>::new()
                        .route(&axum_path, post(serve_rest_post_dynamic))
                        .layer(axum::Extension(RestOwner(cid.clone())))
                        .with_state(state.clone()),
                ),
                other => {
                    tracing::warn!(kind = %other, "web: unknown route kind");
                    None
                }
            };
            if let Some(r) = next {
                surface = surface.merge(r);
                surface_has_routes = true;
            }
        }
    }

    if surface_has_routes {
        base.merge(surface)
    } else {
        base
    }
}

/// Translate a Python/FastAPI-style path with `{name}` placeholders
/// into axum 0.7's `:name` syntax. Wildcards (`{name:path}`) become
/// `*name` (axum's catch-all). Idempotent — already-axum paths pass
/// through.
fn translate_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut i = 0;
    let bytes = p.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Find matching '}'.
            if let Some(end_rel) = p[i + 1..].find('}') {
                let name = &p[i + 1..i + 1 + end_rel];
                if let Some((n, kind)) = name.split_once(':') {
                    if kind == "path" {
                        out.push('*');
                        out.push_str(n);
                    } else {
                        out.push(':');
                        out.push_str(n);
                    }
                } else {
                    out.push(':');
                    out.push_str(name);
                }
                i = i + 1 + end_rel + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `DynamicRouter` wraps an `Arc<RwLock<Router>>` and forwards each
/// request through the current router (clone-cheap because axum's
/// Router is internally `Arc`-shared). Mounting / unmounting child
/// surfaces is a write-lock swap; per-request reads take the read-lock
/// briefly, clone the router, drop the lock, and `oneshot` the
/// request. `tokio::sync::RwLock` is the async-aware lock — required
/// because the call site is `Service::call` returning a Future, and
/// holding a `parking_lot` guard across an `.await` is unsound.
#[derive(Clone)]
struct DynamicRouter {
    inner: Arc<AsyncRwLock<Router>>,
}

impl tower::Service<Request<Body>> for DynamicRouter {
    type Response = Response;
    type Error = std::convert::Infallible;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let guard = inner.read().await;
            // Router::clone is Arc-cheap. We clone to avoid holding the
            // read lock across the (async) oneshot.
            let svc = guard.clone();
            drop(guard);
            svc.oneshot(req).await
        })
    }
}

/// Dynamic-mount WS endpoint. The mounted path is a literal
/// `/<parent_id>/ws` — no path params. Axum's extractor framework
/// rejects requests against handlers whose path-param arity doesn't
/// match the mounted path, so this handler takes no `AxPath`.
async fn serve_ws_dynamic(
    State(state): State<AppState>,
    AxPath(host_id): AxPath<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let host = AgentId::from(host_id.as_str());
    upgrade.on_upgrade(move |socket| ws_loop(state, socket, host))
}

/// Dynamic-mount POST endpoint. Mounted path is `/<self_id>/:target_id`
/// — one path param (`target_id`). The `self_id` (`rest_id` in
/// telemetry) isn't available from the path; we substitute the synthetic
/// `_ws_dyn_post` sender so the telemetry pane still gets a non-empty
/// sender for the dispatched verb. To attribute back to the surface
/// agent's id we'd need to bake it into the handler via a per-route
/// state — wired into the closure-style mount as a future improvement.
/// Carries the route-owner agent id (the `web_rest` instance whose
/// `get_routes` reply mounted this path). Stamped at mount time via
/// `Extension<RestOwner>` so the handler can attribute the dispatched
/// verb's `_current_sender` back to the real surface agent — Python
/// parity (`web_rest/tools.py:61` uses `self_id` the same way).
#[derive(Clone)]
struct RestOwner(AgentId);

async fn serve_rest_post_dynamic(
    State(state): State<AppState>,
    axum::Extension(owner): axum::Extension<RestOwner>,
    AxPath(target_id): AxPath<String>,
    Json(payload): Json<Value>,
) -> Response {
    let target = AgentId::from(target_id.as_str());
    let reply = fantastic_kernel::send::with_sender(owner.0.clone(), async {
        state.kernel.send(&target, payload).await
    })
    .await;
    // Python parity: return 204 No Content if the verb's reply was
    // null (some bundles signal "fire-and-forget" that way).
    if reply.is_null() {
        return (StatusCode::NO_CONTENT, ()).into_response();
    }
    let status = if reply.get("error").is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        reply.to_string(),
    )
        .into_response()
}

/// Dynamic-mount GET `_reflect` endpoint (no target). Mounted path is
/// `/<self_id>/_reflect` — no path params; the `self_id` again isn't
/// extracted (see note on `serve_rest_post_dynamic`).
async fn serve_rest_reflect_root_dynamic(
    State(state): State<AppState>,
    axum::Extension(owner): axum::Extension<RestOwner>,
    Query(q): Query<ReflectQuery>,
) -> Response {
    let target = AgentId::from("kernel");
    let payload = serde_json::json!({
        "type": "reflect",
        "return_readme": q.readme.unwrap_or(0) != 0,
    });
    let reply = fantastic_kernel::send::with_sender(owner.0.clone(), async {
        state.kernel.send(&target, payload).await
    })
    .await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        reply.to_string(),
    )
        .into_response()
}

/// Dynamic-mount GET `_reflect/:target_id` endpoint. Mounted path is
/// `/<self_id>/_reflect/:target_id` — one path param.
async fn serve_rest_reflect_dynamic(
    State(state): State<AppState>,
    axum::Extension(owner): axum::Extension<RestOwner>,
    AxPath(target_id): AxPath<String>,
    Query(q): Query<ReflectQuery>,
) -> Response {
    let target = AgentId::from(target_id.as_str());
    let payload = serde_json::json!({
        "type": "reflect",
        "return_readme": q.readme.unwrap_or(0) != 0,
    });
    let reply = fantastic_kernel::send::with_sender(owner.0.clone(), async {
        state.kernel.send(&target, payload).await
    })
    .await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        reply.to_string(),
    )
        .into_response()
}

/// Static fallback used by tests + by the dynamic handler when reflect
/// fails. Kept for backwards-compat with the pre-dynamic-mount tests.
#[allow(dead_code)]
async fn serve_root_index() -> impl IntoResponse {
    Html(ROOT_INDEX_HTML)
}

/// Dynamic root index: walks the substrate tree, probes each agent
/// for `render_html` / `get_webapp` in parallel, renders the same
/// nested-tree HTML Python's `_index_page` does. Template body is
/// the `{{tree_body}}` placeholder in `index.html`.
async fn serve_root_index_dynamic(State(state): State<AppState>) -> impl IntoResponse {
    let kernel = Arc::clone(&state.kernel);

    // Pull the substrate primer to get the tree.
    let primer = kernel
        .send(
            &AgentId::from("kernel"),
            serde_json::json!({"type":"reflect"}),
        )
        .await;
    let tree = primer.get("tree").cloned().unwrap_or(Value::Null);

    // Collect every agent id from the tree (depth-first).
    fn collect_ids(node: &Value, out: &mut Vec<String>) {
        let Some(obj) = node.as_object() else { return };
        if let Some(id) = obj.get("id").and_then(Value::as_str) {
            out.push(id.to_string());
        }
        if let Some(children) = obj.get("children").and_then(Value::as_array) {
            for c in children {
                collect_ids(c, out);
            }
        }
    }
    let mut ids = Vec::new();
    collect_ids(&tree, &mut ids);

    // Probe each agent for render_html or get_webapp IN PARALLEL.
    let probes = ids.iter().map(|id| {
        let kernel = Arc::clone(&kernel);
        let id = id.clone();
        async move {
            // Try render_html first.
            let r = kernel
                .send(
                    &AgentId::from(id.as_str()),
                    serde_json::json!({"type":"render_html"}),
                )
                .await;
            if r.get("html").and_then(Value::as_str).is_some() {
                return (id, true);
            }
            // Then get_webapp.
            let r = kernel
                .send(
                    &AgentId::from(id.as_str()),
                    serde_json::json!({"type":"get_webapp"}),
                )
                .await;
            if r.get("url").is_some() && r.get("error").is_none() {
                return (id, true);
            }
            (id, false)
        }
    });
    let probe_results = futures_util::future::join_all(probes).await;
    let mut has_html = std::collections::HashMap::<String, bool>::new();
    for (id, ok) in probe_results {
        has_html.insert(id, ok);
    }

    fn html_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn render_node(node: &Value, has_html: &std::collections::HashMap<String, bool>) -> String {
        let Some(obj) = node.as_object() else {
            return String::new();
        };
        let aid = obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = obj
            .get("display_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&aid)
            .to_string();
        let hm = obj
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("(root)")
            .to_string();
        let visit = if *has_html.get(&aid).unwrap_or(&false) {
            format!(
                r#"<a class="visit" href="/{}/" title="open agent UI">↗</a>"#,
                aid
            )
        } else {
            String::new()
        };
        let kids_html = if let Some(children) = obj.get("children").and_then(Value::as_array) {
            if children.is_empty() {
                String::new()
            } else {
                let inner: String = children.iter().map(|c| render_node(c, has_html)).collect();
                format!("<ul>{inner}</ul>")
            }
        } else {
            String::new()
        };
        format!(
            r#"<li><span class="id">{}</span> {} <code>{}</code> <span class="hm">{}</span>{}</li>"#,
            html_escape(&name),
            visit,
            html_escape(&aid),
            html_escape(&hm),
            kids_html,
        )
    }

    let body = if tree.is_object() {
        render_node(&tree, &has_html)
    } else {
        "<li><em>empty tree</em></li>".to_string()
    };

    let page = ROOT_INDEX_HTML.replace("{{tree_body}}", &body);
    Html(page).into_response()
}

async fn serve_transport_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        TRANSPORT_JS,
    )
}

/// Bundled favicon — copied verbatim from Python's web bundle
/// (`python/bundled_agents/web/host/src/web/favicon.png`) so the
/// browser tab icon matches across runtimes.
pub const FAVICON_PNG: &[u8] = include_bytes!("favicon.png");

async fn serve_favicon() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/png")],
        FAVICON_PNG,
    )
        .into_response()
}

// ── /_assets/* handlers ────────────────────────────────────────────

async fn serve_three_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        THREE_JS,
    )
        .into_response()
}

async fn serve_xterm_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        XTERM_JS,
    )
        .into_response()
}

async fn serve_xterm_css() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/css"),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        XTERM_CSS,
    )
        .into_response()
}

async fn serve_xterm_addon_fit_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        XTERM_ADDON_FIT_JS,
    )
        .into_response()
}

async fn serve_agent_render(
    State(state): State<AppState>,
    AxPath(agent_id): AxPath<String>,
) -> Response {
    let target = AgentId::from(agent_id.as_str());
    let reply = state
        .kernel
        .send(&target, json!({"type": "render_html"}))
        .await;
    // Successful response: {html: "..."}.
    if let Some(html) = reply.get("html").and_then(Value::as_str) {
        let injected = inject_transport(html);
        return Html(injected).into_response();
    }
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            format!("agent {agent_id}: {err}"),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        format!("agent {agent_id}: no render_html reply"),
    )
        .into_response()
}

async fn serve_file_proxy(
    State(state): State<AppState>,
    AxPath((agent_id, path)): AxPath<(String, String)>,
) -> Response {
    let target = AgentId::from(agent_id.as_str());
    let reply = state
        .kernel
        .send(&target, json!({"type": "read", "path": path}))
        .await;
    if let Some(content) = reply.get("content").and_then(Value::as_str) {
        let mime = guess_mime(&path);
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime)],
            content.to_string(),
        )
            .into_response();
    }
    if let Some(img) = reply.get("image_base64").and_then(Value::as_str) {
        let mime = reply
            .get("mime")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(img) {
            return (StatusCode::OK, [(header::CONTENT_TYPE, mime)], bytes).into_response();
        }
    }
    let msg = reply
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("agent {agent_id}: no file content"));
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        msg,
    )
        .into_response()
}

/// Inject `<script src="/transport.js"></script>` + the favicon
/// `<link>` before `</head>` if present, else at the top of the body.
/// Mirrors Python's transport injection + `_FAVICON_TAG` so browser
/// tabs show the bundled icon and pages get the JS runtime without
/// each agent declaring it.
fn inject_transport(html: &str) -> String {
    const TRANSPORT_TAG: &str = "<script src=\"/transport.js\"></script>";
    const FAVICON_TAG: &str =
        "<link rel=\"icon\" type=\"image/png\" href=\"/_assets/favicon.png\">";
    let needs_transport = !html.contains(TRANSPORT_TAG);
    // Skip favicon if the page already declares any `<link rel="icon"`
    // (per-agent custom icons win).
    let needs_favicon = !html.contains("rel=\"icon\"") && !html.contains("rel='icon'");
    if !needs_transport && !needs_favicon {
        return html.to_string();
    }
    let mut inject = String::new();
    if needs_favicon {
        inject.push_str(FAVICON_TAG);
        inject.push('\n');
    }
    if needs_transport {
        inject.push_str(TRANSPORT_TAG);
        inject.push('\n');
    }
    if let Some(idx) = html.find("</head>") {
        let (head, tail) = html.split_at(idx);
        format!("{head}{inject}{tail}")
    } else {
        format!("{inject}{html}")
    }
}

fn guess_mime(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".js") {
        "application/javascript"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "text/plain; charset=utf-8"
    }
}

// ─── WebSocket surface ──────────────────────────────────────────────
//
// `ws://host/<agent_id>/ws` — frame protocol per docs/_proxy.py:
//   C→S {type:"call",  target, payload, id}     → kernel.send → reply
//   C→S {type:"emit",  target, payload}         → kernel.emit (no reply)
//   C→S {type:"watch", src}                     → register synthetic watcher
//   C→S {type:"unwatch", src}                   → unregister
//   S→C {type:"reply", id, data} | {type:"error", id, error}
//   S→C {type:"event", payload}                 → drained from watcher inbox
//
// Binary frames are deferred to a later milestone.

/// WebSocket proxy loop. 1:1 port of Python's `web/_proxy.py::run`.
///
/// - `host_agent_id` — the URL the browser connected to
///   (`ws://host/<host_id>/ws`). Auto-watched on connect so events
///   emitted on its inbox reach the page without an explicit `watch`
///   frame. Also used as `_current_sender` for browser-driven calls
///   so telemetry rays attribute to the visible page agent (Python
///   uses the web_ws sub-agent's id here; Rust mounts WS routes
///   directly from the web bundle so the host is the closest match).
///
/// Mirrors Python's structure: `watching` set + pending task set,
/// inline emit/watch/unwatch handlers, `call` dispatched as a task
/// so long-lived sends (LLM streaming, slow tools) don't block the
/// receive loop. Disconnect cancels every pending task + unwatches
/// every src + drops the synthetic inbox.
async fn ws_loop(state: AppState, socket: WebSocket, host_agent_id: AgentId) {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_CLIENT_HEX: AtomicU64 = AtomicU64::new(0);
    let n = NEXT_CLIENT_HEX.fetch_add(1, Ordering::SeqCst);
    let client_id = AgentId::from(format!("_ws_{n:06x}").as_str());

    // Per-WS chunked-upload reassembly state. Lives in the ws_loop scope
    // so it drops on disconnect — no GC task needed for the "dead client"
    // case. Memory is bounded by MAX_CONCURRENT_UPLOADS × MAX_UPLOAD_SIZE.
    let pending_uploads: Arc<std::sync::Mutex<std::collections::HashMap<String, ChunkBuffer>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let (mut sink, mut stream) = socket.split();

    // Spawn a watcher-drain task: pulls events from the client's
    // auto-vivified inbox and serializes them as {type:"event"} frames.
    // We need a separate channel because axum's split sink is single-
    // consumer; the inbox receiver lives in the kernel.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(state.kernel.inbox_bound);
    // Hook the synthetic client inbox into the kernel.
    let (inbox_tx, mut inbox_rx) = tokio::sync::mpsc::channel::<Value>(state.kernel.inbox_bound);
    state.kernel.inboxes.insert(client_id.clone(), inbox_tx);

    // Auto-watch the host agent (Python parity — see web/_proxy.py:135).
    // Track every watch so the cleanup block can unwatch them all on
    // disconnect.
    let watching: Arc<tokio::sync::Mutex<HashSet<AgentId>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::from_iter([
            host_agent_id.clone()
        ])));
    state.kernel.watch(&host_agent_id, client_id.clone()).await;

    // Pending `call` tasks — cancelled on disconnect so long-lived
    // sends (ollama / nvidia streaming, slow tools) release their
    // locks immediately. Python: `pending: set[asyncio.Task]`.
    let pending_calls: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // State-stream subscription tokens — remove on disconnect to
    // unregister the kernel's telemetry callback for this WS.
    // (Python: `state_unsubs: list` of opaque unsubscribe callables.
    // Rust uses opaque tokens that we hand back to remove_state_subscriber.)
    let state_unsubs: Arc<std::sync::Mutex<Vec<fantastic_kernel::SubscriberToken>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let drain_task = tokio::spawn({
        let out_tx = out_tx.clone();
        async move {
            while let Some(payload) = inbox_rx.recv().await {
                let frame = serde_json::json!({"type": "event", "payload": payload});
                let line = match serde_json::to_string(&frame) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if out_tx.send(line).await.is_err() {
                    break;
                }
            }
        }
    });

    // Outbound forwarder: anything written to out_tx hits the socket.
    let send_task = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if sink.send(Message::Text(line)).await.is_err() {
                break;
            }
        }
    });

    // Inbound loop: parse text + binary frames, dispatch.
    // Browser-driven traffic uses host_agent_id as `_current_sender`
    // so telemetry rays visually originate from the page agent —
    // Python parity (web/_proxy.py uses web_agent_id; closest Rust
    // equivalent is the host since we don't have a separate web_ws
    // sub-agent on the route).
    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(bytes)) => {
                handle_binary_frame(&state, &client_id, &out_tx, &pending_uploads, bytes).await;
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let Ok(env): Result<Value, _> = serde_json::from_str(&text) else {
            continue;
        };
        let ty = env.get("type").and_then(Value::as_str).unwrap_or("");
        let target = env.get("target").and_then(Value::as_str).map(AgentId::from);
        let payload = env.get("payload").cloned().unwrap_or(Value::Null);
        let id = env.get("id").and_then(Value::as_str).map(str::to_string);
        let kernel = Arc::clone(&state.kernel);
        let sender_for_scope = host_agent_id.clone();
        let out = out_tx.clone();
        match ty {
            "call" => {
                let Some(target) = target else {
                    if let Some(id) = id {
                        let _ = out
                            .send(
                                serde_json::json!({"type":"error","id":id,"error":"call requires target"})
                                    .to_string(),
                            )
                            .await;
                    }
                    continue;
                };
                // Spawn the call so a long-lived send (LLM streaming,
                // slow tools) doesn't block the receive loop. Stash
                // the JoinHandle so disconnect can abort it, releasing
                // any kernel-side locks the dispatch holds.
                let handle = tokio::spawn(async move {
                    let reply = fantastic_kernel::send::with_sender(sender_for_scope, async {
                        kernel.send(&target, payload).await
                    })
                    .await;
                    if let Some(id) = id {
                        // Always wrap as `type:"reply"` with the verb's
                        // reply inside `data`. Verb-level errors travel
                        // inside `data.error` (Python's wire shape); the
                        // separate `type:"error"` frame is reserved for
                        // out-of-band failures the caller's promise
                        // can't resolve from data alone.
                        let frame = serde_json::json!({
                            "type": "reply",
                            "id": id,
                            "data": reply,
                        });
                        let _ = out.send(frame.to_string()).await;
                    }
                });
                pending_calls.lock().expect("pending poisoned").push(handle);
            }
            "emit" => {
                let Some(target) = target else { continue };
                // Inline-await — emit is a state mutation, not a long
                // round-trip; no need to spawn.
                fantastic_kernel::send::with_sender(sender_for_scope, async {
                    kernel.emit(&target, payload).await
                })
                .await;
            }
            "watch" => {
                let Some(src) = env.get("src").and_then(Value::as_str).map(AgentId::from) else {
                    continue;
                };
                let mut w = watching.lock().await;
                if !w.contains(&src) {
                    kernel.watch(&src, client_id.clone()).await;
                    w.insert(src);
                }
            }
            "unwatch" => {
                let Some(src) = env.get("src").and_then(Value::as_str).map(AgentId::from) else {
                    continue;
                };
                let mut w = watching.lock().await;
                if w.remove(&src) {
                    kernel.unwatch(&src, &client_id).await;
                }
            }
            "state_subscribe" => {
                // Python parity (web/_proxy.py:231-256): first emit a
                // `state_snapshot` frame carrying every agent's identity
                // so the consumer (telemetry_pane) can bootstrap before
                // the first event arrives, THEN register a callback
                // pumping subsequent `state_event` frames.
                let snapshot_frame = serde_json::json!({
                    "type": "state_snapshot",
                    "agents": kernel.state_snapshot(),
                });
                if let Ok(line) = serde_json::to_string(&snapshot_frame) {
                    let _ = out_tx.send(line).await;
                }
                let out_for_cb = out_tx.clone();
                let kernel_for_cb = Arc::clone(&kernel);
                let token = kernel.add_state_subscriber(Arc::new(move |event: &Value| {
                    let mut frame = match event.clone() {
                        Value::Object(m) => m,
                        _ => return,
                    };
                    // Lazy `name` fill — Python derives it from the
                    // agent record if the state event lacks one, so the
                    // browser always renders with a single shape. The
                    // lookup is cheap (DashMap by id) and only fires for
                    // events missing the field.
                    if !frame.contains_key("name") {
                        if let Some(aid) = frame.get("agent_id").and_then(Value::as_str) {
                            let aid_key = AgentId::from(aid);
                            if let Some(a) = kernel_for_cb.agents.get(&aid_key) {
                                let name = a.display_name().unwrap_or_else(|| a.id.0.clone());
                                frame.insert("name".into(), Value::String(name));
                            }
                        }
                    }
                    frame.insert("type".to_string(), Value::String("state_event".into()));
                    let line = match serde_json::to_string(&Value::Object(frame)) {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    // Best-effort: drop if the WS drain channel is gone
                    // (disconnected mid-fanout) — the unsubscribe will
                    // remove this callback shortly anyway.
                    let _ = out_for_cb.try_send(line);
                }));
                state_unsubs
                    .lock()
                    .expect("state_unsubs poisoned")
                    .push(token);
            }
            "state_unsubscribe" => {
                let tokens: Vec<_> = state_unsubs
                    .lock()
                    .expect("state_unsubs poisoned")
                    .drain(..)
                    .collect();
                for t in tokens {
                    kernel.remove_state_subscriber(t);
                }
            }
            _ => {}
        }
    }

    // ─── Cleanup on disconnect (Python: `finally` block) ──────────
    // 1. Cancel any in-flight `call` handlers so kernel.send tasks
    //    unwind (ollama_backend honors CancelledError, releases its
    //    FIFO lock, emits done).
    {
        let mut p = pending_calls.lock().expect("pending poisoned");
        for h in p.drain(..) {
            h.abort();
        }
    }
    // 2. Unwatch every src this WS subscribed to.
    {
        let srcs: Vec<AgentId> = watching.lock().await.drain().collect();
        for src in srcs {
            state.kernel.unwatch(&src, &client_id).await;
        }
    }
    // 3. Unregister any state-stream callbacks tied to this WS.
    {
        let tokens: Vec<_> = state_unsubs
            .lock()
            .expect("state_unsubs poisoned")
            .drain(..)
            .collect();
        for t in tokens {
            state.kernel.remove_state_subscriber(t);
        }
    }
    // 4. Drop the synthetic inbox + abort the drain/send tasks.
    state.kernel.inboxes.remove(&client_id);
    drain_task.abort();
    send_task.abort();
}

/// Maximum size of a single binary WS frame's blob payload. Frames
/// larger than this are rejected with an error reply.
const MAX_CHUNK_SIZE: usize = 1_048_576; // 1 MB

/// Maximum total size of a reassembled chunked upload. Sum of all
/// chunks; chunks that would push past this cap are rejected.
const MAX_UPLOAD_SIZE: usize = 100 * 1_048_576; // 100 MB

/// Maximum concurrent in-flight chunked uploads per WS connection.
const MAX_CONCURRENT_UPLOADS: usize = 16;

/// Per-upload reassembly buffer. Lives in a per-WS map keyed by
/// `upload_id`; drops on WS disconnect or on successful final dispatch.
struct ChunkBuffer {
    /// Chunks indexed by `chunk_index`. BTreeMap keeps them sorted
    /// so concatenation is in-order regardless of arrival order.
    chunks: std::collections::BTreeMap<u32, Vec<u8>>,
    /// Declared total chunk count (must match across every chunk).
    total: u32,
    /// Cumulative byte count across all chunks received so far.
    cumulative_bytes: usize,
    /// First chunk's header (with chunking fields stripped) — used as
    /// the dispatched header when the upload finalizes.
    base_header: Value,
}

/// Decode a binary WS frame and dispatch it through
/// [`Kernel::send_with_binary`]. Frame shape (matches Python's wire):
///
/// ```text
/// [4-byte BE u32 header_len][header_len bytes JSON header][rest = blob]
/// ```
///
/// `header` must carry at least `target` (agent id); `id` (if present)
/// rides through to the reply frame so callers can correlate.
///
/// ## Chunked uploads (opt-in)
///
/// For payloads larger than `MAX_CHUNK_SIZE`, callers can chunk by
/// adding these optional fields to the header:
///
/// - `upload_id`: stable string tying all chunks of one upload
/// - `chunk_index`: 0-based index of this chunk
/// - `total_chunks`: declared chunk count (must match across chunks)
/// - `final`: `true` on the last chunk; triggers reassembly + dispatch
///
/// Server-side: chunks accumulate in a per-WS `pending_uploads` map.
/// On the final chunk, the full blob is concatenated in chunk-index
/// order, the chunking fields are stripped from the header, and the
/// dispatch happens via `kernel.send_with_binary` exactly as if a
/// single-frame upload had been sent.
///
/// Frames without `upload_id` take the single-frame fast path
/// (current behaviour, byte-compatible with Python's wire).
///
/// On any decode error the frame is dropped silently (we log a debug
/// trace) — the WS stays open. The shape is documented + a single
/// malformed frame from a JS client shouldn't kill the channel.
async fn handle_binary_frame(
    state: &AppState,
    client_id: &AgentId,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    pending_uploads: &Arc<std::sync::Mutex<std::collections::HashMap<String, ChunkBuffer>>>,
    bytes: Vec<u8>,
) {
    if bytes.len() < 4 {
        tracing::debug!(
            len = bytes.len(),
            "ws binary frame too short for header_len"
        );
        return;
    }
    let hdr_len = u32::from_be_bytes(match bytes[0..4].try_into() {
        Ok(arr) => arr,
        Err(_) => {
            tracing::debug!("ws binary frame: failed to read header_len");
            return;
        }
    }) as usize;
    if 4usize.saturating_add(hdr_len) > bytes.len() {
        tracing::debug!(
            hdr_len,
            frame_len = bytes.len(),
            "ws binary frame: header_len exceeds frame"
        );
        return;
    }
    let header: Value = match serde_json::from_slice(&bytes[4..4 + hdr_len]) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "ws binary frame: header decode failed");
            return;
        }
    };
    let blob = bytes[4 + hdr_len..].to_vec();
    let id = header.get("id").cloned();

    // Per-frame chunk size cap (applies to both single-frame uploads and
    // individual chunks of a chunked upload).
    if blob.len() > MAX_CHUNK_SIZE {
        let _ = out_tx
            .send(
                json!({
                    "type": "error",
                    "id": id,
                    "error": format!(
                        "binary frame: blob {} bytes exceeds chunk cap {}",
                        blob.len(),
                        MAX_CHUNK_SIZE,
                    ),
                })
                .to_string(),
            )
            .await;
        return;
    }

    // Chunked upload? Check for upload_id; otherwise fall through to
    // the single-frame fast path.
    let upload_id = header
        .get("upload_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(upload_id) = upload_id {
        handle_chunked_frame(
            state,
            client_id,
            out_tx,
            pending_uploads,
            upload_id,
            header,
            id.unwrap_or(Value::Null),
            blob,
        )
        .await;
        return;
    }

    // Single-frame upload — existing dispatch path.
    let Some(target_str) = header.get("target").and_then(Value::as_str) else {
        tracing::debug!("ws binary frame: header missing target");
        return;
    };
    let target = AgentId::from(target_str);

    let kernel = Arc::clone(&state.kernel);
    let sender_for_scope = client_id.clone();
    let out = out_tx.clone();
    let header_for_dispatch = header.clone();
    tokio::spawn(async move {
        let reply = fantastic_kernel::send::with_sender(sender_for_scope, async {
            kernel
                .send_with_binary(&target, header_for_dispatch, blob)
                .await
        })
        .await;
        // Verb-level errors live inside `data.error` (Python wire shape).
        let frame = json!({
            "type": "reply",
            "id": id,
            "data": reply,
        });
        let _ = out.send(frame.to_string()).await;
    });
}

/// Handle one chunk of a chunked upload. Either appends to the
/// reassembly buffer + acks (non-final), or assembles + dispatches
/// (final). Per-WS state, no global locks.
#[allow(clippy::too_many_arguments)]
async fn handle_chunked_frame(
    state: &AppState,
    client_id: &AgentId,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    pending_uploads: &Arc<std::sync::Mutex<std::collections::HashMap<String, ChunkBuffer>>>,
    upload_id: String,
    mut header: Value,
    id: Value,
    blob: Vec<u8>,
) {
    let chunk_index = match header.get("chunk_index").and_then(Value::as_u64) {
        Some(n) if n <= u32::MAX as u64 => n as u32,
        _ => {
            let _ = out_tx
                .send(
                    json!({
                        "type": "error",
                        "id": id,
                        "error": "chunked upload: chunk_index required (u32)",
                    })
                    .to_string(),
                )
                .await;
            return;
        }
    };
    let total_chunks = match header.get("total_chunks").and_then(Value::as_u64) {
        Some(n) if n >= 1 && n <= u32::MAX as u64 => n as u32,
        _ => {
            let _ = out_tx
                .send(
                    json!({
                        "type": "error",
                        "id": id,
                        "error": "chunked upload: total_chunks required (u32 ≥ 1)",
                    })
                    .to_string(),
                )
                .await;
            return;
        }
    };
    let is_final = header
        .get("final")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if chunk_index >= total_chunks {
        let _ = out_tx
            .send(
                json!({
                    "type": "error",
                    "id": id,
                    "error": format!(
                        "chunked upload: chunk_index {chunk_index} >= total_chunks {total_chunks}",
                    ),
                })
                .to_string(),
            )
            .await;
        return;
    }

    // Build base_header (header with chunking fields stripped) before we
    // borrow it for the assembled dispatch later.
    if let Some(obj) = header.as_object_mut() {
        obj.remove("upload_id");
        obj.remove("chunk_index");
        obj.remove("total_chunks");
        obj.remove("final");
    }

    // Outcome of the locked block — either `Ok(Some(...))` (final chunk
    // ready to dispatch), `Ok(None)` (non-final, expect ack), or
    // `Err(msg)` (validation failure; map entry already cleaned up).
    let outcome: Result<Option<(Value, Vec<u8>)>, String> = {
        let mut map = pending_uploads.lock().expect("pending_uploads poisoned");

        // Cap concurrent uploads BEFORE inserting a new id.
        if !map.contains_key(&upload_id) && map.len() >= MAX_CONCURRENT_UPLOADS {
            Err(format!(
                "chunked upload: concurrent upload cap {MAX_CONCURRENT_UPLOADS} reached",
            ))
        } else {
            // Insert if missing, then validate against any prior state.
            let entry_existed = map.contains_key(&upload_id);
            let buf = map.entry(upload_id.clone()).or_insert_with(|| ChunkBuffer {
                chunks: std::collections::BTreeMap::new(),
                total: total_chunks,
                cumulative_bytes: 0,
                base_header: header.clone(),
            });

            // total_chunks must agree across every chunk.
            if entry_existed && buf.total != total_chunks {
                let prior = buf.total;
                map.remove(&upload_id);
                Err(format!(
                    "chunked upload: total_chunks mismatch (saw {prior}, now {total_chunks})",
                ))
            } else {
                let projected = buf.cumulative_bytes.saturating_add(blob.len());
                if projected > MAX_UPLOAD_SIZE {
                    map.remove(&upload_id);
                    Err(format!(
                        "chunked upload: projected {projected} bytes exceeds total cap {MAX_UPLOAD_SIZE}",
                    ))
                } else {
                    buf.cumulative_bytes = projected;
                    buf.chunks.insert(chunk_index, blob);

                    if is_final {
                        let total_seen = buf.chunks.len() as u32;
                        let total_expected = buf.total;
                        if total_seen != total_expected {
                            map.remove(&upload_id);
                            Err(format!(
                                "chunked upload: final received but {} chunk(s) missing",
                                total_expected - total_seen,
                            ))
                        } else {
                            let removed = map.remove(&upload_id).expect("entry just inserted");
                            let mut full = Vec::with_capacity(removed.cumulative_bytes);
                            for (_, chunk) in removed.chunks {
                                full.extend_from_slice(&chunk);
                            }
                            Ok(Some((removed.base_header, full)))
                        }
                    } else {
                        Ok(None)
                    }
                }
            }
        }
    };

    let assembled: Option<(Value, Vec<u8>)> = match outcome {
        Ok(v) => v,
        Err(msg) => {
            let _ = out_tx
                .send(
                    json!({
                        "type": "error",
                        "id": id,
                        "error": msg,
                    })
                    .to_string(),
                )
                .await;
            return;
        }
    };

    match assembled {
        Some((base_header, full_blob)) => {
            // Dispatch the reassembled upload exactly like a single-frame.
            let Some(target_str) = base_header.get("target").and_then(Value::as_str) else {
                let _ = out_tx
                    .send(
                        json!({
                            "type": "error",
                            "id": id,
                            "error": "chunked upload: assembled header missing target",
                        })
                        .to_string(),
                    )
                    .await;
                return;
            };
            let target = AgentId::from(target_str);
            let kernel = Arc::clone(&state.kernel);
            let sender_for_scope = client_id.clone();
            let out = out_tx.clone();
            tokio::spawn(async move {
                let reply = fantastic_kernel::send::with_sender(sender_for_scope, async {
                    kernel
                        .send_with_binary(&target, base_header, full_blob)
                        .await
                })
                .await;
                // Verb-level errors live inside `data.error` (Python wire shape).
                let frame = json!({
                    "type": "reply",
                    "id": id,
                    "data": reply,
                });
                let _ = out.send(frame.to_string()).await;
            });
        }
        None => {
            // Non-final chunk — emit a chunk_ack so the client can
            // flow-control its upload pipeline.
            let _ = out_tx
                .send(
                    json!({
                        "type": "chunk_ack",
                        "upload_id": upload_id,
                        "chunk_index": chunk_index,
                    })
                    .to_string(),
                )
                .await;
        }
    }
}

// ─── REST surface ───────────────────────────────────────────────────
//
// POST /<rest_id>/<target_id> body=<payload-json> → kernel.send → JSON.
// GET  /<rest_id>/_reflect[/<target_id>][?readme=1]               → reflect helper.

#[derive(serde::Deserialize)]
struct ReflectQuery {
    readme: Option<u8>,
}

// (REST handler implementations live above in the dynamic-mount block:
// `serve_rest_post_dynamic`, `serve_rest_reflect_root_dynamic`,
// `serve_rest_reflect_dynamic`. The previous static-route versions
// expected the `:rest_id` segment in the path; that segment is now
// baked into the mount-time path literal, so no static handlers are
// needed.)

// base64 is a transitive dep — re-declare via fantastic-kernel's
// indirect graph. Pull it in via Cargo.toml if not already there.
#[allow(unused_imports)]
use base64 as _;

#[cfg(test)]
mod tests;
