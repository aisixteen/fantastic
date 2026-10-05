//! HTTP host bundle — the rust port of python's `web` (`PB/web/host/src/web/`).
//!
//! A `web` agent owns a `port` field on its record (REQUIRED — no default). On
//! `boot` it builds the route table, mounts every child surface through the
//! duck-typed `get_routes` verb, and only then starts listening, so the port
//! never serves a request before its full route table is in place.
//!
//! The baked routes are RENDERING ONLY, and they are exactly python's two:
//!
//! | route                           | what                                   |
//! |---------------------------------|----------------------------------------|
//! | `GET /_assets/favicon.png`      | the bundled favicon                    |
//! | `GET /{agent_id}/file/{path}`   | pipe the agent's `read_stream` SOURCE  |
//!
//! Verb-invocation surfaces (WS, REST) are NOT baked in: they live in
//! `fantastic-web-ws` / `fantastic-web-rest` child agents and mount through
//! `mount` / `unmount` / boot.
//!
//! ### Verbs
//!
//! - `reflect` → identity, port, host, allowed_origins, running, served_agent_count, surfaces, routes,
//!   browser_reachable
//! - `boot`    → `{running, surfaces}`. Idempotent; a missing `port` RAISES.
//! - `stop`    → `{stopped}`
//! - `mount` / `unmount` → (re)mount one child's declared routes
//! - `fetch_file` → one `read_stream` chunk of an agent's file, read AS THIS HOST
//!   — the file route hands each browser fetch here
//!
//! IDENTITY: the HTTP server is the TRANSPORT — its request tasks run outside
//! every dispatch, so a request is the operator. The file route never reads as
//! anyone itself: it hands the fetch to this host (`fetch_file`), and the host
//! reads as itself, gated as `web`. The call legs hand their frames to THEIR leg
//! the same way (`fantastic_io_bridge::inbound`).

#![deny(missing_docs)]

mod handlers;
pub(crate) mod mimetypes;
mod mimetypes_table;
pub(crate) mod origin;
pub(crate) mod routes;
pub(crate) mod ws;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use fantastic_bundle::{py_int, py_repr_value, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::task::JoinHandle;

use routes::RouteTable;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web.tools";

pub use origin::DEFAULT_HOST;

/// readme.md auto-seeded into the agent's dir on creation. Python's file, byte
/// for byte (`PB/web/host/src/web/readme.md`).
pub const README: &str = include_str!("readme.md");

/// Bundled favicon — python's `web/favicon.png`, byte for byte.
pub const FAVICON_PNG: &[u8] = include_bytes!("favicon.png");

/// Live web servers, keyed by web-agent id.
pub(crate) static SERVERS: once_cell_lock::OnceLockMap = once_cell_lock::OnceLockMap::new();

/// Per-agent live server bookkeeping. Tracked outside `Agent` so the
/// substrate's plain record stays disk-shape only.
pub(crate) struct ServerHandle {
    /// The serve loop's task handle; `None` until `_start_serving`'s twin runs.
    pub task: Option<JoinHandle<()>>,
    /// The live ordered route table; mount/unmount swap entries in place.
    pub table: Arc<AsyncRwLock<RouteTable>>,
    /// The port the record asked for (python reports `handle.port`, the
    /// CONFIGURED port, never the bound one).
    pub port: u16,
    /// The bind address, read off the record when the handle is built (python
    /// `_spawn` puts it in uvicorn's config) — what the listener binds and what
    /// the boot announcement names.
    pub host: String,
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

/// Shared state available to every request handler.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Reference to the live kernel so handlers can `kernel.send(...)`.
    pub kernel: Arc<Kernel>,
    /// This web agent's id — the host the file route hands each fetch to.
    pub web_agent_id: AgentId,
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

