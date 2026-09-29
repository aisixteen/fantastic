//! fantastic-bridge — cross-kernel comms. TWO io_bridge derivations:
//! `ws_bridge` (ws/memory) + `relay_connector` (relay-kernel router),
//! sharing one engine.
//!
//! Pairs of bridge agents forward `send` envelopes between kernels
//! over a pluggable transport. Weak binding: remote agents are
//! addressed by URL + path only; no shared Rust types across kernels.
//!
//! WS-only, asymmetric (matches the canonical Python kernel). Transports:
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
//! | `reconnect` | none | shutdown + boot |
//! | `forward` | `{target, payload, timeout?}` | the unwrapped reply from the remote |
//! | `watch_remote` | `{target}` | `{ok, watching}` — stream a remote agent's emits onto this bridge's inbox |
//! | `unwatch_remote` | `{target}` | `{ok, unwatched}` |
//!
//! # Frame protocol (asymmetric — bridge is a pure client)
//!
//! Ships **raw** call frames to the remote's `web_ws`, which dispatches
//! `kernel.send` exactly like a browser frame. No peer bridge needed.
//! Matches Python ws_bridge + fantastic-web WS server:
//!
//! ```text
//! outbound  {type:"call",  id:corr, target, payload}
//! outbound  {type:"watch", src:target}  / {type:"unwatch", src:target}
//! inbound   {type:"reply", id, data}     — read loop routes to pending oneshot
//! inbound   {type:"error", id, error}    — read loop fails the pending oneshot
//! inbound   {type:"event", src, payload} — re-emitted (payload + `src`) on this bridge's
//!                                        inbox ONLY for a subscribed `src`, else dropped
//! ```

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{
    py_float, py_float_repr, py_int, py_repr, py_repr_value, py_str, py_truthy,
};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{refusal_hint, NOT_CONNECTED};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

// The authorization rule registries live in the shared `fantastic-io-bridge` base
// (every io derivation imports them); re-export so existing `authorizer::` paths hold.
pub use fantastic_io_bridge::authorizer;
pub mod transport;

use authorizer::{Action, Decision, EgressRule, IngressRule};
use transport::memory::MemoryTransport;
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

    /// python's `make_verbs(default_kind=…)` — the reflect fallback only; `boot`
    /// defaults to `ws` for BOTH derivations (the shared engine).
    fn default_kind(self) -> &'static str {
        match self {
            Family::Ws => "ws",
            Family::Relay => "relay",
        }
    }

    /// python's reflect `verbs` — the first doc line of each verb in the table,
    /// verbatim, in `make_verbs` order (plus the relay's four directory verbs).
    fn verbs(self) -> Value {
        let mut v = json!({
            "reflect": "Identity + transport + connectivity. No args.",
            "boot": "No args. Reads `transport` (kind) + transport-specific fields off the agent record, builds the transport, spawns the read loop, emits `bridge_up`. Idempotent: re-booting a connected bridge is a no-op.",
            "reconnect": "No args. Teardown + boot — explicit because we don't auto-reconnect on transport failure (keeps real network problems visible to operators / telemetry).",
            "forward": "args: target:str (req — id on the REMOTE kernel) + payload:dict (req — the call to run there; payload is ITSELF a verb frame and MUST carry its own \"type\") + timeout:float? (default 30s). Example: {\"type\":\"forward\",\"target\":\"src\",\"payload\":{\"type\":\"read_stream\",\"path\":\"data.bin\",\"offset\":0}}. Ships a raw `{type:'call', id, target, payload}` frame over the transport, awaits the matching reply, returns the unwrapped data. Local→local stays direct kernel.send (this verb is only for cross-kernel). Multi-hop loop detection is the caller's responsibility — the bridge ships whatever it's given.",
            "watch_remote": "args: target:str (req — id on the REMOTE kernel to watch). Sends `{type:'watch', src:<target>}` over the transport and subscribes this leg to `target`. Subsequent `{type:'event', src, payload}` frames whose `src` is subscribed are re-emitted on THIS bridge agent's inbox as the payload plus `src`; an event for anything not subscribed is dropped. Local watchers subscribe to the bridge with `kernel.watch(<bridge_id>, ...)`. Idempotent (web_ws de-dups via its own `watching` set).",
            "unwatch_remote": "args: target:str (req — id previously passed to watch_remote). Unsubscribes this leg from `target` (events for it are dropped from now on) and sends `{type:'unwatch', src:<target>}` so the remote stops emitting them.",
        });
        if self == Family::Relay {
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "list_peers".into(),
                    json!("No args (optional timeout). One-shot snapshot of the relay directory:"),
                );
                o.insert("watch_directory".into(), json!("No args. Subscribe to the relay directory; live `peer_joined`/`peer_left`/"));
                o.insert(
                    "unwatch_directory".into(),
                    json!("No args. Stop the directory subscription started by `watch_directory`."),
                );
                o.insert("set_identity".into(), json!("Advertise/update this peer's DIRECTORY identity — the typed attrs the relay"));
            }
        }
        v
    }
}

