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
//! - `reflect` → identity, port, host, allowed_origins, running, served_agent_count, surfaces,
//!   browser_reachable
//! - `request` → one outbound HTTP call to a `request_allow` host (issue #695)
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
pub mod record;
mod request;
pub(crate) mod routes;
pub mod verbs;
pub(crate) mod ws;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
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

/// The address the listener binds: the record's `bind` when it names one (the
/// container's `0.0.0.0`), else its `host`, else loopback. The Host/Origin guard
/// judges by `host` alone (`origin.rs`), so a wildcard `bind` leaves the guard's
/// identity where it was. Python `tools.py` agrees.
pub fn listen_host(rec: &Map<String, Value>) -> String {
    match rec.get("bind").and_then(Value::as_str) {
        Some(bind) => bind.to_string(),
        None => origin::effective_host(rec),
    }
}

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
    /// The identity of the dispatch that started the server: every request runs
    /// with it, so its hand-ins are that dispatch's sends (the per-connection
    /// tasks the server spawns carry no task-locals of their own).
    pub inherited: fantastic_kernel::Inherited,
}

/// The HTTP host bundle.
pub struct WebBundle;

/// What the bundle is for — its one short man text.
const MAN: &str = "An HTTP host: it serves the favicon and GET /<agent_id>/file/<path> (a file streamed from an agent's read_stream, read as this host) and mounts the call surfaces (web_ws, web_rest) its child agents declare.";

/// Serve one checked call; `fetch_file` answers a raw chunk beside its JSON.
async fn serve(
    agent_id: &AgentId,
    call: verbs::Call,
    kernel: &Arc<Kernel>,
) -> Result<(Reply, Option<fantastic_kernel::Blob>), BundleError> {
    let reply = match call {
        verbs::Call::Reflect(_) => reflect_reply(agent_id, kernel).await,
        verbs::Call::Boot(_) => boot_reply(agent_id, kernel).await?,
        verbs::Call::Shutdown(_) => Value::Null,
        verbs::Call::Stop(_) => stop_reply(agent_id).await,
        verbs::Call::Mount(a) => mount_reply(agent_id, &a.child_id, kernel).await,
        verbs::Call::Unmount(a) => unmount_reply(agent_id, &a.child_id).await,
        verbs::Call::Request(a) => {
            let rec = read_rec(agent_id, kernel);
            request::serve(agent_id, &a, &rec, kernel).await
        }
        verbs::Call::FetchFile(a) => {
            return Ok(match fetch_file_reply(a, kernel).await {
                Some((v, body)) => (Some(v), body),
                // deferred: the read's settle settles the call
                None => (Some(Value::Null), None),
            });
        }
    };
    Ok((Some(reply), None))
}