    /// `fetch_file` answers a raw chunk beside its JSON; every other verb is
    /// [`Bundle::handle`]'s.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        _body: Option<fantastic_kernel::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<fantastic_kernel::Blob>), BundleError> {
        if header.get("type").and_then(Value::as_str) == Some("fetch_file") {
            let (v, body) = fetch_file_reply(&header, kernel).await;
            return Ok((Some(v), body));
        }
        Ok((self.handle(agent_id, &header, kernel).await?, None))
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
            "boot" => boot_reply(agent_id, kernel).await?,
            "stop" => stop_reply(agent_id).await,
            "mount" => mount_reply(agent_id, payload, kernel).await,
            "unmount" => unmount_reply(agent_id, payload).await,
            _ => {
                let raw = payload.get("type").cloned().unwrap_or(Value::Null);
                json!({"error": format!("web: unknown type {}", py_repr_value(&raw))})
            }
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        stop_reply(agent_id).await;
        Ok(())
    }
    /// Shutdown frees what delete frees — declared, never a fallback (audit c9).
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.on_delete(agent_id, kernel).await
    }
}

// ─── verbs ──────────────────────────────────────────────────────────

/// python `_reflect` — identity + port + running flag + the live surfaces, the
/// browser-facing route CONTRACT and the computed `browser_reachable` property.
async fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let port = read_port(agent_id, kernel);
    let (running, surfaces) = {
        let handle = SERVERS.lock();
        match handle.get(agent_id) {
            Some(h) => (true, Some(Arc::clone(&h.table))),
            None => (false, None),
        }
    };
    let mut surface_map = Map::new();
    if let Some(table) = surfaces {
        let table = table.read().await;
        for cid in table.child_ids() {
            let key = AgentId::from(cid.as_str());
            surface_map.insert(cid, json!(table.paths_for(&key)));
        }
    }
    let mut out = Map::new();
    out.insert("id".into(), json!(agent_id.as_str()));
    out.insert(
        "sentence".into(),
        json!("HTTP host — rendering routes baked in; call surfaces mounted from sub-agents."),
    );
    out.insert("port".into(), port.map_or(Value::Null, |p| json!(p)));
    let rec = read_rec(agent_id, kernel);
    out.insert("host".into(), json!(origin::effective_host(&rec)));
    out.insert(
        "allowed_origins".into(),
        json!(origin::reflect_origins(&rec)),
    );
    out.insert("running".into(), json!(running));
    out.insert("served_agent_count".into(), json!(kernel.agents.len()));
    out.insert("surfaces".into(), Value::Object(surface_map));
    // THE BROWSER-FACING CONTRACT. `/file/` is the only route a browser hits,
    // and it was described NOWHERE — so a model reflected this agent, found no
    // route contract, and HALLUCINATED one (measured 2026-08-21). The field
    // they invented is the field that was missing.
    out.insert(
        "routes".into(),
        json!({
            "GET /<agent_id>/file/<path>":
                "Streams a file by read_stream-ing through the named agent. The \
        read is dispatched AS THIS WEB AGENT, so it needs BOTH sides of \
        the call gate — this host's can_call[<agent_id>] (egress) AND that \
        agent's callable_by[<this web id>] (ingress) — plus that agent's \
        own io leg open (ingress_rule). Any one missing → 404. See \
        browser_reachable for who actually passes all three."
        }),
    );
    // The PROPERTY, computed — not a description of it. `ingress_rule=allow_all_ingress`
    // reads like "open" while the call gate still refuses, so config never
    // carried the answer to "what can a browser actually get?".
    out.insert(
        "browser_reachable".into(),
        json!(browser_reachable(agent_id, kernel)),
    );
    out.insert(
        "verbs".into(),
        json!({
            "reflect": "Identity + uvicorn port + running flag + currently mounted surfaces (process-local; reflect the live serve for truth).",
            "boot": "Idempotent. Builds the host on rec.port (required, no default), mounts each",
            "stop": "No args. Asks uvicorn to shut down; cancels its task. Returns {stopped:bool}.",
            "mount": "args: child_id:str (req). Pulls `get_routes` from the named child and (re)mounts its routes onto the live app. Idempotent — unmounts any prior routes for the child first.",
            "unmount": "args: child_id:str (req). Removes every route belonging to the named child from the live app.",
            "fetch_file": "args: agent_id:str (req), path:str (req), offset:int (req), length:int (req). One `read_stream` chunk of `path` on `agent_id`, read AS THIS HOST — the file route hands every browser fetch here, so the served agent's gates judge `web`. Returns that agent's reply.",
        }),
    );
    Value::Object(out)
}