/// `ws_bridge`'s readme.md, auto-seeded into the agent's dir on creation.
pub const WS_README: &str = include_str!("readme_ws.md");

/// `relay_connector`'s readme.md, auto-seeded into the agent's dir on creation.
pub const RELAY_README: &str = include_str!("readme_relay.md");

/// Default forward timeout in seconds. Caller can override per call
/// via `payload.timeout`.
pub const DEFAULT_FORWARD_TIMEOUT_SECS: f64 = 30.0;

/// python `relay_connector._relay.DEFAULT_HEARTBEAT` (0 or less = off).
pub const DEFAULT_HEARTBEAT_SECS: f64 = 30.0;

/// python `relay_connector._relay.DEFAULT_RECONNECT`.
pub const DEFAULT_RECONNECT_SECS: f64 = 10.0;

/// python `relay_connector._relay.ROLES`, as its `repr` prints in a refusal.
pub const ROLES: &str = "('manager', 'kernel')";

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
/// raw chunk for a `read_stream` forward, empty otherwise) or an error string.
type PendingReply = oneshot::Sender<Result<(Value, Option<Vec<u8>>), String>>;

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
    pub(crate) corr_counter: AtomicU64,
    /// One `bridge_down` per boot: whoever tears the leg down first (the read
    /// loop's own exit, or a teardown that cancels it) emits it.
    pub(crate) down_emitted: std::sync::atomic::AtomicBool,
    /// The per-leg INGRESS rule (default `AllowAll`); consulted by the read loop
    /// before dispatching an inbound `call` — the single auth choke point.
    pub(crate) ingress: Arc<dyn IngressRule>,
    /// The per-leg EGRESS rule (default `Silent`); `forward` stamps its credential
    /// on the outbound frame envelope.
    pub(crate) egress: Arc<dyn EgressRule>,
    /// The remote agents this leg SUBSCRIBED to (`watch_remote`). An inbound
    /// `event` frame is re-emitted only when its `src` is here — nothing flows
    /// until subscribed, and nothing after an unwatch.
    pub(crate) watching: Mutex<HashSet<String>>,
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

/// Shared dispatch — the one bridge engine both derivations run. `family` is the
/// only difference: it bounds which transport a `boot` may open.
async fn dispatch(
    family: Family,
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Reply, BundleError> {
    let verb_value = payload.get("type").unwrap_or(&Value::Null);
    let verb = verb_value.as_str().unwrap_or("");
    // The directory verbs belong to the RELAY derivation only — on a ws leg they
    // are not in the verb table at all, so they take the unknown-type reply.
    let directory = matches!(
        verb,
        "list_peers" | "watch_directory" | "unwatch_directory" | "set_identity"
    ) && family == Family::Relay;
    let reply = match verb {
        "reflect" => reflect_reply(family, agent_id, kernel),
        "boot" => boot_reply(family, agent_id, kernel).await,
        "reconnect" => reconnect_reply(family, agent_id, kernel).await,
        "forward" => forward_reply(agent_id, payload, kernel).await,
        "watch_remote" => watch_remote_reply(agent_id, payload, "watch").await,
        "unwatch_remote" => watch_remote_reply(agent_id, payload, "unwatch").await,
        "list_peers" | "watch_directory" | "unwatch_directory" if directory => {
            directory_reply(agent_id, payload, verb).await
        }
        "set_identity" if directory => set_identity_reply(agent_id, payload, kernel).await,
        // python's `dispatch`: a verb the table does not carry (`shutdown`
        // included — the teardown lives in `on_delete`) teaches the envelope.
        _ => unknown_type(verb_value),
    };
    Ok(Some(reply))
}

/// python `dispatch`'s miss branch — the commonest slip is a frame with no
/// `type`, or a `forward` whose inner `payload` dropped its own.
fn unknown_type(verb: &Value) -> Value {
    json!({
        "error": format!("bridge: unknown type {}", py_repr_value(verb)),
        "hint": "every bridge frame needs a \"type\"; to reach a remote agent use \
    {\"type\":\"forward\",\"target\":\"<remote_id>\",\"payload\":{\"type\":\"<verb>\",...}} \
    — the inner payload is itself a verb frame and needs its own \"type\".",
    })
}

/// The relay directory verbs (`relay_connector` only) — addressed to the relay's
/// own `relay` agent (`target:"relay"`), not the partner. `list_peers` is a
/// one-shot snapshot; `watch_directory` subscribes so `peer_*` events re-emit on
/// this connector's inbox; `unwatch_directory` stops it.
async fn directory_reply(agent_id: &AgentId, payload: &Value, op: &str) -> Value {
    let state = BRIDGES.lock().get(agent_id).cloned();
    let live = state.as_ref().filter(|s| !s.transport.closed());
    let Some(state) = live else {
        // python: `unwatch_directory` on a leg with no transport is simply `ok`.
        if op == "unwatch_directory" {
            return json!({"ok": true});
        }
        return refusal_hint(
            NOT_CONNECTED,
            format!("relay_connector.{op}: not connected"),
            "call boot first",
        );
    };
    match op {
        "list_peers" => {
            let timeout = match payload.get("timeout") {
                Some(v) => match py_float(v) {
                    Ok(t) => t,
                    Err(_) => DEFAULT_FORWARD_TIMEOUT_SECS,
                },
                None => DEFAULT_FORWARD_TIMEOUT_SECS,
            };
            state.transport.list_peers(timeout).await
        }
        // python passes NO timeout here — the transport's own 10 s stands.
        "watch_directory" => {
            state
                .transport
                .watch_directory(DIRECTORY_WATCH_TIMEOUT_SECS)
                .await
        }
        _ => state.transport.unwatch_directory().await,
    }
}

/// `set_identity` (`relay_connector` only): advertise/update this peer's directory
/// typing — `role`/`owner_guid`/`exposes` (all optional, merged over the record).
/// Persists the change so it re-announces on the next boot, then pushes it to the
/// relay live (→ `peer_updated`). Mirrors py `_set_identity`.
async fn set_identity_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
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
    if let Some(role) = payload.get("role") {
        if !role.is_null() && role.as_str() != Some("manager") && role.as_str() != Some("kernel") {
            // python `bad("set_identity", "role", f"one of {ROLES}")`.
            let mut out = fantastic_kernel::refusal::refusal(
                fantastic_kernel::refusal::BAD_ARG,
                format!("set_identity: role must be one of {ROLES}"),
            );
            fantastic_kernel::refusal::with(&mut out, "arg", json!("role"));
            return out;
        }
    }
    // Merge only the provided well-known keys into the record + persist (explicit
    // null retracts a field, e.g. owner_guid:null = become standalone).
    if let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        let mut patch = serde_json::Map::new();
        for k in ["role", "owner_guid", "exposes"] {
            if let Some(v) = payload.get(k) {
                patch.insert(k.to_string(), v.clone());
            }
        }
        if !patch.is_empty() {
            agent.update_meta(patch);
            let _ = fantastic_kernel::persistence::persist(kernel, &agent).await;
        }
    }
    let attrs = identity_from_meta(agent_id, kernel);
    state.transport.set_identity(attrs).await
}