#[async_trait]
impl Handler for WebBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "web"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    /// `fetch_file` answers a raw chunk beside its JSON; every other verb has no body.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        _body: Option<fantastic_kernel::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<fantastic_kernel::Blob>), BundleError> {
        serve(agent_id, call, kernel).await
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(serve(agent_id, call, kernel).await?.0)
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        stop_reply(agent_id).await;
        request::forget(agent_id);
        Ok(())
    }
    /// Shutdown frees what delete frees — declared, never a fallback.
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
        json!(MAN),
    );
    out.insert("port".into(), port.map_or(Value::Null, |p| json!(p)));
    let rec = read_rec(agent_id, kernel);
    out.insert("host".into(), json!(origin::effective_host(&rec)));
    out.insert(
        "allowed_origins".into(),
        json!(origin::reflect_origins(&rec)),
    );
    out.insert("running".into(), json!(running));
    out.insert("served_agent_count".into(), json!(kernel.agent_ids().len()));
    out.insert("surfaces".into(), Value::Object(surface_map));
    // The PROPERTY, computed — not a description of it. `ingress_rule=allow_all_ingress`
    // reads like "open" while the call gate still refuses, so config never
    // carried the answer to "what can a browser actually get?".
    out.insert(
        "browser_reachable".into(),
        json!(browser_reachable(agent_id, kernel)),
    );
    // the outbound side: the allowlist, the credential ref per host (its name only)
    // and the latest outcomes; HOW IT IS DRAWN is an html view, compiled in
    let allow = request::allow_list(&rec);
    out.insert("request_allow".into(), json!(allow));
    // its place in the reply; the store reads fill it (steps, #795)
    out.insert("request_credentials".into(), json!({}));
    out.insert("recent_requests".into(), request::recent(agent_id));
    out.insert(
        "view".into(),
        json!({
            "kind": "html",
            "bytes": REQUEST_VIEW.replace("{{agent}}", agent_id.as_str()),
            "calls": {agent_id.as_str(): VIEW_CALLS},
        }),
    );
    if !request::looks_up(kernel, &allow) {
        // no store to read: the steps answer here and now
        let slot = Arc::new(std::sync::Mutex::new(Value::Null));
        let put = Arc::clone(&slot);
        request::bound_refs(
            Arc::clone(kernel),
            allow,
            Box::new(move |refs| {
                Box::pin(async move {
                    *put.lock().expect("refs poisoned") = refs;
                })
            }),
        )
        .await;
        let refs = std::mem::take(&mut *slot.lock().expect("refs poisoned"));
        out.insert("request_credentials".into(), refs);
        return Value::Object(out);
    }
    // THE CREDENTIAL REFS ARE STORE READS: the reflect defers, and its steps settle it
    let d = match kernel.defer() {
        Ok(d) => d,
        Err(refused) => return refused,
    };
    let k = Arc::clone(kernel);
    request::bound_refs(
        Arc::clone(kernel),
        allow,
        Box::new(move |refs| {
            Box::pin(async move {
                out.insert("request_credentials".into(), refs);
                k.settle_deferred(
                    d.pid(),
                    fantastic_kernel::Outcome::new(Some(d.pid()), fantastic_kernel::Status::Completed, Value::Object(out)),
                );
            })
        }),
    )
    .await;
    Value::Null
}

/// The agent's own view (html, plain JS): the outbound allowlist, the credential ref per
/// host and the recent requests. `{{agent}}` is the web agent's id, filled at reflect time.
const REQUEST_VIEW: &str = include_str!("request_view.html");

/// The verbs the view calls on its own agent: reads only, and `watch` (it re-reads on `request_served`).
pub const VIEW_CALLS: [&str; 2] = ["reflect", "watch"];

/// python `_browser_reachable` — which agents a browser could actually fetch
/// through this host, RIGHT NOW. Asks the REAL gate (the kernel's own check of
/// `read_stream` for the reader the file proxy sends as) rather than re-deriving
/// it: a reimplementation here would be a second truth that silently drifts, which
/// is the exact bug this field exists to expose.
fn browser_reachable(web_id: &AgentId, kernel: &Kernel) -> Vec<String> {
    if !kernel.exists(web_id.as_str()) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    // Snapshot first: the gates below must not run under a live map iteration.
    let agents: Vec<Arc<fantastic_kernel::Agent>> = kernel
        .agent_ids()
        .into_iter()
        .filter_map(|aid| kernel.resolve_target(aid.as_str()))
        .collect();
    for agent in &agents {
        if agent.id == *web_id {
            continue;
        }
        let meta = agent.record().meta;
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
        if kernel.may_call(web_id, "read_stream", &agent.id) {
            out.push(agent.id.0.clone());
        }
    }
    out.sort();
    out
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
                // LOOPBACK unless the record names a bind or a host (`bind: "0.0.0.0"` to publish).
                host: listen_host(&read_rec(agent_id, kernel)),
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
        inherited: fantastic_kernel::Inherited::capture(),
    };
    // Mount all child surfaces BEFORE the listener starts accepting.
    let children: Vec<AgentId> = kernel.resolve_target(agent_id.as_str())
        .map(|e| e.child_ids())
        .unwrap_or_default();
    // A BOOT THAT IS A CALL defers: its settle comes once every child is mounted and
    // the listener serves. A boot the kernel runs directly (`kernel_start`, the
    // web's create) answers `{running: true}` at once, and the mounts go on as calls
    // (#795) — the listener still binds only after the last of them.
    let deferred = kernel.defer().ok();
    let direct = deferred.is_none();
    mount_all(Booting {
        agent_id: agent_id.clone(),
        kernel: Arc::clone(kernel),
        state,
        table,
        host,
        port,
        todo: children.into(),
        deferred,
    })
    .await;
    Ok(if direct { json!({"running": true}) } else { Value::Null })
}