/// python `_browser_reachable` — which agents a browser could actually fetch
/// through this host, RIGHT NOW. Asks the REAL gates (the same `authorize_call`
/// `Agent.send` uses, plus the io leg's own resolved ingress rule) rather than
/// re-deriving them: a reimplementation here would be a second truth that
/// silently drifts, which is the exact bug this field exists to expose.
fn browser_reachable(web_id: &AgentId, kernel: &Kernel) -> Vec<String> {
    use fantastic_kernel::authz::authorize_call;
    let Some(web) = kernel.agents.get(web_id).map(|e| Arc::clone(&e)) else {
        return Vec::new();
    };
    let web_meta = web.meta.read().expect("meta poisoned").clone();
    let web_ancestors = kernel.ancestor_ids(Some(web.as_ref()));
    let mut out: Vec<String> = Vec::new();
    // Snapshot first: the gates below must not run under a live map iteration.
    let agents: Vec<Arc<fantastic_kernel::Agent>> = kernel
        .agents
        .iter()
        .map(|e| Arc::clone(e.value()))
        .collect();
    for agent in &agents {
        if agent.id == *web_id {
            continue;
        }
        let meta = agent.meta.read().expect("meta poisoned").clone();
        // An agent that cannot answer the SOURCE verb is not a file server at all.
        let declares = meta
            .get("verbs")
            .and_then(Value::as_object)
            .is_some_and(|v| v.contains_key("read_stream"));
        let servable = declares
            || agent
                .handler_module
                .as_deref()
                .unwrap_or("")
                .starts_with("file_bridge");
        if !servable {
            continue;
        }
        let egress = authorize_call(
            web_meta.get("can_call").unwrap_or(&Value::Null),
            "read_stream",
            agent.id.as_str(),
            web_id.as_str(),
            web.parent_id.as_ref().map(|p| p.0.as_str()),
            &kernel.ancestor_ids(Some(agent.as_ref())),
        );
        if !egress.allowed {
            continue;
        }
        let ingress = authorize_call(
            meta.get("callable_by").unwrap_or(&Value::Null),
            "read_stream",
            web_id.as_str(),
            agent.id.as_str(),
            agent.parent_id.as_ref().map(|p| p.0.as_str()),
            &web_ancestors,
        );
        if !ingress.allowed {
            continue;
        }
        if leg_admits(&meta, agent.id.as_str(), "read_stream") {
            out.push(agent.id.0.clone());
        }
    }
    out.sort();
    out
}

/// The io leg's own resolved ingress rule (python `gate_inbound(resolve_ingress(rec), …)`).
fn leg_admits(meta: &Map<String, Value>, target: &str, verb: &str) -> bool {
    use fantastic_io_bridge::authorizer::{ingress, manifold_spec, Action, Decision};
    let spec = manifold_spec(meta.get("ingress_rule"), meta.get("auth"), "_ingress");
    matches!(
        ingress::resolve(spec.as_ref()).authorize(&Action {
            kind: "call",
            target,
            verb,
            token: None,
        }),
        Decision::Allow
    )
}

