//! fantastic-bridge — cross-kernel comms. TWO io_bridge derivations:
//! `ws_bridge` (ws/memory) + `relay_connector` (relay-kernel router),
//! sharing one engine.
//!
//! Pairs of bridge agents forward `send` envelopes between kernels
//! over a pluggable transport. Weak binding: remote agents are
//! addressed by URL + path only; no shared Rust types across kernels.
//!
//! WS-only; the dialer owns the link (matches the canonical Python kernel). Transports:
//! - `memory`  — in-process paired channels (tests only)
//! - `ws`      — `tokio-tungstenite` client against a remote `web_ws` surface
//!   (`host` explicit). The bridge spawns nothing: an ssh tunnel is an
//!   `exec_bridge` job or devops, and the bridge dials its local end.
//!
//! (The HTTP/`web_rest` transport was removed — WS subsumes its
//! request/reply semantic and adds streaming via watch/event.)
//!
//! # Verbs
//!
//! | verb | payload | reply |
//! |---|---|---|
//! | `reflect` | none | `{id, sentence, transport, connected, host?, port?, peer_id?, ingress, egress, auth, pending_count, verbs, emits}` |
//! | `boot` | none | `{booted, transport}` or `{error, already}` |
//! | `shutdown` | none | `{stopped:true}` (runs the `on_delete` cascade) |
//! | `reconnect` | none | shutdown + boot (the manual redial; a dropped `ws` link redials by itself) |
//! | `forward` | `{target, payload, forward_timeout}` | the unwrapped reply from the remote |
//! | `watch_remote` | `{target}` | `{ok, watching}` — stream a remote agent's emits onto this bridge's inbox |
//! | `unwatch_remote` | `{target}` | `{ok, unwatched}` |
//!
//! # Frame protocol (the dialer owns the link; calls and watches go both ways)
//!
//! Ships **raw** call frames to the remote's `web_ws`, which dispatches
//! `kernel.send` exactly like a browser frame. No peer bridge needed.
//! Matches Python ws_bridge + fantastic-web WS server:
//!
//! ```text
//! outbound  {type:"hello", guid, leg}   — `ws_bridge` only, first, at every boot
//!                                        (`reconnect` included), when this kernel
//!                                        has a guid: the remote then reaches this
//!                                        kernel's agents as `<guid>|<agent>`
//! outbound  {type:"call",  id:corr, target, payload}
//!                                        — the far kernel judges it as the leg
//!                                        that serves it there
//! outbound  {type:"watch", src:target}  / {type:"unwatch", src:target}
//! inbound   {type:"reply", id, data, secret?} — read loop routes to pending oneshot
//!                                        (`secret`: dotted paths into `data`, noted
//!                                        as the forward's reply marks)
//! inbound   {type:"error", id, error}    — read loop fails the pending oneshot
//! inbound   {type:"event", src, payload} — re-emitted (payload + `src`) on this bridge's
//!                                        inbox ONLY for a subscribed `src`, else dropped
//! inbound   {type:"call", id, target, payload} — served AS THE LEG, answered
//!                                        `{type:"reply", id, data, secret?}`
//! inbound   {type:"watch"|"unwatch", src} — the remote watching a local agent: the
//!                                        leg's ingress rule, then the leg's own
//!                                        `watch` grant on `src`; each event of
//!                                        `src` goes back `{type:"event", src,
//!                                        payload}` until `unwatch` or the link
//!                                        drops (a refusal is `{type:"error", src, …}`)
//! ```
//!
//! Reply correlation: a frame id is `<bridge id>-<boot nonce>-<counter>` — the
//! bridge id keeps bridges apart, the nonce (minted at every boot) keeps BOOTS
//! apart. The frame id NAMES the execution on the receiver, so an id reused by a
//! restarted sender would join a settled execution there and never run.
//!
//! A leg's rules (`ingress_rule`/`egress_rule`/`auth`) are read off the LIVE
//! record per frame — never cached at boot — so what `reflect` reports is what the
//! gate enforces the moment `update_agent` writes it.
//!
//! IDENTITY: an inbound `call` frame is served by the read loop — a task this
//! leg's own `boot` spawned — so it is sent AS THE LEG and gated as the leg (its
//! `can_call`, the target's `callable_by`). Whoever called on the other kernel is
//! that kernel's business; here the caller is the leg.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{py_float_repr, py_repr, py_str, py_truthy};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::refusal::{refusal_hint, NOT_CONNECTED};
use fantastic_kernel::{AgentId, Blob, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

// The authorization rule registries live in the shared `fantastic-io-bridge` base
// (every io derivation imports them).
use fantastic_io_bridge::authorizer;
pub mod record;
pub mod transport;
pub mod verbs;

use authorizer::{Action, Decision};
use transport::memory::MemoryTransport;
use verbs::CallFrame;
use transport::relay::{RelaySpawn, RelayTransport};
use transport::ws::WsTransport;
use transport::{BridgeTransport, TransportError};

/// `handler_module` of the WS derivation (ws / memory transports).
pub const WS_HANDLER_MODULE: &str = "ws_bridge.tools";
/// `handler_module` of the RELAY derivation (the relay-kernel router transport).
pub const RELAY_HANDLER_MODULE: &str = "relay_connector.tools";

/// Which transport family a bundle admits — the only behavioural difference
/// between the two derivations (they share one engine, mirroring py's
/// io_bridge engine + thin ws_bridge / relay_connector derivations).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// ws / memory (the `ws_bridge` derivation).
    Ws,
    /// the relay-kernel router (`relay_connector` derivation).
    Relay,
}

impl Family {
    /// python's `SENTENCE` for this derivation.
    fn sentence(self) -> &'static str {
        match self {
            Family::Ws => "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); weak proxy.",
            Family::Relay => "Cross-kernel comms through a relay-kernel router — dial-out WS, group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC tunneled over the relay; auto-reconnecting.",
        }
    }
}

/// python `relay_connector` fixes the directory watch timeout at 10 s.
const DIRECTORY_WATCH_TIMEOUT_SECS: f64 = 10.0;

/// Live bridge state, keyed by bridge-agent id.
static BRIDGES: OnceLockBridgeMap = OnceLockBridgeMap::new();

// ── once-lock helpers ───────────────────────────────────────────────

struct OnceLockBridgeMap(OnceLock<Mutex<HashMap<AgentId, Arc<BridgeState>>>>);
impl OnceLockBridgeMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Arc<BridgeState>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("BRIDGES poisoned")
    }
}

/// A pending forward's reply channel — carries `(reply, body)` (the body is the
/// reply frame's raw chunk AT ITS SLOT of the reply — `["bytes"]` for a
/// `read_stream` forward, `["x.y"]` for a peer that answered at the key `"x.y"`
/// — `None` for a text reply), plus the reply frame's `secret` marks (dotted
/// paths into the reply, empty when the frame named none), or an error string.
type PendingReply = oneshot::Sender<Result<(Value, Option<Blob>, Vec<String>), String>>;

/// Per-agent bridge runtime. Cloneable through an `Arc` so the read
/// loop can hold one reference and the verb handlers can hold
/// another without lifetime gymnastics.
///
/// The fields are intentionally `pub(crate)` — outside callers
/// interact exclusively via verbs.
pub(crate) struct BridgeState {
    pub(crate) transport: Arc<dyn BridgeTransport>,
    pub(crate) transport_kind: String,
    pub(crate) read_task: AsyncMutex<Option<fantastic_kernel::TaskId>>,
    pub(crate) pending: Mutex<HashMap<String, PendingReply>>,
    /// `<bridge id>-<nonce>`, minted at every boot ([`new_corr_prefix`]), and the
    /// count under it: the frame ids of this boot's forwards.
    pub(crate) corr_prefix: String,
    pub(crate) corr_counter: AtomicU64,
    /// One `bridge_down` per boot: whoever tears the leg down first (the read
    /// loop's own exit, or a teardown that cancels it) emits it.
    pub(crate) down_emitted: std::sync::atomic::AtomicBool,
    /// The remote agents this leg SUBSCRIBED to (`watch_remote`). An inbound
    /// `event` frame is re-emitted only when its `src` is here — nothing flows
    /// until subscribed, and nothing after an unwatch.
    pub(crate) watching: Mutex<HashSet<String>>,
    /// The LOCAL agents the far side watches through this leg (an inbound
    /// `watch` frame), by the `src` it named — each with its reader and the
    /// drain that streams its events back. Dropped on `unwatch` and with the
    /// link.
    pub(crate) served: Mutex<HashMap<String, Served>>,
}