/// A boot on its way to serving: the children still to mount, and where its answer goes.
struct Booting {
    agent_id: AgentId,
    kernel: Arc<Kernel>,
    state: AppState,
    table: Arc<AsyncRwLock<RouteTable>>,
    host: String,
    port: u16,
    todo: std::collections::VecDeque<AgentId>,
    /// The boot's own call, when the boot was one.
    deferred: Option<fantastic_kernel::Deferred>,
}

impl Booting {
    /// The boot is over: `Ok` serves, `Err` is the refusal a conflicting route raises.
    fn end(self, r: Result<Value, String>) {
        match (self.deferred, r) {
            (Some(d), Ok(reply)) => {
                self.kernel
                    .settle_deferred(d.pid(), fantastic_kernel::Outcome::of_reply(Some(d.pid()), reply));
            }
            (Some(d), Err(reason)) => {
                let raised = json!({"error": format!("RuntimeError: {reason}"), "raised": true});
                self.kernel.settle_deferred(
                    d.pid(),
                    fantastic_kernel::Outcome::new(Some(d.pid()), fantastic_kernel::Status::Failed, raised),
                );
            }
            (None, Ok(_)) => {}
            (None, Err(reason)) => eprintln!("webapp {} boot: RuntimeError: {reason}", self.agent_id),
        }
    }
}