/// python `_boot` — build the host on `rec.port`, mount every child's surface,
/// THEN start serving, then announce the URL to `cli`.
async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<Value, BundleError> {
    let already = SERVERS.lock().contains_key(agent_id);
    if !already {
        // python `_spawn`: a falsy `port` RAISES — no default, no fallback.
        let Some(port) = read_port(agent_id, kernel) else {
            return Err(format!(
                "RuntimeError: webapp {agent_id}: rec.port is required (no default). \
Set port on create_agent or via update_agent."
            )
            .into());
        };
        SERVERS.lock().insert(
            agent_id.clone(),
            ServerHandle {
                task: None,
                table: Arc::new(AsyncRwLock::new(RouteTable::base())),
                port,
                // LOOPBACK unless the record names a host (`host: "0.0.0.0"` to publish).
                host: origin::effective_host(&read_rec(agent_id, kernel)),
            },
        );
    }
    let Some((table, port, host)) = SERVERS
        .lock()
        .get(agent_id)
        .map(|h| (Arc::clone(&h.table), h.port, h.host.clone()))
    else {
        return Ok(json!({"running": false}));
    };
    let state = AppState {
        kernel: Arc::clone(kernel),
        web_agent_id: agent_id.clone(),
    };
    // Mount all child surfaces BEFORE the listener starts accepting.
    let children: Vec<AgentId> = kernel
        .agents
        .get(agent_id)
        .map(|e| e.child_ids())
        .unwrap_or_default();
    for cid in children {
        mount_surface(&table, kernel, &cid).await;
    }
    start_serving(agent_id, &state, &table, &host, port).await;
    // Announce OURSELVES to the cli renderer (the boot-event convention): the
    // web host owns its port, so it tells the terminal where to attach — the
    // renderer never reaches in for it. No cli (non-tty) ⇒ nobody is told; the
    // bound port is on this agent's reflect. The URL names the BOUND host — a
    // host bound to a LAN address does not answer at 127.0.0.1 (audit ARCH2-10).
    let line = format!(
        "listening on http://{host}:{port}/ · rest/ws are children (reflect kernel for ids)"
    );
    kernel
        .send(vec![Item::new(
            AgentId::from("cli"),
            json!({"type": "say", "source": "web", "text": line}),
        )])
        .await;
    let surfaces = table.read().await.child_ids();
    Ok(json!({"running": true, "surfaces": surfaces}))
}

/// python `_start_serving` — schedule the serve loop for an already-built,
/// already-route-mounted handle. Idempotent. A bind failure is LOGGED, never
/// raised: uvicorn's `sys.exit(1)` out of the serve task would otherwise take
/// the whole kernel down with one bad webapp.
async fn start_serving(
    agent_id: &AgentId,
    state: &AppState,
    table: &Arc<AsyncRwLock<RouteTable>>,
    host: &str,
    port: u16,
) {
    if SERVERS
        .lock()
        .get(agent_id)
        .is_some_and(|h| h.task.is_some())
    {
        return;
    }
    let listener = match tokio::net::TcpListener::bind((host, port)).await {
        Ok(l) => l,
        Err(e) => {
            // python `logger.error("webapp %s :%d serve failed: %s", …)`
            // (`web/tools.py:158`), through `logging.lastResort` → bare stderr.
            // python's OTHER spelling here (uvicorn raising SystemExit on a bind
            // clash → `serve exited (code=…) — port likely in use`) is a uvicorn
            // artefact, residual per D16.
            eprintln!("webapp {agent_id} :{port} serve failed: {e}");
            SERVERS.lock().remove(agent_id);
            return;
        }
    };
    let dynamic = DynamicRouter {
        state: state.clone(),
        table: Arc::clone(table),
    };
    let serve = axum::serve(listener, dynamic.into_make_service());
    let failed_id = agent_id.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = serve.await {
            // Same python line — `_safe_serve`'s `except Exception` wraps the
            // whole `server.serve()`, not just the bind.
            eprintln!("webapp {failed_id} :{port} serve failed: {e}");
        }
    });
    if let Some(h) = SERVERS.lock().get_mut(agent_id) {
        h.task = Some(task);
    }
}

/// python `_stop_uvicorn` + `_stop` — `{stopped:bool}`.
async fn stop_reply(agent_id: &AgentId) -> Value {
    let removed = SERVERS.lock().remove(agent_id);
    match removed {
        Some(handle) => {
            if let Some(task) = handle.task {
                task.abort();
            }
            json!({"stopped": true})
        }
        None => json!({"stopped": false}),
    }
}

/// python `_mount` — pull `get_routes` from the named child and (re)mount it.
/// python `_fetch_file` — one `read_stream` chunk of `path` on `agent_id`, sent
/// from this host's own dispatch, so the read is THE HOST's: the served agent's
/// `callable_by` and its io leg judge `web`. The reply comes back whole, its raw
/// chunk at its slot.
async fn fetch_file_reply(
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> (Value, Option<fantastic_kernel::Blob>) {
    let Some(agent_id) = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return (json!({"error": "web.fetch_file: agent_id required"}), None);
    };
    let field = |k: &str| payload.get(k).cloned().unwrap_or(Value::Null);
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(agent_id),
            json!({
                "type": "read_stream",
                "path": field("path"),
                "offset": field("offset"),
                "length": field("length"),
            }),
        )])
        .await
        .remove(0);
    (out.result, out.body)
}