/// One local agent the far side watches through the leg.
pub(crate) struct Served {
    /// The agent watched.
    target: AgentId,
    /// The watch's own reader (`<leg>><agent>`).
    reader: AgentId,
    /// The reader's receive half: the drain forwards under its lock, and a reply
    /// flushes it first (see [`serve_inbound_call`]).
    rx: Arc<AsyncMutex<fantastic_kernel::inbox::InboxReceiver>>,
    /// The drain task, a `ps` row of the leg.
    task: fantastic_kernel::TaskId,
}

impl BridgeState {
    fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending poisoned").len()
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The WS derivation of io_bridge — ws / memory transports. Registered
/// under `ws_bridge.tools`. (Was the combined `kernel_bridge`; cloud split out.)
pub struct WsBridgeBundle;
/// The RELAY derivation of io_bridge — the relay-kernel router transport.
/// Registered under `relay_connector.tools`. Shares the engine with `WsBridgeBundle`.
pub struct RelayConnectorBundle;

/// The relay directory verbs (`relay_connector` only) — addressed to the relay's
/// own `relay` agent (`target:"relay"`), not the partner. `list_peers` is a
/// one-shot snapshot; `watch_directory` subscribes so `peer_*` events re-emit on
/// this connector's inbox; `unwatch_directory` stops it.
enum DirectoryOp {
    /// `list_peers`, with the seconds to wait for the relay's answer.
    ListPeers(f64),
    Watch,
    Unwatch,
}

impl DirectoryOp {
    fn name(&self) -> &'static str {
        match self {
            DirectoryOp::ListPeers(_) => "list_peers",
            DirectoryOp::Watch => "watch_directory",
            DirectoryOp::Unwatch => "unwatch_directory",
        }
    }
}

async fn directory_reply(agent_id: &AgentId, op: DirectoryOp) -> Result<Value, BundleError> {
    let state = BRIDGES.lock().get(agent_id).cloned();
    let live = state.as_ref().filter(|s| !s.transport.closed());
    let Some(state) = live else {
        // python: `unwatch_directory` on a leg with no transport is simply `ok`.
        if let DirectoryOp::Unwatch = op {
            return Ok(json!({"ok": true}));
        }
        return Ok(refusal_hint(
            NOT_CONNECTED,
            format!("relay_connector.{}: not connected", op.name()),
            "call boot first",
        ));
    };
    Ok(match op {
        DirectoryOp::ListPeers(timeout) => state.transport.list_peers(timeout).await,
        // python passes NO timeout here — the transport's own 10 s stands.
        DirectoryOp::Watch => {
            // The leg SUBSCRIBES to the directory as `watch_remote` does to a
            // remote agent — before asking: the read loop may hand the engine an
            // event that follows the relay's ack before this verb resumes. A
            // refusal withdraws it.
            let src = transport::relay::DIRECTORY_SRC;
            state
                .watching
                .lock()
                .expect("watching poisoned")
                .insert(src.to_string());
            let out = state
                .transport
                .watch_directory(DIRECTORY_WATCH_TIMEOUT_SECS)
                .await;
            if out.get("error").is_some_and(py_truthy) {
                state
                    .watching
                    .lock()
                    .expect("watching poisoned")
                    .remove(src);
            }
            out
        }
        DirectoryOp::Unwatch => {
            state
                .watching
                .lock()
                .expect("watching poisoned")
                .remove(transport::relay::DIRECTORY_SRC);
            state.transport.unwatch_directory().await
        }
    })
}

/// `set_identity` (`relay_connector` only): advertise/update this peer's directory
/// typing — `role`/`owner_guid`/`exposes` (all optional, merged over the record).
/// Persists the change so it re-announces on the next boot, then pushes it to the
/// relay live (→ `peer_updated`). Mirrors py `_set_identity`.
async fn set_identity_reply(
    agent_id: &AgentId,
    a: verbs::SetIdentity,
    kernel: &Arc<Kernel>,
) -> Value {
    let Some(state) = BRIDGES
        .lock()
        .get(agent_id)
        .cloned()
        .filter(|s| !s.transport.closed())
    else {
        return refusal_hint(
            NOT_CONNECTED,
            "relay_connector.set_identity: not connected",
            "call boot first",
        );
    };
    // Merge only the provided well-known keys into the record + persist (explicit
    // null retracts a field, e.g. owner_guid:null = become standalone).
    // python `kernel.update(id, **patch)` — the `updated` state event is what
    // the root loader's writer persists.
    let mut patch = serde_json::Map::new();
    if let Some(role) = a.role {
        patch.insert("role".into(), json!(role));
    }
    if let Some(owner) = a.owner_guid {
        patch.insert("owner_guid".into(), json!(owner));
    }
    if let Some(exposes) = a.exposes {
        patch.insert("exposes".into(), json!(exposes));
    }
    if !patch.is_empty() {
        kernel.update(agent_id, patch);
    }
    let attrs = identity_from_meta(agent_id, kernel);
    state.transport.set_identity(attrs).await
}

/// `forward` — the one verb that carries bytes either way: the local caller sends
/// `{type:forward, target, payload, forward_timeout}`, a request body (if any)
/// inside the INNER payload (`Item::with_binary(["payload", …], …)` — python's
/// forward payload holds the bytes there); the bridge ships a `call` frame (binary
/// when it carries that body, text otherwise) and returns `(reply, reply_body)` —
/// the reply body at its slot of the reply (`["bytes"]` for a `read_stream`
/// forward, the peer's own key otherwise), whenever the peer answered one,
/// whatever verb it was. A body anywhere else is not part of the forwarded call,
/// exactly as python's inner payload would not hold it.
async fn forward_call(
    agent_id: &AgentId,
    a: verbs::Forward,
    body: Option<Blob>,
    kernel: &Arc<Kernel>,
) -> (Value, Option<Blob>) {
    // A body INSIDE the forwarded `payload` — its path below the step `payload`.
    // One bound to the literal top-level key `payload.x` (the REST octet leg) is
    // a different key, never inside it: python's `payload[field] = body` puts it
    // beside `payload`, not in it.
    let carried = body.and_then(|b| b.under("payload"));
    forward_core(
        kernel,
        agent_id,
        &a.target,
        Value::Object(a.payload),
        carried,
        a.forward_timeout,
    )
    .await
}

/// The secret paths of a FORWARDED call: a `forward` of a `set` marks each
/// `payload.fields.<name>` (a name that is empty or holds a dot marks the whole
/// `payload.fields`, as the credential store's own `field_marks` does). The
/// caller cannot know the far bundle before sending, so this is deliberate
/// over-redaction in the CALLER's records for every forwarded `set`: the far
/// kernel's own record is exact. A `get`'s reply marks come back on the reply
/// frame instead. The page-peer `<guid>|agent` route (web `ws_peer`) is not
/// covered; it only reaches browser kernels, which hold no credential store.
fn forward_payload_secret(payload: &Value) -> Vec<String> {
    if payload.get("type").and_then(Value::as_str) != Some("forward") {
        return Vec::new();
    }
    let Some(inner) = payload
        .get("payload")
        .filter(|p| p.get("type") == Some(&json!("set")))
    else {
        return Vec::new();
    };
    match inner.get("fields") {
        Some(Value::Object(m)) if m.keys().all(|k| !k.is_empty() && !k.contains('.')) => {
            m.keys().map(|k| format!("payload.fields.{k}")).collect()
        }
        Some(Value::Object(_)) => vec!["payload.fields".to_string()],
        _ => Vec::new(),
    }
}

/// What the bundle is for — its one short man text (ws_bridge).
const WS_MAN: &str = "A cross-kernel bridge: it dials a far kernel's web_ws leg and forwards calls to it, watches its agents and serves the far side's calls and watches as this leg, gated as the leg. Emits bridge_up and bridge_down on its inbox.";

/// What the bundle is for — its one short man text (relay_connector).
const RELAY_MAN: &str = "A cross-kernel bridge through a relay kernel: it dials the relay with a group password from a credential store, forwards calls to a partner kernel by guid, serves the partner's calls as this leg, keeps a directory of peers, and redials by itself. Emits bridge_up and bridge_down on its inbox.";