/// Shared BINARY dispatch — `forward` carrying a raw request chunk
/// (`write_stream` over the wire). The local caller does
/// `send_with_binary(<bridge>, {type:forward, target, payload, timeout?}, blob)`;
/// the bridge ships a binary `call` frame and returns `(reply, reply_body)` (the
/// reply body is the raw chunk for a `read_stream` forward). Any non-`forward`
/// binary verb routes through the text dispatch (its blob is unused).
async fn dispatch_binary(
    family: Family,
    agent_id: &AgentId,
    header: Value,
    blob: Vec<u8>,
    kernel: &Arc<Kernel>,
) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
    let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
    if verb != "forward" {
        let reply = dispatch(family, agent_id, &header, kernel).await?;
        return Ok((reply, None));
    }
    let target = header
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let inner = header.get("payload").cloned().unwrap_or(Value::Null);
    if target.is_empty() || !inner.is_object() {
        return Ok((
            Some(json!({"error": "bridge.forward: target (str) + payload (dict) required"})),
            None,
        ));
    }
    let timeout_secs = header
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS);
    let (v, body) = forward_core(kernel, agent_id, &target, inner, blob, true, timeout_secs).await;
    Ok((Some(v), body))
}

#[async_trait]
impl Bundle for WsBridgeBundle {
    fn name(&self) -> &str {
        "ws_bridge"
    }
    fn readme(&self) -> Option<&'static str> {
        Some(WS_README)
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        dispatch(Family::Ws, agent_id, payload, kernel).await
    }
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
        dispatch_binary(Family::Ws, agent_id, header, blob, kernel).await
    }
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id, kernel).await;
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
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        validate_peer_guid(meta)
    }
}