async fn mount_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(child) = child_arg(payload) else {
        return json!({"error": "web.mount: child_id required"});
    };
    let Some(table) = SERVERS.lock().get(agent_id).map(|h| Arc::clone(&h.table)) else {
        return json!({"error": "web.mount: not running"});
    };
    table.write().await.unmount(&child);
    mount_surface(&table, kernel, &child).await;
    json!({"mounted": table.read().await.paths_for(&child)})
}

/// python `_unmount` — remove every route belonging to the named child.
async fn unmount_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let Some(child) = child_arg(payload) else {
        return json!({"error": "web.unmount: child_id required"});
    };
    let Some(table) = SERVERS.lock().get(agent_id).map(|h| Arc::clone(&h.table)) else {
        return json!({"error": "web.unmount: not running"});
    };
    table.write().await.unmount(&child);
    json!({"unmounted": child.0})
}

/// python `payload.get("child_id")` guarded by its own truthiness.
fn child_arg(payload: &Value) -> Option<AgentId> {
    let v = payload.get("child_id")?;
    if !py_truthy(v) {
        return None;
    }
    v.as_str().map(AgentId::from)
}

/// python `_mount_surface` — ask one child for its routes and mount them. A
/// child that does not answer with a route list mounts nothing.
async fn mount_surface(
    table: &Arc<AsyncRwLock<RouteTable>>,
    kernel: &Arc<Kernel>,
    child: &AgentId,
) {
    let reply = kernel
        .send(vec![Item::new(
            child.clone(),
            json!({"type": "get_routes"}),
        )])
        .await
        .remove(0)
        .result;
    let mounted = routes::routes_from_spec(child, &reply);
    if mounted.is_empty() {
        return;
    }
    table.write().await.mount(child.clone(), mounted);
}

/// python `int(rec.get("port"))` with its falsy check — `None`, `0` and `""`
/// all mean "not set".
fn read_port(agent_id: &AgentId, kernel: &Kernel) -> Option<u16> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned").clone();
    let raw = meta.get("port")?;
    if !py_truthy(raw) {
        return None;
    }
    py_int(raw).ok().and_then(|n| u16::try_from(n).ok())
}

/// The agent's record meta (python `kernel.get(id) or {}`).
pub(crate) fn read_rec(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    kernel
        .agents
        .get(agent_id)
        .map(|e| e.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default()
}

// ─── the serving edge ───────────────────────────────────────────────

/// `DynamicRouter` walks the live route table for each request. Mounting and
/// unmounting a child surface is a write-lock swap; per-request reads take the
/// read-lock for the length of the match only.
#[derive(Clone)]
struct DynamicRouter {
    state: AppState,
    table: Arc<AsyncRwLock<RouteTable>>,
}

impl DynamicRouter {
    fn into_make_service(self) -> IntoMakeService {
        IntoMakeService(self)
    }
}

/// `tower::MakeService` shim: one router instance per accepted connection.
#[derive(Clone)]
struct IntoMakeService(DynamicRouter);

impl<T> tower::Service<T> for IntoMakeService {
    type Response = DynamicRouter;
    type Error = std::convert::Infallible;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: T) -> Self::Future {
        let router = self.0.clone();
        Box::pin(async move { Ok(router) })
    }
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
        let state = self.state.clone();
        let table = Arc::clone(&self.table);
        Box::pin(async move {
            // python `_OriginGuard`: in front of EVERY route, the record read per request.
            let rec = read_rec(&state.web_agent_id, &state.kernel);
            if let Some(refused) = origin::guard(state.web_agent_id.as_str(), &rec, &req) {
                return Ok(refused);
            }
            Ok(handlers::dispatch(state, table, req).await)
        })
    }
}

#[cfg(test)]
mod tests;