#[async_trait]
impl Handler for WsBridgeBundle {
    type Call = verbs::ws::Call;

    fn name(&self) -> &str {
        "ws_bridge"
    }

    fn man(&self) -> &'static str {
        WS_MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::ws::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::ws::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::ws::RECORD
    }

    fn payload_secret(&self, payload: &Value) -> Vec<String> {
        forward_payload_secret(payload)
    }

    /// The JSON half of [`Self::handle_binary`] — a `forward` reply body is not carried.
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(self.handle_binary(agent_id, call, None, kernel).await?.0)
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        let family = Family::Ws;
        let reply = match call {
            verbs::ws::Call::Reflect(_) => reflect_reply(family, agent_id, kernel),
            verbs::ws::Call::Boot(_) => boot_reply(family, agent_id, kernel).await,
            verbs::ws::Call::Reconnect(_) => reconnect_reply(family, agent_id, kernel).await,
            verbs::ws::Call::Forward(a) => {
                let (v, b) = forward_call(agent_id, a, body, kernel).await;
                return Ok((Some(v), b));
            }
            verbs::ws::Call::WatchRemote(a) => {
                watch_remote_reply(agent_id, &a.target, "watch").await
            }
            verbs::ws::Call::UnwatchRemote(a) => {
                watch_remote_reply(agent_id, &a.target, "unwatch").await
            }
        };
        Ok((Some(reply), None))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id, kernel).await;
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
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        validate_bridge_meta(meta)
    }
}

#[async_trait]
impl Handler for RelayConnectorBundle {
    type Call = verbs::relay::Call;

    fn name(&self) -> &str {
        "relay_connector"
    }

    fn man(&self) -> &'static str {
        RELAY_MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::relay::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::relay::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::relay::RECORD
    }

    fn payload_secret(&self, payload: &Value) -> Vec<String> {
        forward_payload_secret(payload)
    }

    /// The JSON half of [`Self::handle_binary`] — a `forward` reply body is not carried.
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(self.handle_binary(agent_id, call, None, kernel).await?.0)
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        let family = Family::Relay;
        let reply = match call {
            verbs::relay::Call::Reflect(_) => reflect_reply(family, agent_id, kernel),
            verbs::relay::Call::Boot(_) => boot_reply(family, agent_id, kernel).await,
            verbs::relay::Call::Reconnect(_) => reconnect_reply(family, agent_id, kernel).await,
            verbs::relay::Call::Forward(a) => {
                let (v, b) = forward_call(agent_id, a, body, kernel).await;
                return Ok((Some(v), b));
            }
            verbs::relay::Call::WatchRemote(a) => {
                watch_remote_reply(agent_id, &a.target, "watch").await
            }
            verbs::relay::Call::UnwatchRemote(a) => {
                watch_remote_reply(agent_id, &a.target, "unwatch").await
            }
            verbs::relay::Call::ListPeers(a) => {
                directory_reply(agent_id, DirectoryOp::ListPeers(a.list_timeout)).await?
            }
            verbs::relay::Call::WatchDirectory(_) => {
                directory_reply(agent_id, DirectoryOp::Watch).await?
            }
            verbs::relay::Call::UnwatchDirectory(_) => {
                directory_reply(agent_id, DirectoryOp::Unwatch).await?
            }
            verbs::relay::Call::SetIdentity(a) => set_identity_reply(agent_id, a, kernel).await,
        };
        Ok((Some(reply), None))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id, kernel).await;
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
    /// The leg's `ingress_rule`, refused at the WRITE when no rule implements it.
    /// Python `relay_connector.tools.validate_meta`.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        validate_bridge_meta(meta)
    }
}

// ── the relay password (a credential_store ref, never a record field) ──

/// The credential field that holds the relay group password. Python `FIELD`.
pub const RELAY_PASSWORD_FIELD: &str = "password";