#[async_trait]
impl Bundle for RelayConnectorBundle {
    fn name(&self) -> &str {
        "relay_connector"
    }
    fn readme(&self) -> Option<&'static str> {
        Some(RELAY_README)
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        dispatch(Family::Relay, agent_id, payload, kernel).await
    }
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
        dispatch_binary(Family::Relay, agent_id, header, blob, kernel).await
    }
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id, kernel).await;
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
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        validate_peer_guid(meta)
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Mint an opaque relay GUID (32 hex) — python `relay_connector._relay`'s
/// `uuid.uuid4().hex`: 16 bytes from the OS CSPRNG carrying uuid4's version
/// nibble and the RFC-4122 variant bits. A failure is FATAL — there is no
/// second source (the time+pid+counter mixer this replaced was not random).
fn mint_guid() -> String {
    let mut b = [0u8; 16];
    // SAFETY: `b` is a valid writable buffer of exactly 16 bytes.
    let rc = unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) };
    assert!(rc == 0, "getentropy failed: no randomness source");
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
    // python: `st.transport_kind or rec.get("transport") or default_kind`.
    let transport_kind = bridge
        .as_ref()
        .map(|b| b.transport_kind.clone())
        .filter(|k| !k.is_empty())
        .or_else(|| meta_string(agent_id, kernel, "transport"))
        .unwrap_or_else(|| family.default_kind().to_string());
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
    let ingress_name = authorizer::rule_name(
        rule_spec(agent_id, kernel, "ingress_rule").as_ref(),
        "deny_ingress",
    );
    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(agent_id.as_str()));
    out.insert("sentence".into(), json!(family.sentence()));
    out.insert("transport".into(), json!(transport_kind));
    out.insert("connected".into(), json!(connected));
    out.insert("pending_count".into(), json!(pending));
    // Read-key == write-key (py parity): `ingress_rule`/`egress_rule`, so a
    // `reflect` → `update_agent` edit is a direct mirror. No `auth` in the
    // reflect (it's a WRITE shorthand only). `sealed` = the leg still
    // denies inbound (the conscious-open signal).
    out.insert("ingress_rule".into(), json!(ingress_name));
    out.insert(
        "egress_rule".into(),
        json!(authorizer::rule_name(
            rule_spec(agent_id, kernel, "egress_rule").as_ref(),
            "silent_egress"
        )),
    );
    out.insert("sealed".into(), json!(ingress_name != "allow_all_ingress"));
    // The ONE peer kernel whose agents this leg serves (`<peer_guid>|<agent>`);
    // null = no pin, no inbound call.
    out.insert(
        "peer_guid".into(),
        meta_value(agent_id, kernel, "peer_guid").unwrap_or(Value::Null),
    );
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
            for key in ["relay_url", "guid", "partner_guid"] {
                out.insert(
                    key.into(),
                    meta_value(agent_id, kernel, key).unwrap_or(Value::Null),
                );
            }
            out.insert(
                "reconnect".into(),
                json!(meta_value(agent_id, kernel, "reconnect")
                    .as_ref()
                    .map_or(Ok(DEFAULT_RECONNECT_SECS), py_float)
                    .unwrap_or(DEFAULT_RECONNECT_SECS)),
            );
            let role = meta_value(agent_id, kernel, "role").unwrap_or(Value::Null);
            out.insert(
                "role".into(),
                if py_truthy(&role) {
                    role
                } else {
                    json!("kernel")
                },
            );
            out.insert(
                "owner_guid".into(),
                meta_value(agent_id, kernel, "owner_guid").unwrap_or(Value::Null),
            );
            let exposes = meta_value(agent_id, kernel, "exposes").unwrap_or(Value::Null);
            out.insert(
                "exposes".into(),
                if py_truthy(&exposes) {
                    exposes
                } else {
                    json!([])
                },
            );
        }
    }
    out.insert("verbs".into(), family.verbs());
    out.insert(
        "emits".into(),
        json!({
            "bridge_up": "{type:'bridge_up'} — emitted on this agent's inbox after a successful boot",
            "bridge_down": "{type:'bridge_down'} — emitted when the transport drops (peer closed, tunnel died)",
            "<remote event>": "events from `watch_remote` subscriptions are re-emitted on this agent's inbox as their original `{type, ...}` shape plus `src` (the watched remote id); unsubscribed events are dropped",
        }),
    );
    Value::Object(out)
}