/// Mount each child's surface, ONE AFTER THE OTHER (each `get_routes` is a call whose
/// settle mounts it), THEN start serving, then announce the URL to `cli`.
fn mount_all(mut b: Booting) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let Some(child) = b.todo.pop_front() else {
            start_serving(&b.agent_id, &b.state, &b.table, &b.host, b.port).await;
            // Announce OURSELVES to the cli renderer (the boot-event convention): the
            // web host owns its port, so it tells the terminal where to attach — the
            // renderer never reaches in for it. No cli (non-tty) ⇒ nobody is told; the
            // bound port is on this agent's reflect. The URL names the BOUND host — a
            // host bound to a LAN address does not answer at 127.0.0.1.
            let line = format!(
                "listening on http://{}:{}/ · rest/ws are children (reflect kernel for ids)",
                b.host, b.port
            );
            let _ack = b
                .kernel
                .send(vec![Item::new(
                    AgentId::from("cli"),
                    json!({"type": "say", "source": "web", "text": line}),
                )
                .with_reply_to(fantastic_kernel::ReplyTo::Row)])
                .await;
            let surfaces = b.table.read().await.child_ids();
            return b.end(Ok(json!({"running": true, "surfaces": surfaces})));
        };
        let (kernel, table) = (Arc::clone(&b.kernel), Arc::clone(&b.table));
        mount_surface(table, &kernel, child, move |r| async move {
            match r {
                // Refuse BEFORE the listener binds: no shadowed route ever serves.
                Err(reason) => b.end(Err(reason)),
                Ok(()) => mount_all(b).await,
            }
        })
        .await;
    })
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
            // artefact, not reproduced.
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
    let task = fantastic_kernel::spawn_inherit(async move {
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

/// python `_fetch_file` — one `read_stream` chunk of `path` on `agent_id`, sent
/// from this host's own dispatch, so the read is THE HOST's: the served agent's
/// `web`'s own lines and the served agent's io leg judge it. The reply comes back whole, its raw
/// chunk at its slot. ASYNC (#795): the host's call defers its settle and the read
/// goes out through [`Kernel::call`]; the read's settle settles it. `None`: deferred.
async fn fetch_file_reply(
    a: verbs::FetchFile,
    kernel: &Arc<Kernel>,
) -> Option<(Value, Option<fantastic_kernel::Blob>)> {
    let d = match kernel.defer() {
        Ok(d) => d,
        Err(refused) => return Some((refused, None)),
    };
    let item = Item::new(
        AgentId::from(a.agent_id.as_str()),
        json!({
            "type": "read_stream",
            "path": a.path,
            "offset": a.offset,
            "length": a.length,
        }),
    );
    let k = Arc::clone(kernel);
    let ack = kernel
        .call(item, move |out| async move {
            k.settle_deferred(d.pid(), as_own_reply(out));
        })
        .await;
    if ack.status != fantastic_kernel::Status::Accepted {
        kernel.settle_deferred(d.pid(), as_own_reply(ack));
    }
    None
}

/// Another call's outcome as this handler's own reply: its result and body, settled
/// as a handler's reply settles (`errored` when refusal-shaped).
fn as_own_reply(out: fantastic_kernel::Outcome) -> fantastic_kernel::Outcome {
    let status = if fantastic_kernel::is_refused(&out.result) {
        fantastic_kernel::Status::Errored
    } else {
        fantastic_kernel::Status::Completed
    };
    fantastic_kernel::Outcome::new(out.pid, status, out.result).with_body(out.body)
}

/// python `_mount` — pull `get_routes` from the named child and (re)mount it. The
/// `get_routes` is a call (#795): the verb defers, and its settle mounts the child.
async fn mount_reply(agent_id: &AgentId, child_id: &str, kernel: &Arc<Kernel>) -> Value {
    let child = AgentId::from(child_id);
    let Some(table) = SERVERS.lock().get(agent_id).map(|h| Arc::clone(&h.table)) else {
        return json!({"error": "web.mount: not running"});
    };
    let d = match kernel.defer() {
        Ok(d) => d,
        Err(refused) => return refused,
    };
    table.write().await.unmount(&child);
    let k = Arc::clone(kernel);
    let t = Arc::clone(&table);
    mount_surface(table, kernel, child.clone(), move |r| async move {
        let reply = match r {
            Err(reason) => json!({"error": reason}),
            Ok(()) => json!({"mounted": t.read().await.paths_for(&child)}),
        };
        k.settle_deferred(d.pid(), fantastic_kernel::Outcome::of_reply(Some(d.pid()), reply));
    })
    .await;
    Value::Null
}

/// python `_unmount` — remove every route belonging to the named child.
async fn unmount_reply(agent_id: &AgentId, child_id: &str) -> Value {
    let child = AgentId::from(child_id);
    let Some(table) = SERVERS.lock().get(agent_id).map(|h| Arc::clone(&h.table)) else {
        return json!({"error": "web.unmount: not running"});
    };
    table.write().await.unmount(&child);
    json!({"unmounted": child.0})
}

/// python `_mount_surface` — ask one child for its routes and mount them. A
/// child that does not answer with a route list mounts nothing. The ask is a call
/// (#795): `then` gets the mount's result once it settled.
async fn mount_surface<F, Fut>(
    table: Arc<AsyncRwLock<RouteTable>>,
    kernel: &Arc<Kernel>,
    child: AgentId,
    then: F,
) where
    F: FnOnce(Result<(), String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let item = Item::new(child.clone(), json!({"type": "get_routes"}));
    kernel
        .call_then(item, move |out| async move {
            let mounted = routes::routes_from_spec(&child, &out.result);
            if mounted.is_empty() {
                return then(Ok(())).await;
            }
            let r = {
                let mut table = table.write().await;
                match table.conflict(&child, &mounted) {
                    Some(reason) => Err(reason),
                    None => {
                        table.mount(child.clone(), mounted);
                        Ok(())
                    }
                }
            };
            then(r).await;
        })
        .await;
}

/// The record's `port` (declared `u16`, at least 1); absent means not set.
fn read_port(agent_id: &AgentId, kernel: &Kernel) -> Option<u16> {
    let agent = kernel.resolve_target(agent_id.as_str())?;
    let meta = agent.record().meta;
    meta.get("port")?.as_u64().and_then(|n| u16::try_from(n).ok())
}

/// The agent's record meta (python `kernel.get(id) or {}`).
pub(crate) fn read_rec(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    kernel.resolve_target(agent_id.as_str())
        .map(|e| e.record().meta.clone())
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
        let inherited = state.inherited.clone();
        Box::pin(async move {
            inherited
                .scope(async move {
                    // python `_OriginGuard`: in front of EVERY route, the record read per request.
                    let rec = read_rec(&state.web_agent_id, &state.kernel);
                    if let Some(refused) = origin::guard(state.web_agent_id.as_str(), &rec, &req) {
                        return Ok(refused);
                    }
                    Ok(handlers::dispatch(state, table, req).await)
                })
                .await
        })
    }
}

#[cfg(test)]
mod tests;