/// The async `read() -> password` a dial awaits — every dial, nothing cached.
/// Asks the store named by the record's `credentials` for the ref's `password`,
/// pinned to the relay URL's host (the store refuses a ref bound elsewhere). The
/// dial runs in this connector's dispatch (`boot`) or a task it spawned (the
/// reconnect supervisor), so the read is the connector's own send, gated as it.
/// `Err` names the store and the ref on any refusal or a missing field
/// (python's `CredentialError`). Python `password_reader`.
fn password_reader(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    relay_url: &str,
) -> transport::relay::PasswordReader {
    // The shared read: the store read, its host pin and its texts live in ai_core.
    let reader =
        fantastic_ai_core::credentials::key_reader(self_id, kernel, "relay_connector", true);
    let url = relay_url.to_string();
    Arc::new(move || {
        let (reader, url) = (reader.clone(), url.clone());
        Box::pin(async move {
            let values = reader
                .read_fields(&url, &[RELAY_PASSWORD_FIELD])
                .await
                .map_err(|e| e.0)?;
            values
                .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0)))
                .ok_or_else(|| "relay_connector: no `credentials`".to_string())
        })
    })
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `<bridge id>-<12 hex>`: a fresh nonce per BOOT (python `_new_corr_prefix`,
/// `secrets.token_hex(6)`; ts's `mintPrefix` width). The counter under it
/// restarts at every boot, so without the nonce a restarted sender's `<leg>-1`
/// would name an execution the receiver had already settled — the new call
/// joined to it and never run. A failure is FATAL — no second source.
pub(crate) fn new_corr_prefix(id: &AgentId) -> String {
    let mut b = [0u8; 6];
    fantastic_kernel::lifecycle::fill_random(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{id}-{hex}")
}

/// Mint an opaque relay GUID (32 hex) — python `relay_connector._relay`'s
/// `uuid.uuid4().hex`: 16 bytes from the OS CSPRNG carrying uuid4's version
/// nibble and the RFC-4122 variant bits. A failure is FATAL — there is no
/// second source (the time+pid+counter mixer this replaced was not random).
fn mint_guid() -> String {
    let mut b = [0u8; 16];
    fantastic_kernel::lifecycle::fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Assemble the directory-attributes blob to advertise from the record's well-known
/// keys (`role`/`owner_guid`/`exposes`). Defaults are OMITTED (role=kernel /
/// owner_guid=null / exposes=[]) so a plain peer advertises nothing — the relay's
/// own defaults stand. Opaque to the relay; mirrors py `_identity_from_record`.
fn identity_from_meta(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let mut attrs = serde_json::Map::new();
    if let Some(role) = meta_string(agent_id, kernel, "role") {
        if role != "kernel" {
            attrs.insert("role".into(), Value::String(role));
        }
    }
    if let Some(owner) = meta_value(agent_id, kernel, "owner_guid") {
        if !owner.is_null() {
            attrs.insert("owner_guid".into(), owner);
        }
    }
    if let Some(exposes) = meta_value(agent_id, kernel, "exposes") {
        if exposes.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            attrs.insert("exposes".into(), exposes);
        }
    }
    Value::Object(attrs)
}

fn meta_f64(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<f64> {
    meta_value(agent_id, kernel, key).and_then(|v| v.as_f64())
}

fn meta_value(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<Value> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).cloned()
}

/// The rule spec for one direction: the per-direction field if present, else the
/// `auth` shorthand — which carries a bare STEM that each manifold scopes with its own
/// suffix (`auth:"password"` ⇒ `password_ingress` + `password_egress`). See
/// `authorizer::manifold_spec`.
fn rule_spec(agent_id: &AgentId, kernel: &Kernel, primary: &str) -> Option<Value> {
    let suffix = if primary == "egress_rule" {
        "_egress"
    } else {
        "_ingress"
    };
    authorizer::manifold_spec(
        meta_value(agent_id, kernel, primary).as_ref(),
        meta_value(agent_id, kernel, "auth").as_ref(),
        suffix,
    )
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(family: Family, agent_id: &AgentId, kernel: &Kernel) -> Value {
    let bridge = BRIDGES.lock().get(agent_id).cloned();
    // the live transport's kind, else the record's `transport` as stored
    let transport_kind = match bridge.as_ref() {
        Some(b) => json!(b.transport_kind),
        None => meta_value(agent_id, kernel, "transport").unwrap_or(Value::Null),
    };
    // python's engine: the transport EXISTS and is not closed. The relay leg
    // overrides it in `reflect_fields` with the LIVE socket state.
    let connected = match bridge.as_ref() {
        None => false,
        Some(b) => match family {
            Family::Relay => b.transport.is_live(),
            Family::Ws => !b.transport.closed(),
        },
    };
    let pending = bridge.as_ref().map(|b| b.pending_count()).unwrap_or(0);
    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(agent_id.as_str()));
    out.insert("sentence".into(), json!(family.sentence()));
    out.insert("transport".into(), json!(transport_kind));
    out.insert("connected".into(), json!(connected));
    out.insert("pending_count".into(), json!(pending));
    // Read-key == write-key (py parity): `ingress_rule`/`egress_rule`, so a
    // `reflect` → `update_agent` edit is a direct mirror. No `auth` in the
    // reflect (it's a WRITE shorthand only). `sealed` = the leg still
    // denies inbound (the conscious-open signal). The SAME `describe` every leg
    // reflects with (python `**describe(rec)`) — the gate's own scoping, so a
    // bridge never reports a rule its gate resolves differently.
    let rec = kernel
        .agents
        .get(agent_id)
        .map(|e| e.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default();
    if let Some(posture) = authorizer::describe(&rec).as_object() {
        for (k, v) in posture {
            out.insert(k.clone(), v.clone());
        }
    }
    // The family's own `reflect_fields` — python's `out.update(...)`, so these
    // land after the shared block, in their own order.
    match family {
        Family::Ws => {
            for key in ["host", "peer_id", "port"] {
                out.insert(
                    key.into(),
                    meta_value(agent_id, kernel, key).unwrap_or(Value::Null),
                );
            }
        }
        Family::Relay => {
            for key in ["relay_url", "guid", "partner_guid", "reconnect", "role"] {
                out.insert(
                    key.into(),
                    meta_value(agent_id, kernel, key).unwrap_or(Value::Null),
                );
            }
            for key in ["owner_guid", "exposes"] {
                out.insert(
                    key.into(),
                    meta_value(agent_id, kernel, key).unwrap_or(Value::Null),
                );
            }
        }
    }
    Value::Object(out)
}

async fn boot_reply(family: Family, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    boot_carrying(family, agent_id, kernel, HashSet::new()).await
}

/// `boot`, with the remote agents a dropped link was watching: the new link asks
/// the far side for each of them again, so a watcher of this leg keeps receiving
/// without subscribing anew. A plain `boot` carries none.
///
/// Boxed: the read loop `boot` spawns redials through this very function, and a
/// boxed `Send` future breaks the cycle the compiler would otherwise chase.
fn boot_carrying<'a>(
    family: Family,
    agent_id: &'a AgentId,
    kernel: &'a Arc<Kernel>,
    carried: HashSet<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + 'a>> {
    Box::pin(boot_carrying_inner(family, agent_id, kernel, carried))
}

async fn boot_carrying_inner(
    family: Family,
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    carried: HashSet<String>,
) -> Value {
    // python: idempotent only while the transport is OPEN — a closed leg is
    // rebuilt in place.
    if let Some(existing) = BRIDGES.lock().get(agent_id).cloned() {
        if !existing.transport.closed() {
            return json!({"already": true, "transport": existing.transport_kind});
        }
    }
    // The leg's rules are NOT cached here: the gate and `forward` read them off
    // the live record per frame. Boot only refuses an egress rule the registry
    // does not know, before dialing.
    if let Err(e) = authorizer::egress::resolve(rule_spec(agent_id, kernel, "egress_rule").as_ref())
    {
        return json!({"error": format!("bridge: bad auth rule: {e}")});
    }

    // The record declares its `transport` (required); the builder is chosen by
    // the derivation, not by the kind.
    let Some(kind) = meta_string(agent_id, kernel, "transport") else {
        return json!({"error": "bridge: the record has no `transport`"});
    };
    let transport: Arc<dyn BridgeTransport> = if kind == "memory" {
        match take_injected(agent_id) {
            Some(t) => t,
            None => return json!({"error": "bridge: memory transport requires test injection"}),
        }
    } else {
        match build_transport(family, agent_id, kernel).await {
            Ok(t) => t,
            Err(e) => return json!({"error": format!("bridge: {kind} boot failed: {e}")}),
        }
    };

    let state = Arc::new(BridgeState {
        transport: Arc::clone(&transport),
        transport_kind: kind.clone(),
        read_task: AsyncMutex::new(None),
        pending: Mutex::new(HashMap::new()),
        // this boot's correlator nonce; the count restarts under it
        corr_prefix: new_corr_prefix(agent_id),
        corr_counter: AtomicU64::new(0),
        down_emitted: std::sync::atomic::AtomicBool::new(false),
        watching: Mutex::new(carried.clone()),
        served: Mutex::new(HashMap::new()),
    });

    // HELLO FIRST: the far `web_ws` registers this connection as this kernel's
    // leg, so its agents reach ours as `<guid>|<agent>` — at every boot, so a
    // redial (`reconnect`) replaces the dropped connection there. A kernel with
    // no guid yet has no address to give and says nothing.
    if family == Family::Ws {
        if let Some(guid) = own_guid(kernel) {
            let hello = json!({"type": "hello", "guid": guid, "leg": agent_id.as_str()});
            if let Err(e) = transport.send_frame(hello).await {
                transport.close().await;
                return json!({"error": format!("bridge: {kind} boot failed: hello: {}", transport_error_text(&e))});
            }
        }
    }

    // Spawn the read loop BEFORE we publish the state so the
    // very first inbound frame can land.
    let read_state = Arc::clone(&state);
    let read_agent = agent_id.clone();
    let read_kernel = Arc::clone(kernel);
    let read_family = family;
    // REGISTERED, AND UNDER THE LEG'S OWN IDENTITY. The read loop outlives the
    // `boot` dispatch and issues a send per inbound frame, so it is real work:
    // `spawn_task` gives it a `ps` row and a `cancel` that reaches it, and
    // carries this dispatch's sender into the fork — without it an inbound `call`
    // would reach `Kernel::launch` with `current_sender() == None`, the OPERATOR,
    // and the two-sided gate would be skipped for every frame a peer sends.
    let task_id = kernel.spawn_task(&agent_id.clone(), "read_loop", async move {
        read_loop(read_agent, read_state, read_kernel, read_family).await;
    });
    *state.read_task.lock().await = Some(task_id);

    BRIDGES.lock().insert(agent_id.clone(), Arc::clone(&state));

    // The watches the dropped link held, asked for again on this one. A failed
    // send is the link dropping already: the read loop sees it and redials.
    for src in &carried {
        let _ = transport
            .send_frame(json!({"type": "watch", "src": src}))
            .await;
    }

    kernel.emit(agent_id, json!({"type": "bridge_up"})).await;
    json!({"booted": true, "transport": kind})
}

/// python's per-derivation `build_transport(kind, rec, kernel, st)` — the ONLY
/// difference between the two bundles. Its `Err` is python's raised `ValueError`
/// text; the caller wraps it as `bridge: <kind> boot failed: <text>`.
async fn build_transport(
    family: Family,
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<dyn BridgeTransport>, String> {
    match family {
        Family::Ws => build_ws_transport(agent_id, kernel).await,
        Family::Relay => build_relay_transport(agent_id, kernel).await,
    }
}

/// python `ws_bridge._ws.build_transport` — `host` explicit, nothing guessed.
async fn build_ws_transport(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<dyn BridgeTransport>, String> {
    let peer_id = meta_string(agent_id, kernel, "peer_id")
        .ok_or_else(|| "peer_id required for the ws transport".to_string())?;
    let port = meta_value(agent_id, kernel, "port")
        .and_then(|p| p.as_u64())
        .ok_or_else(|| "ws transport requires port".to_string())?;
    let host = meta_string(agent_id, kernel, "host")
        .ok_or_else(|| "ws transport requires host (nothing is guessed)".to_string())?;
    let url = format!("ws://{host}:{port}/{peer_id}/ws");
    WsTransport::connect(&url)
        .await
        .map(|t| t as Arc<dyn BridgeTransport>)
        .map_err(|e| transport_error_text(&e))
}

/// python `relay_connector._relay.build_transport` — validates BEFORE minting
/// the guid, so a refused boot leaves no identity behind.
async fn build_relay_transport(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<dyn BridgeTransport>, String> {
    let relay_url = meta_string(agent_id, kernel, "relay_url");
    let partner = meta_string(agent_id, kernel, "partner_guid");
    let (Some(relay_url), Some(partner)) = (relay_url, partner) else {
        return Err("relay_connector requires relay_url, partner_guid".to_string());
    };
    let (Some(heartbeat), Some(reconnect)) = (
        meta_f64(agent_id, kernel, "heartbeat"),
        meta_f64(agent_id, kernel, "reconnect"),
    ) else {
        return Err("relay_connector requires heartbeat, reconnect".to_string());
    };
    // The relay password lives in a credential_store, named by `credentials`:
    // no field ⇒ the boot fails loud, before any identity is minted or anything
    // dialed.
    if meta_value(agent_id, kernel, "credentials").is_none_or(|v| v.is_null()) {
        return Err(format!(
            "relay_connector: no `credentials` on {agent_id} — name the relay password's \
             credential store and ref (update_agent {agent_id} credentials={{store: <id>, \
             ref: <ref>}}; the ref's field `password`, bound to the relay's host)"
        ));
    }
    // `guid` (our WS path) is auto-minted ONCE on first boot if absent and
    // persisted into the record, so every later hydration re-dials the SAME path;
    // an explicit guid always wins, and a minted one is never regenerated.
    let guid = match meta_string(agent_id, kernel, "guid") {
        Some(guid) => guid,
        None => {
            let g = mint_guid();
            // python `kernel.update(rec["id"], guid=guid)` — persisted by the
            // root loader's writer off the `updated` event.
            let mut patch = serde_json::Map::new();
            patch.insert("guid".to_string(), Value::String(g.clone()));
            kernel.update(agent_id, patch);
            g
        }
    };
    let identity = identity_from_meta(agent_id, kernel);
    RelayTransport::connect(
        &relay_url,
        &guid,
        password_reader(agent_id, kernel, &relay_url),
        &partner,
        reconnect,
        heartbeat,
        identity,
        RelaySpawn::new(kernel, agent_id),
    )
    .await
    .map(|t| t as Arc<dyn BridgeTransport>)
    .map_err(|e| transport_error_text(&e))
}

/// The bare message a python `ConnectionClosed`/`ValueError` carries — rust's
/// `Display` prefixes the variant, python prints only the text.
fn transport_error_text(e: &TransportError) -> String {
    match e {
        TransportError::ConnectionClosed(m) | TransportError::Other(m) => m.clone(),
    }
}

/// python's read-loop `finally`: ONE `bridge_down` per boot, whether the loop
/// ended by itself or a teardown cancelled it.
async fn emit_bridge_down(agent_id: &AgentId, state: &Arc<BridgeState>, kernel: &Arc<Kernel>) {
    if state
        .down_emitted
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    kernel.emit(agent_id, json!({"type": "bridge_down"})).await;
}

/// This kernel's guid (the root record's), when its loader minted one.
fn own_guid(kernel: &Kernel) -> Option<String> {
    let root = kernel.root()?;
    let meta = root.meta.read().expect("meta poisoned");
    meta.get("guid")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
}

async fn shutdown_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let removed = BRIDGES.lock().remove(agent_id);
    let Some(state) = removed else {
        return json!({"stopped": true, "reason": "not running"});
    };
    // Tear down the read task — cancelled first so the close below doesn't race
    // a frame mid-parse. Through the TABLE, not a raw abort: the task is a `ps`
    // row now, and cancelling it runs the same path an operator's `cancel`
    // would.
    if let Some(tid) = state.read_task.lock().await.take() {
        kernel.cancel_task(&tid).await;
    }
    // the cancelled loop never reaches its tail: the far side's watches go here
    drop_served(&state, kernel).await;
    state.transport.close().await;
    // The cancelled read loop never reaches its own tail — python's `finally`
    // does, so the leg still announces exactly one `bridge_down`.
    emit_bridge_down(agent_id, &state, kernel).await;
    // Reject every in-flight forward — same semantics as the
    // Python branch: callers see a ConnectionError flavour.
    let pending: Vec<PendingReply> = {
        let mut p = state.pending.lock().expect("pending poisoned");
        p.drain().map(|(_k, v)| v).collect()
    };
    for tx in pending {
        let _ = tx.send(Err("bridge shut down".to_string()));
    }
    json!({"stopped": true})
}

async fn reconnect_reply(family: Family, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let _ = shutdown_reply(agent_id, kernel).await;
    boot_reply(family, agent_id, kernel).await
}

/// The shared forward engine — ships a raw `call` frame (text, or binary when
/// `body` carries a request body at its path inside `inner`) to the remote's
/// web_ws and awaits the correlated reply. Returns `(reply, body)`; `body` is
/// `None` when the remote answered a TEXT frame (no chunk at all) and
/// `Some(chunk)` at its slot of the reply when it answered a binary one — empty
/// chunk included. Errors surface as `{error}` in the reply.
async fn forward_core(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
    target: &str,
    mut inner: Value,
    body: Option<Blob>,
    timeout_secs: f64,
) -> (Value, Option<Blob>) {
    let state = match BRIDGES
        .lock()
        .get(agent_id)
        .cloned()
        .filter(|s| !s.transport.closed())
    {
        Some(s) => s,
        None => {
            return (
                refusal_hint(
                    NOT_CONNECTED,
                    "bridge.forward: not connected",
                    "call boot first",
                ),
                None,
            )
        }
    };
    // The leg's EGRESS rule, off the live record per forward (never a boot-time
    // copy): an operator's `update_agent egress_rule=…` stamps the next frame.
    let egress =
        match authorizer::egress::resolve(rule_spec(agent_id, kernel, "egress_rule").as_ref()) {
            Ok(rule) => rule,
            Err(e) => {
                return (
                    json!({"error": format!("bridge: bad auth rule: {e}")}),
                    None,
                )
            }
        };
    let n = state.corr_counter.fetch_add(1, Ordering::SeqCst) + 1;
    // DASH, not colon: `<agent>:<hex>` is the kernel's minted-pid namespace, and
    // a bridge correlator that borrowed it would read as a REFERENCE to an
    // execution this kernel never minted. Same reason a caller-chosen id may not
    // contain one. Fresh per forward AND per boot (the prefix's nonce), so each
    // forward is its own intent.
    let corr = format!("{}-{}", state.corr_prefix, n);

    let (tx, rx) = oneshot::channel::<Result<(Value, Option<Blob>, Vec<String>), String>>();
    state
        .pending
        .lock()
        .expect("pending poisoned")
        .insert(corr.clone(), tx);

    // The body's slot in the inner payload is `null` on the wire, in python's
    // position (`encode_frame` nulls the bytes where they sat); the kernel
    // admitted the path at launch.
    if let Some(b) = &body {
        let _ = b.place(&mut inner, Value::Null);
    }
    // Raw call frame straight to the remote's web_ws — no `forward`
    // envelope, no peer_id addressing (asymmetric; matches the canonical Python
    // kernel). The far kernel judges it as the leg that serves it there.
    let mut frame = json!({
        "type": "call",
        "id": corr,
        "target": target,
        "payload": inner,
    });
    // Stamp this leg's EGRESS credential on the envelope, if its rule presents one
    // (password ⇒ the group token; silent ⇒ None ⇒ no field, wire unchanged). The
    // dispatched `payload` stays clean — the target never sees the token.
    if let Some(token) = egress.credential() {
        frame["auth_token"] = json!(token);
    }
    let send_res = match body {
        // The request body rides raw at `["payload", …its path]` (a python peer
        // reinserts it there; the inner payload's slot stays null on the wire).
        Some(b) => {
            let at = Blob::new(b.nested_in("payload"), b.bytes);
            state.transport.send_binary(frame, at).await
        }
        None => state.transport.send_frame(frame).await,
    };
    if let Err(e) = send_res {
        state
            .pending
            .lock()
            .expect("pending poisoned")
            .remove(&corr);
        return (
            json!({"error": format!("bridge.forward: send failed: {}", transport_error_text(&e))}),
            None,
        );
    }

    // Wait for the reply with a timeout. On EITHER timeout OR
    // close-rejection we clean the pending slot so it doesn't leak.
    let dur = std::time::Duration::from_secs_f64(timeout_secs.max(0.001));
    let err = match tokio::time::timeout(dur, rx).await {
        Ok(Ok(Ok((v, b, marks)))) => {
            // The far store's reply marks, rooted at this reply: the calling
            // kernel records the reply as `[secret]` there, the caller gets it
            // whole (this runs inside the forward dispatch, `note_reply_secret`).
            fantastic_kernel::note_reply_secret(marks);
            return (v, b);
        }
        Ok(Ok(Err(e))) => format!("bridge.forward: {e}"),
        // (the remote-error frame resolves the pending slot with a VALUE — see
        // the read loop — so it returns through the `Ok(Ok(Ok(..)))` arm above)
        Ok(Err(_canceled)) => "bridge.forward: pending dropped".to_string(),
        Err(_elapsed) => format!(
            "bridge.forward: timeout after {}s",
            py_float_repr(timeout_secs)
        ),
    };
    state
        .pending
        .lock()
        .expect("pending poisoned")
        .remove(&corr);
    (json!({ "error": err }), None)
}

/// `watch_remote` / `unwatch_remote`: send `{type:"watch"|"unwatch",
/// src:target}` over the transport. Inbound `event` frames arrive via
/// the read loop and are re-emitted on this bridge agent's inbox.
async fn watch_remote_reply(agent_id: &AgentId, target: &str, kind: &str) -> Value {
    let state = match BRIDGES
        .lock()
        .get(agent_id)
        .cloned()
        .filter(|s| !s.transport.closed())
    {
        Some(s) => s,
        None => {
            return refusal_hint(
                NOT_CONNECTED,
                format!("bridge.{kind}_remote: not connected"),
                "call boot first",
            )
        }
    };
    if kind == "unwatch" {
        state
            .watching
            .lock()
            .expect("watching poisoned")
            .remove(target);
    }
    let frame = json!({ "type": kind, "src": target });
    if let Err(e) = state.transport.send_frame(frame).await {
        return json!({
            "error": format!("bridge.{kind}_remote: send failed: {}", transport_error_text(&e))
        });
    }
    if kind == "watch" {
        state
            .watching
            .lock()
            .expect("watching poisoned")
            .insert(target.to_string());
    }
    let key = if kind == "watch" {
        "watching"
    } else {
        "unwatched"
    };
    json!({ "ok": true, key: target })
}

// ── read loop ───────────────────────────────────────────────────────

/// The TEACHING denial a leg answers an inbound frame with — the rule's own
/// `reason`/`hint`/`see`, never a sentence invented at the call site. Mirrors the
/// python read loop and the web inbound legs, so discovery-through-denial works
/// over a bridge leg exactly as it does over a browser socket.
fn deny_reply(denial: &authorizer::Denial) -> Value {
    let mut out = json!({"error": denial.reason, "reason": "unauthorized"});
    if !denial.hint.is_empty() {
        out["hint"] = json!(denial.hint);
    }
    if !denial.see.is_empty() {
        out["see"] = json!(denial.see);
    }
    out
}

/// Serve ONE inbound `call` frame — `{type:'call', id, target, payload}` — and
/// ship its reply `{type:'reply', id, data}`. Sent AS THE LEG `leg`: this runs in
/// a task the leg's own read loop spawned, so the gate judges the leg.
///
/// Run as its own task (see the read loop): the local handler may itself
/// `forward` back over this same bridge, and a read loop parked inside
/// `kernel.send` cannot read the frame that would settle it.
///
/// Over WS the remote's `web_ws` calls this kernel's agents once the leg said
/// `hello` (`<guid>|<agent>` there); the memory-paired tests and the symmetric
/// relay legs reach the same path (each read loop plays the `web_ws._on_call`
/// role).
///
/// The reply goes out AFTER every event the far side watches through this leg
/// that is already queued (the served readers are flushed under their drains'
/// locks first): a watcher never sees a call's reply overtake the events the
/// call produced.
async fn serve_inbound_call(
    leg: AgentId,
    state: Arc<BridgeState>,
    kernel: Arc<Kernel>,
    frame: Value,
    body: Option<Blob>,
) {
    // The frame is typed once: its id, target and the call's verb come from the
    // declaration, not from reads of the raw value. A frame that is not a call of
    // that shape is answered with the refusal.
    let CallFrame {
        id: corr_id,
        target,
        payload,
    } = match serde_json::from_value::<CallFrame>(frame.clone()) {
        Ok(f) => f,
        Err(e) => {
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let _ = state
                .transport
                .send_frame(json!({
                    "type": "reply",
                    "id": id,
                    "data": fantastic_kernel::verbs::unparsed("call", &e.to_string()),
                }))
                .await;
            return;
        }
    };
    let verb = payload.verb.clone();
    let inner = payload.into_value();
    // AUTH GATE — the single choke point. The leg's INGRESS rule — read off the
    // LIVE record, per frame, as every web/file/exec leg does — decides whether
    // this inbound call dispatches locally; on deny we reply {error,
    // reason:"unauthorized"} (fail-fast, not a silent drop).
    let decision = authorizer::ingress::resolve(rule_spec(&leg, &kernel, "ingress_rule").as_ref())
        .authorize(&Action {
            kind: "call",
            target: &target,
            verb: &verb,
            // the `auth_token` rides the frame envelope, not the payload
            token: frame.get("auth_token").and_then(Value::as_str),
        });
    if let Decision::Deny(denial) = decision {
        let _ = state
            .transport
            .send_frame(json!({"type":"reply","id":corr_id,"data":deny_reply(&denial)}))
            .await;
    } else {
        // THE FRAME ID IS THE EXECUTION ID. The correlator a peer already sends
        // NAMES the execution, so a reconnect that replays its queue joins the
        // original instead of running the side effect twice. No second `exec_id`
        // field exists. A BINARY frame is the same call: its body sits at
        // `["payload", …path]` (the frame's `_binary_path`) and rides the SAME
        // batch item — pid, call/settle rows, `ps` row, dedup — on the binary
        // channel, at that path of the payload. A body the frame put anywhere else
        // is not in the payload python's decoder hands the kernel.
        let mut item = Item::from_frame(AgentId::from(target.as_str()), inner, Some(corr_id.as_str()));
        // Whatever frame asked — text or binary — the reply carries a body
        // whenever the verb answered one: python's reply dict holds bytes at a
        // key, and the frame kind follows from that alone.
        if let Some(b) = body.and_then(|b| b.under("payload")) {
            item = item.with_binary(b.path, b.bytes);
        }
        let out = kernel.send(vec![item]).await.remove(0);
        let _held = flush_served(&state).await;
        // the reply's own secret paths ride the frame (absent when none), as
        // `web_ws` sends them
        let marks = out.secret.clone();
        match out.body {
            None => {
                let mut env = json!({"type":"reply","id":corr_id,"data":out.result});
                if !marks.is_empty() {
                    env["secret"] = json!(marks);
                }
                let _ = state.transport.send_frame(env).await;
            }
            Some(reply_body) => {
                // Binary reply frame — the body rides raw at `["data", …its slot]`
                // (`["data", "bytes"]` for a `read_stream` chunk), `null` there on
                // the head in python's position (appended when the reply did not
                // name the slot).
                let mut env = json!({"type":"reply","id":corr_id,"data":out.result});
                if !marks.is_empty() {
                    env["secret"] = json!(marks);
                }
                let at = Blob::new(reply_body.nested_in("data"), reply_body.bytes);
                let _ = fantastic_bundle::path::place(&mut env, &at.path, Value::Null);
                let _ = state.transport.send_binary(env, at).await;
            }
        }
    }
}

/// Every event already queued for a watch the far side holds through this leg,
/// sent now — and the readers' locks HELD, so no drain sends after them until
/// the caller (a reply about to go out) drops the guards.
async fn flush_served(
    state: &Arc<BridgeState>,
) -> Vec<tokio::sync::OwnedMutexGuard<fantastic_kernel::inbox::InboxReceiver>> {
    let readers: Vec<(
        String,
        Arc<AsyncMutex<fantastic_kernel::inbox::InboxReceiver>>,
    )> = state
        .served
        .lock()
        .expect("served poisoned")
        .iter()
        .map(|(src, s)| (src.clone(), Arc::clone(&s.rx)))
        .collect();
    let mut held = Vec::with_capacity(readers.len());
    for (src, rx) in readers {
        let mut g = rx.lock_owned().await;
        while let Some(item) = g.try_recv() {
            let _ = send_event(&state.transport, &src, item).await;
        }
        held.push(g);
    }
    held
}

/// One event of a watched local agent → `{type:"event", src, payload}` over the
/// transport; binary when it carries a body (at its path inside `payload`).
async fn send_event(
    transport: &Arc<dyn BridgeTransport>,
    src: &str,
    item: fantastic_kernel::inbox::InboxItem,
) -> Result<(), TransportError> {
    let mut env = json!({"type": "event", "src": src, "payload": item.payload});
    match item.body {
        None => transport.send_frame(env).await,
        Some(b) => {
            let at = Blob::new(b.nested_in("payload"), b.bytes);
            let _ = fantastic_bundle::path::place(&mut env, &at.path, Value::Null);
            transport.send_binary(env, at).await
        }
    }
}

/// `{type:"error", id, src, …refusal}` — the answer to an inbound `watch` that
/// did not register (its keys flattened, as `web_ws` answers a refused watch).
async fn refuse_watch(state: &Arc<BridgeState>, frame: &Value, src: &str, r: &Value) {
    let mut out = Map::new();
    out.insert("type".into(), json!("error"));
    out.insert(
        "id".into(),
        frame.get("id").cloned().unwrap_or_else(|| json!("")),
    );
    out.insert("src".into(), json!(src));
    if let Value::Object(m) = r {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    let _ = state.transport.send_frame(Value::Object(out)).await;
}

/// Serve ONE inbound `{type:"watch", src}`: the far side asks to watch the
/// local agent `src` through this leg. The leg's INGRESS rule first (kind
/// `watch`, read off the live record per frame), then the kernel's own gate AS
/// THE LEG — this runs in the leg's read loop, so the leg's `can_call` must grant
/// `watch` on `src` and `src`'s `callable_by` must accept the leg. Registered, the
/// agent's events stream back as `{type:"event", src, payload}` until `unwatch`
/// or the link drops. A refusal, or a name that is no agent, is answered with an
/// `error` frame and nothing is registered. Inline in the read loop, so a call
/// frame that follows it is served after the watch is in place.
async fn serve_watch(leg: &AgentId, state: &Arc<BridgeState>, kernel: &Arc<Kernel>, frame: &Value) {
    let src = frame.get("src").and_then(Value::as_str).unwrap_or("");
    if src.is_empty()
        || state
            .served
            .lock()
            .expect("served poisoned")
            .contains_key(src)
    {
        return;
    }
    let decision = authorizer::ingress::resolve(rule_spec(leg, kernel, "ingress_rule").as_ref())
        .authorize(&Action {
            kind: "watch",
            target: src,
            verb: "watch",
            token: frame.get("auth_token").and_then(Value::as_str),
        });
    if let Decision::Deny(denial) = decision {
        refuse_watch(state, frame, src, &deny_reply(&denial)).await;
        return;
    }
    let Some(target) = kernel.resolve_target(src) else {
        let r = fantastic_kernel::refusal(
            fantastic_kernel::refusal::UNKNOWN_AGENT,
            format!("watch: no agent {} (an id, or `kernel`)", py_repr(src)),
        );
        refuse_watch(state, frame, src, &r).await;
        return;
    };
    // THIS WATCH'S OWN READER — registered before the watch asks for one.
    let reader = AgentId::from(format!("{leg}>{}", target.id).as_str());
    let rx = Arc::new(AsyncMutex::new(kernel.ensure_inbox(&reader)));
    let ready = kernel.ensure_inbox(&reader);
    if let Some(denial) = kernel.watch(&target.id, reader.clone()).await {
        kernel.remove_inbox(&reader);
        refuse_watch(state, frame, src, &denial).await;
        return;
    }
    let task = kernel.spawn_task(leg, "watch_drain", {
        let (rx, transport, tag) = (
            Arc::clone(&rx),
            Arc::clone(&state.transport),
            src.to_string(),
        );
        async move {
            loop {
                {
                    let mut g = rx.lock().await;
                    while let Some(item) = g.try_recv() {
                        if send_event(&transport, &tag, item).await.is_err() {
                            return;
                        }
                    }
                }
                ready.ready().await;
            }
        }
    });
    state.served.lock().expect("served poisoned").insert(
        src.to_string(),
        Served {
            target: target.id.clone(),
            reader,
            rx,
            task,
        },
    );
}

/// Give one served watch back: unwatch as the leg's reader, stop its drain,
/// drop its inbox.
async fn drop_watch(kernel: &Arc<Kernel>, served: Served) {
    kernel.unwatch(&served.target, &served.reader).await;
    kernel.cancel_task(&served.task).await;
    kernel.remove_inbox(&served.reader);
}

/// Every watch the far side held through this leg, given back — the link is
/// gone (or going).
async fn drop_served(state: &Arc<BridgeState>, kernel: &Arc<Kernel>) {
    let all: Vec<Served> = state
        .served
        .lock()
        .expect("served poisoned")
        .drain()
        .map(|(_, s)| s)
        .collect();
    for s in all {
        drop_watch(kernel, s).await;
    }
}

/// Write-time check of the leg's `ingress_rule` (`ingress::unknown_refusal`, the
/// one write-time check every leg makes). Python `io_bridge._engine.validate_meta`.
fn validate_bridge_meta(meta: &Map<String, Value>) -> Option<String> {
    authorizer::ingress::unknown_refusal(meta, &authorizer::ingress::REGISTRY)
}

async fn read_loop(
    agent_id: AgentId,
    state: Arc<BridgeState>,
    kernel: Arc<Kernel>,
    family: Family,
) {
    // In-flight inbound dispatches — one task per `call` frame (with the frame id
    // that names its execution), ended together when the loop exits so nothing
    // keeps running into a dead transport.
    let mut calls: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();
    loop {
        let frame = match state.transport.recv_frame().await {
            Ok(f) => f,
            Err(TransportError::ConnectionClosed(_)) => break,
            Err(_) => break,
        };
        // Normalize text/binary into (envelope, body). For a binary frame the
        // envelope holds null at the body's slot; the raw body travels here, at
        // its steps from the envelope root.
        let (frame, body) = match frame {
            transport::Frame::Text(v) => (v, None),
            transport::Frame::Binary(h, b) => (h, Some(b)),
        };
        let ftype = frame.get("type").and_then(Value::as_str).unwrap_or("");
        match ftype {
            "call" => {
                // TRACKED TASK, not an inline await. Serving the call here parked
                // the loop inside `kernel.send`, so a handler that forwarded back
                // over THIS bridge could never have its reply read — the round
                // trip deadlocked until the 30s forward timeout. One task per
                // call, each carrying THIS loop's identity — the leg's, since its
                // own `boot` spawned the loop (`spawn_inherit`) — all aborted when
                // the loop exits.
                let call_state = Arc::clone(&state);
                let call_kernel = Arc::clone(&kernel);
                calls.retain(|(_, h)| !h.is_finished());
                let frame_id = frame
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                calls.push((
                    frame_id,
                    fantastic_kernel::spawn_inherit(serve_inbound_call(
                        agent_id.clone(),
                        call_state,
                        call_kernel,
                        frame,
                        body,
                    )),
                ));
            }
            "event" => {
                // The `watch_remote` RETURN half: re-emitted on the bridge's OWN
                // inbox for local watchers ONLY when its `src` is a remote agent
                // this leg subscribed to. An event nobody asked for is dropped.
                let Some(src) = frame.get("src").and_then(Value::as_str) else {
                    continue;
                };
                if !state
                    .watching
                    .lock()
                    .expect("watching poisoned")
                    .contains(src)
                {
                    continue;
                }
                let mut payload = match frame.get("payload") {
                    Some(v) if v.is_object() => v.clone(),
                    _ => continue,
                };
                payload["src"] = json!(src);
                // A BINARY event re-emits WHOLE. The envelope's `_binary_path` is
                // `["payload", …p]`; the inbox carrier wants `p`, the steps inside
                // the payload, so the body reaches a local watcher as the frame
                // the remote sent rather than a head with a hole in it.
                let item = match body.and_then(|b| b.under("payload")) {
                    Some(b) => fantastic_kernel::inbox::InboxItem::binary(payload, b),
                    None => fantastic_kernel::inbox::InboxItem::text(payload),
                };
                kernel.emit_item(&agent_id, item).await;
            }
            "reply" => {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let data = frame.get("data").cloned().unwrap_or(Value::Null);
                // optional `secret`: dotted paths into `data` the far reply's
                // bundle marked. Malformed ⇒ none (the reply is still delivered).
                let marks =
                    fantastic_kernel::secret::check(frame.get("secret")).unwrap_or_default();
                let tx = state.pending.lock().expect("pending poisoned").remove(id);
                if let Some(tx) = tx {
                    // Carry the reply body alongside, at its slot of the reply
                    // (`["data", …p]` on the frame is `p` in `data`). A TEXT reply
                    // frame carried no chunk at all (`None`); a binary one carries
                    // `Some`, even when the chunk is empty. A body the frame
                    // put outside `data` is not part of the reply.
                    let _ = tx.send(Ok((data, body.and_then(|b| b.under("data")), marks)));
                }
            }
            "error" => {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // python RESOLVES the pending future with this value — the
                // forward returns it verbatim, with no `bridge.forward:` prefix.
                let err = py_str(frame.get("error").unwrap_or(&Value::Null));
                let tx = state.pending.lock().expect("pending poisoned").remove(id);
                if let Some(tx) = tx {
                    let _ = tx.send(Ok((
                        json!({ "error": format!("remote error: {err}") }),
                        None,
                        Vec::new(),
                    )));
                }
            }
            "watch" => serve_watch(&agent_id, &state, &kernel, &frame).await,
            "unwatch" => {
                // NO GATE — giving a watch back (as `web_ws` leaves it ungated).
                let src = frame.get("src").and_then(Value::as_str).unwrap_or("");
                let served = state.served.lock().expect("served poisoned").remove(src);
                if let Some(s) = served {
                    drop_watch(&kernel, s).await;
                }
            }
            _ => {
                // Unknown frame type — ignore so the loop stays robust.
                // (call/reply/error/event/watch/unwatch are handled above.)
            }
        }
    }

    // Read loop exit — the transport is gone, so every in-flight inbound dispatch
    // has nowhere to send its reply. End them, then emit bridge_down + fail
    // every pending oneshot.
    //
    // THROUGH THE TABLE, not a raw abort: the frame id IS the execution id (the
    // pid), and a dropped dispatch frees none of what its handler holds — an AI
    // agent's turn stays `generating` for good and every later send to it hangs.
    // `cancel_task` runs the bundle's `on_cancel` (the turn is released) before it
    // stops the task, exactly as an operator's `cancel` would. The abort then
    // catches the serving task itself (its reply has nowhere to go) and a call
    // still before its dispatch (no execution yet).
    for (frame_id, h) in &calls {
        if !h.is_finished() {
            if let Some(exec) = kernel.ledger.resolve_name(frame_id) {
                kernel.cancel_task(&fantastic_kernel::TaskId(exec)).await;
            }
        }
        h.abort();
    }
    drop_served(&state, &kernel).await;
    emit_bridge_down(&agent_id, &state, &kernel).await;
    let drained: Vec<PendingReply> = {
        let mut p = state.pending.lock().expect("pending poisoned");
        p.drain().map(|(_k, v)| v).collect()
    };
    for tx in drained {
        let _ = tx.send(Err("bridge transport closed".to_string()));
    }
    // A dialled `ws` link that dropped is dialled again. Any other end of this
    // loop is not a drop to heal: the memory transport is a test pair, and the
    // relay transport heals its own socket.
    if family == Family::Ws && state.transport_kind == "ws" {
        redial(family, &agent_id, &state, &kernel).await;
    }
}

/// The first wait of the redial backoff.
const REDIAL_FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
/// The longest wait of the redial backoff.
const REDIAL_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// The wait before redial attempt `attempt` (0 = the first, which is at once):
/// one second, doubling, up to thirty.
fn redial_delay(attempt: u32) -> std::time::Duration {
    match attempt {
        0 => std::time::Duration::ZERO,
        n => REDIAL_FIRST_DELAY
            .saturating_mul(1u32 << (n - 1).min(16))
            .min(REDIAL_MAX_DELAY),
    }
}

/// The dialing side redials the known address after a drop — the record's
/// `host`/`port`/`peer_id`, as `boot` reads them — until a link is up, waiting
/// longer after each refusal. It runs as the dropped link's own read task, so
/// `shutdown` (the verb or the agent's deletion) cancels it with that task; a
/// `reconnect` or `boot` that got there first has replaced the state, and ends it.
async fn redial(
    family: Family,
    agent_id: &AgentId,
    dropped: &Arc<BridgeState>,
    kernel: &Arc<Kernel>,
) {
    // The reader may report the drop a moment before the socket reads as closed;
    // closing settles it, so `boot` rebuilds instead of answering `already`.
    dropped.transport.close().await;
    let watched = dropped.watching.lock().expect("watching poisoned").clone();
    let mut attempt = 0u32;
    loop {
        let wait = redial_delay(attempt);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let current = BRIDGES.lock().get(agent_id).cloned();
        if !current.is_some_and(|c| Arc::ptr_eq(&c, dropped)) {
            return;
        }
        let r = boot_carrying(family, agent_id, kernel, watched.clone()).await;
        if r.get("error").is_none() {
            return;
        }
        attempt = attempt.saturating_add(1);
    }
}

// ── memory transport test seam ──────────────────────────────────────

/// Pending memory-transport injections keyed by the agent id that
/// will pick them up at `boot` time. The memory transport carries
/// no record-level config (a Queue can't be serialized), so tests
/// stash a paired half here and the bridge's `boot` consumes it.
static INJECTED: OnceLock<Mutex<HashMap<AgentId, Arc<dyn BridgeTransport>>>> = OnceLock::new();

fn injected_map() -> std::sync::MutexGuard<'static, HashMap<AgentId, Arc<dyn BridgeTransport>>> {
    INJECTED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("INJECTED poisoned")
}

fn take_injected(agent_id: &AgentId) -> Option<Arc<dyn BridgeTransport>> {
    injected_map().remove(agent_id)
}

/// Wire two agent ids together with a paired in-process
/// [`MemoryTransport`]. After calling this, `kernel.send(vec![Item::new(<id>, /// {type:"boot"})])` on either side will pick up its half from the
/// injection table.
///
/// Test-only helper; real WS / HTTP bridges build their transport
/// from agent.yaml fields at boot time.
pub fn inject_pair(a_id: &AgentId, b_id: &AgentId) {
    let (a, b) = MemoryTransport::pair();
    let mut map = injected_map();
    map.insert(a_id.clone(), a);
    map.insert(b_id.clone(), b);
}

/// Test seam: inject one half for `agent_id` and return the peer half
/// so a test can drive the wire directly — read what the bridge sends
/// (`recv_frame`) and push synthetic inbound frames (`send_frame`).
/// Mirrors Python's `MemoryTransport.pair()` + single-side injection.
pub fn inject_one(agent_id: &AgentId) -> Arc<dyn BridgeTransport> {
    let (a, b) = MemoryTransport::pair();
    injected_map().insert(agent_id.clone(), a);
    b
}

#[cfg(test)]
mod tests;

/// The ws transport on the wire, against a peer speaking as python's
/// `websockets.serve` does (deflate negotiation, closing handshake text).
#[cfg(test)]
#[path = "tests_ws_wire.rs"]
mod tests_ws_wire;

/// A message on a dialled socket (ws_bridge,
/// relay_connector), fragmented or not, is bounded by the one frame cap.
#[cfg(test)]
#[path = "tests_ws_message_cap.rs"]
mod tests_ws_message_cap;

/// `_binary_path` as a list of steps across a bridge (python
/// `io_bridge/tests/test_binary_path_steps.py`, bridge half).
#[cfg(test)]
#[path = "tests_binary_steps.rs"]
mod binary_steps;

/// Relay directory events reach the connector's inbox (python
/// `relay_connector/tests/test_relay_connector.py`, the engine half).
#[cfg(test)]
#[path = "tests_relay_directory.rs"]
mod relay_directory;