async fn boot_reply(family: Family, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    // python: idempotent only while the transport is OPEN — a closed leg is
    // rebuilt in place.
    if let Some(existing) = BRIDGES.lock().get(agent_id).cloned() {
        if !existing.transport.closed() {
            return json!({"already": true, "transport": existing.transport_kind});
        }
    }

    // python's shared engine: `kind = rec.get("transport") or "ws"` for BOTH
    // derivations; the builder is chosen by the derivation, not by the kind.
    let kind = match meta_value(agent_id, kernel, "transport") {
        Some(v) if py_truthy(&v) => py_str(&v),
        _ => "ws".to_string(),
    };
    let transport: Arc<dyn BridgeTransport> = if kind == "memory" {
        match take_injected(agent_id) {
            Some(t) => t,
            None => return json!({"error": "bridge: memory transport requires test injection"}),
        }
    } else {
        match build_transport(family, &kind, agent_id, kernel).await {
            Ok(t) => t,
            Err(e) => return json!({"error": format!("bridge: {kind} boot failed: {e}")}),
        }
    };

    // Resolve the per-leg ingress + egress rules from the record (`ingress_rule` /
    // `egress_rule`, else the `auth` shorthand). A bad rule must not leave
    // the just-built transport leaked/open.
    let rules = authorizer::ingress::resolve(rule_spec(agent_id, kernel, "ingress_rule").as_ref())
        .and_then(|i| {
            authorizer::egress::resolve(rule_spec(agent_id, kernel, "egress_rule").as_ref())
                .map(|e| (i, e))
        });
    let (ingress, egress) = match rules {
        Ok(pair) => pair,
        Err(e) => {
            transport.close().await;
            return json!({"error": format!("bridge: bad auth rule: {e}")});
        }
    };

    let state = Arc::new(BridgeState {
        transport: Arc::clone(&transport),
        transport_kind: kind.clone(),
        read_task: AsyncMutex::new(None),
        pending: Mutex::new(HashMap::new()),
        corr_counter: AtomicU64::new(0),
        down_emitted: std::sync::atomic::AtomicBool::new(false),
        ingress,
        egress,
        watching: Mutex::new(HashSet::new()),
    });

    // Spawn the read loop BEFORE we publish the state so the
    // very first inbound frame can land.
    let read_state = Arc::clone(&state);
    let read_agent = agent_id.clone();
    let read_kernel = Arc::clone(kernel);
    // REGISTERED, AND UNDER THE LEG'S OWN IDENTITY. The read loop outlives the
    // `boot` dispatch and issues a send per inbound frame, so it is real work:
    // `spawn_task` gives it a `ps` row and a `cancel` that reaches it, and
    // carries this dispatch's sender into the fork — an inbound `call` used to
    // reach `Kernel::launch` with `current_sender() == None`, the OPERATOR, and
    // the two-sided gate was skipped for every frame a peer sent.
    let task_id = kernel.spawn_task(&agent_id.clone(), "read_loop", async move {
        read_loop(read_agent, read_state, read_kernel).await;
    });
    *state.read_task.lock().await = Some(task_id);

    BRIDGES.lock().insert(agent_id.clone(), Arc::clone(&state));

    kernel.emit(agent_id, json!({"type": "bridge_up"})).await;
    json!({"booted": true, "transport": kind})
}

/// python's per-derivation `build_transport(kind, rec, kernel, st)` — the ONLY
/// difference between the two bundles. Its `Err` is python's raised `ValueError`
/// text; the caller wraps it as `bridge: <kind> boot failed: <text>`.
async fn build_transport(
    family: Family,
    kind: &str,
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<dyn BridgeTransport>, String> {
    match family {
        Family::Ws => build_ws_transport(kind, agent_id, kernel).await,
        Family::Relay => build_relay_transport(agent_id, kernel).await,
    }
}

/// python `ws_bridge._ws.build_transport` — `host` explicit, nothing guessed.
async fn build_ws_transport(
    kind: &str,
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<dyn BridgeTransport>, String> {
    let peer_id = match meta_value(agent_id, kernel, "peer_id") {
        Some(v) if py_truthy(&v) => py_str(&v),
        _ => return Err("peer_id required for the ws transport".to_string()),
    };
    if kind == "ws" {
        let port = meta_value(agent_id, kernel, "port")
            .filter(py_truthy)
            .ok_or_else(|| "ws transport requires port".to_string())?;
        let port = py_int(&port).map_err(|e| e.message().to_string())?;
        let host = match meta_value(agent_id, kernel, "host") {
            Some(v) if py_truthy(&v) => py_str(&v),
            _ => return Err("ws transport requires host (nothing is guessed)".to_string()),
        };
        let url = format!("ws://{host}:{port}/{peer_id}/ws");
        return WsTransport::connect(&url)
            .await
            .map(|t| t as Arc<dyn BridgeTransport>)
            .map_err(|e| transport_error_text(&e));
    }
    Err(format!("unknown transport {}", py_repr(kind)))
}

/// python `relay_connector._relay.build_transport` — validates BEFORE minting
/// the guid, so a refused boot leaves no identity behind.
async fn build_relay_transport(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Arc<dyn BridgeTransport>, String> {
    let relay_url = meta_value(agent_id, kernel, "relay_url").filter(py_truthy);
    let partner = meta_value(agent_id, kernel, "partner_guid").filter(py_truthy);
    let (Some(relay_url), Some(partner)) = (relay_url, partner) else {
        return Err("relay_connector requires relay_url, partner_guid".to_string());
    };
    // `guid` (our WS path) is auto-minted ONCE on first boot if absent and
    // persisted into the record, so every later hydration re-dials the SAME path;
    // an explicit guid always wins, and a minted one is never regenerated.
    let guid = match meta_value(agent_id, kernel, "guid") {
        Some(v) if py_truthy(&v) => py_str(&v),
        _ => {
            let g = mint_guid();
            if let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
                let mut patch = serde_json::Map::new();
                patch.insert("guid".to_string(), Value::String(g.clone()));
                agent.update_meta(patch);
                let _ = fantastic_kernel::persistence::persist(kernel, &agent).await;
            }
            g
        }
    };
    if let Some(role) = meta_value(agent_id, kernel, "role") {
        if !role.is_null() && role.as_str() != Some("manager") && role.as_str() != Some("kernel") {
            return Err(format!(
                "relay_connector role must be one of {ROLES}, got {}",
                py_repr_value(&role)
            ));
        }
    }
    let token = match meta_value(agent_id, kernel, "relay_token") {
        Some(v) if py_truthy(&v) => py_str(&v),
        _ => String::new(),
    };
    let heartbeat = meta_value(agent_id, kernel, "heartbeat")
        .as_ref()
        .map_or(Ok(DEFAULT_HEARTBEAT_SECS), py_float)
        .map_err(|e| e.message().to_string())?;
    let reconnect = meta_value(agent_id, kernel, "reconnect")
        .as_ref()
        .map_or(Ok(DEFAULT_RECONNECT_SECS), py_float)
        .map_err(|e| e.message().to_string())?;
    let identity = identity_from_meta(agent_id, kernel);
    RelayTransport::connect(
        &py_str(&relay_url),
        &guid,
        &token,
        &py_str(&partner),
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

async fn forward_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let target = payload.get("target").and_then(Value::as_str).unwrap_or("");
    let inner = payload.get("payload");
    if target.is_empty() || !inner.map(Value::is_object).unwrap_or(false) {
        return json!({
            "error": "bridge.forward: target (str) + payload (dict) required"
        });
    }
    let inner = inner.cloned().unwrap_or(Value::Null);
    let timeout_secs = payload
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS);
    // TEXT forward — no request body; the reply body (if any) is dropped here
    // because the text channel can't carry it (a binary forward uses
    // `handle_binary`, which preserves the reply body).
    forward_core(
        kernel,
        agent_id,
        target,
        inner,
        Vec::new(),
        false,
        timeout_secs,
    )
    .await
    .0
}

/// The shared forward engine — ships a raw `call` frame (text or binary) to the
/// remote's web_ws and awaits the correlated reply. Returns `(reply, body)`;
/// `body` is `None` when the remote answered a TEXT frame (no chunk at all) and
/// `Some(chunk)` when it answered a binary one — empty chunk included (C37).
/// Errors surface as `{error}` in the reply.
async fn forward_core(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
    target: &str,
    inner: Value,
    body: Vec<u8>,
    is_binary: bool,
    timeout_secs: f64,
) -> (Value, Option<Vec<u8>>) {
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
    let n = state.corr_counter.fetch_add(1, Ordering::SeqCst) + 1;
    // DASH, not colon: `<agent>:<hex>` is the kernel's minted-pid namespace, and
    // a bridge correlator that borrowed it would read as a REFERENCE to an
    // execution this kernel never minted. Same reason a caller-chosen id may not
    // contain one.
    let corr = format!("{}-{}", agent_id, n);

    let (tx, rx) = oneshot::channel::<Result<(Value, Option<Vec<u8>>), String>>();
    state
        .pending
        .lock()
        .expect("pending poisoned")
        .insert(corr.clone(), tx);

    // Raw call frame straight to the remote's web_ws — no `forward`
    // envelope, no peer_id addressing (asymmetric; matches the canonical Python kernel).
    let mut frame = json!({
        "type": "call",
        "id": corr,
        "target": target,
        "payload": inner,
    });
    // WHO IS CALLING, QUALIFIED: `<this kernel's guid>|<the agent that asked>`
    // (`kernel` for the operator). The far side keys store rows by it.
    if let Some(guid) = local_guid(kernel) {
        let who = fantastic_kernel::current_origin().map_or_else(|| "kernel".to_string(), |o| o.0);
        frame["caller"] = json!(format!("{guid}|{who}"));
    }
    // Stamp this leg's EGRESS credential on the envelope, if its rule presents one
    // (password ⇒ the group token; silent ⇒ None ⇒ no field, wire unchanged). The
    // dispatched `payload` stays clean — the target never sees the token.
    if let Some(token) = state.egress.credential() {
        frame["auth_token"] = json!(token);
    }
    let send_res = if is_binary {
        // The request body rides raw at `payload.bytes` (a python peer reinserts
        // it there; the inner payload's bytes field stays null on the wire).
        frame["_binary_path"] = json!("payload.bytes");
        state.transport.send_binary(frame, body).await
    } else {
        state.transport.send_frame(frame).await
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
        Ok(Ok(Ok((v, b)))) => return (v, b),
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
async fn watch_remote_reply(agent_id: &AgentId, payload: &Value, kind: &str) -> Value {
    let target = payload.get("target").and_then(Value::as_str).unwrap_or("");
    if target.is_empty() {
        return json!({"error": format!("bridge.{kind}_remote: target (str) required")});
    }
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

/// Serve ONE inbound `call` frame and ship its reply.
///
/// Run as its own task (see the read loop): the local handler may itself
/// `forward` back over this same bridge, and a read loop parked inside
/// `kernel.send` cannot read the frame that would settle it.
///
/// In production (WS) the bridge is a pure client and never sees inbound calls —
/// its peer is the remote's `web_ws`, not another bridge. This path serves the
/// memory-paired tests and the symmetric relay legs, where two in-process bridges
/// shake hands (each read loop plays the `web_ws._on_call` role).
async fn serve_inbound_call(
    state: Arc<BridgeState>,
    kernel: Arc<Kernel>,
    frame: Value,
    body: Vec<u8>,
    is_binary: bool,
) {
    let corr_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let target = frame
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // python `frame.get("payload") or {}` — a missing or falsy payload is an
    // empty call, not a null one.
    let inner = match frame.get("payload") {
        Some(v) if py_truthy(v) => v.clone(),
        _ => json!({}),
    };
    // AUTH GATE — the single choke point. The leg's policy decides whether this
    // inbound call dispatches locally; on deny we reply {error,
    // reason:"unauthorized"} (fail-fast, not a silent drop).
    let verb = inner.get("type").and_then(Value::as_str).unwrap_or("");
    let decision = state.ingress.authorize(&Action {
        kind: "call",
        target: &target,
        verb,
        // the `auth_token` rides the frame envelope, not the payload
        token: frame.get("auth_token").and_then(Value::as_str),
    });
    // THE QUALIFIED CALLER, checked on INGRESS before the kernel sees it: from
    // the ONE pinned peer, and never this kernel's own guid.
    let caller = frame.get("caller").cloned().filter(|c| !c.is_null());
    let leg = fantastic_kernel::current_sender().unwrap_or_else(|| AgentId::from(""));
    let pin = meta_string(&leg, &kernel, "peer_guid");
    let bad_caller = check_caller(pin.as_deref(), caller.as_ref()).or_else(|| {
        (pin.is_some() && pin == local_guid(&kernel)).then(|| {
            refusal_hint(
                fantastic_kernel::refusal::UNAUTHORIZED,
                "bridge: this leg's peer_guid is THIS kernel's own guid — refused",
                "pin the PEER kernel's guid",
            )
        })
    });
    if let Decision::Deny(denial) = decision {
        let _ = state
            .transport
            .send_frame(json!({"type":"reply","id":corr_id,"data":deny_reply(&denial)}))
            .await;
    } else if let Some(bad) = bad_caller {
        let _ = state
            .transport
            .send_frame(json!({"type":"reply","id":corr_id,"data":bad}))
            .await;
    } else if is_binary {
        // Binary call — the body is the request chunk. Dispatch on the binary
        // channel; the reply may carry a body (read_stream).
        let target_id = AgentId::from(target.as_str());
        let send = kernel.send_with_binary(&target_id, inner, body);
        let (data, reply_body) = match caller.as_ref().and_then(Value::as_str) {
            // THE QUALIFIED CALLER rides into the dispatch: the gate checks the
            // target's `callable_by` against `<peer guid>|<remote agent>` (never
            // the leg), and a memory store keys what it writes by it.
            Some(c) if !c.is_empty() => {
                fantastic_kernel::with_remote_caller(leg.clone(), c.to_string(), send).await
            }
            _ => send.await,
        };
        match reply_body {
            None => {
                let _ = state
                    .transport
                    .send_frame(json!({"type":"reply","id":corr_id,"data":data}))
                    .await;
            }
            Some(reply_body) => {
                // Binary reply frame — body rides raw at `data.bytes`.
                let mut env = json!({"type":"reply","id":corr_id,"data":data});
                env["_binary_path"] = json!("data.bytes");
                let _ = state.transport.send_binary(env, reply_body).await;
            }
        }
    } else {
        // THE FRAME ID IS THE EXECUTION ID. The correlator a peer already sends
        // NAMES the execution, so a reconnect that replays its queue joins the
        // original instead of running the side effect twice. No second `exec_id`
        // field exists.
        let send = kernel.send(vec![Item::from_frame(
            AgentId::from(target.as_str()),
            inner,
            corr_id.as_str(),
        )]);
        let reply = match caller.as_ref().and_then(Value::as_str) {
            // THE QUALIFIED CALLER rides into the dispatch: the gate checks the
            // target's `callable_by` against `<peer guid>|<remote agent>` (never
            // the leg), and a memory store keys what it writes by it.
            Some(c) if !c.is_empty() => {
                fantastic_kernel::with_remote_caller(leg.clone(), c.to_string(), send).await
            }
            _ => send.await,
        }
        .remove(0)
        .result;
        let _ = state
            .transport
            .send_frame(json!({"type":"reply","id":corr_id,"data":reply}))
            .await;
    }
}

/// This kernel's instance guid — the root record's `guid`, once the root loader
/// minted it.
fn local_guid(kernel: &Arc<Kernel>) -> Option<String> {
    kernel.root().and_then(|r| {
        r.meta
            .read()
            .ok()
            .and_then(|m| m.get("guid").and_then(Value::as_str).map(str::to_string))
            .filter(|g| !g.is_empty())
    })
}

/// INGRESS CHECK on a frame's qualified `caller`, before the kernel sees it.
/// Every inbound call names its sender as `<guid>|<agent id>`, and the guid must
/// be the ONE peer this leg is pinned to (`peer_guid` on the record): a peer
/// cannot pose as a third kernel's agent. No pin, no inbound call. Python
/// `io_bridge._engine._check_caller`.
pub fn check_caller(pin: Option<&str>, caller: Option<&Value>) -> Option<Value> {
    let text = caller.and_then(Value::as_str).filter(|c| {
        let parts: Vec<&str> = c.split('|').collect();
        parts.len() == 2 && parts.iter().all(|p| !p.is_empty())
    });
    let Some(text) = text else {
        return Some(fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::UNAUTHORIZED,
            format!(
                "bridge: an inbound call must name its caller as `<guid>|<agent id>`; got {}",
                caller.map_or_else(|| "None".to_string(), py_repr_value)
            ),
        ));
    };
    let Some(pin) = pin.filter(|p| !p.is_empty()) else {
        return Some(refusal_hint(
            fantastic_kernel::refusal::UNAUTHORIZED,
            "bridge: this leg pins no peer, so it serves no inbound call",
            "update_agent <this leg> peer_guid=<the peer kernel's guid> (its root reflect shows `guid`)",
        ));
    };
    if text.split('|').next() != Some(pin) {
        return Some(fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::UNAUTHORIZED,
            format!(
                "bridge: caller {} is not from the pinned peer {}",
                py_repr(text),
                py_repr(pin)
            ),
        ));
    }
    None
}

/// Write-time check of `peer_guid`: one kernel guid (a non-empty string, no
/// `|`). This kernel's OWN guid is refused where the kernel is in hand — at serve.
fn validate_peer_guid(meta: &Map<String, Value>) -> Option<String> {
    match meta.get("peer_guid") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) if !p.is_empty() && !p.contains('|') => None,
        Some(other) => Some(format!(
            "bridge: peer_guid must be one kernel guid (a non-empty string without `|`); got {}",
            py_repr_value(other)
        )),
    }
}

async fn read_loop(agent_id: AgentId, state: Arc<BridgeState>, kernel: Arc<Kernel>) {
    // In-flight inbound dispatches — one task per `call` frame, aborted together
    // when the loop exits so nothing keeps running into a dead transport.
    let mut calls: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    loop {
        let frame = match state.transport.recv_frame().await {
            Ok(f) => f,
            Err(TransportError::ConnectionClosed(_)) => break,
            Err(_) => break,
        };
        // Normalize text/binary into (envelope, body, is_binary). For a binary
        // frame the envelope's bytes path is null + the raw body travels here.
        let (frame, body, is_binary) = match frame {
            transport::Frame::Text(v) => (v, Vec::new(), false),
            transport::Frame::Binary(h, b) => (h, b, true),
        };
        let ftype = frame.get("type").and_then(Value::as_str).unwrap_or("");
        match ftype {
            "call" => {
                // TRACKED TASK, not an inline await. Serving the call here parked
                // the loop inside `kernel.send`, so a handler that forwarded back
                // over THIS bridge could never have its reply read — the round
                // trip deadlocked until the 30s forward timeout. One task per
                // call, each re-scoped to the leg's identity (a spawned task does
                // not inherit `CURRENT_SENDER`), all aborted when the loop exits.
                let call_state = Arc::clone(&state);
                let call_kernel = Arc::clone(&kernel);
                let call_sender = agent_id.clone();
                calls.spawn(fantastic_kernel::send::with_sender(
                    call_sender,
                    serve_inbound_call(call_state, call_kernel, frame, body, is_binary),
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
                // `payload.<p>`; the inbox carrier wants `<p>`, the path inside
                // the payload, so the body reaches a local watcher as the frame
                // the remote sent rather than a head with a hole in it.
                let inner_path = frame
                    .get("_binary_path")
                    .and_then(Value::as_str)
                    .and_then(|p| p.strip_prefix("payload."));
                let item = match (is_binary, inner_path) {
                    (true, Some(p)) => fantastic_kernel::inbox::InboxItem::binary(payload, p, body),
                    _ => fantastic_kernel::inbox::InboxItem::text(payload),
                };
                kernel.emit_item(&agent_id, item).await;
            }
            "reply" => {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let data = frame.get("data").cloned().unwrap_or(Value::Null);
                let tx = state.pending.lock().expect("pending poisoned").remove(id);
                if let Some(tx) = tx {
                    // Carry the reply body alongside. A TEXT reply frame carried
                    // no chunk at all (`None`); a binary one carries `Some`,
                    // even when the chunk is empty (C37).
                    let _ = tx.send(Ok((data, is_binary.then_some(body))));
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
                    )));
                }
            }
            _ => {
                // Unknown frame type — ignore so the loop stays robust.
                // (call/reply/error/event are handled above.)
            }
        }
    }

    // Read loop exit — the transport is gone, so every in-flight inbound dispatch
    // has nowhere to send its reply. Abort them, then emit bridge_down + fail
    // every pending oneshot.
    calls.abort_all();
    emit_bridge_down(&agent_id, &state, &kernel).await;
    let drained: Vec<PendingReply> = {
        let mut p = state.pending.lock().expect("pending poisoned");
        p.drain().map(|(_k, v)| v).collect()
    };
    for tx in drained {
        let _ = tx.send(Err("bridge transport closed".to_string()));
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
