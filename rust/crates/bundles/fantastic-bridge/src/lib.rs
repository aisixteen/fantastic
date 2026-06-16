//! fantastic-bridge — cross-kernel comms. TWO io_bridge derivations:
//! `ws_bridge` (ws/ssh/memory) + `relay_connector` (relay-kernel router),
//! sharing one engine.
//!
//! Pairs of bridge agents forward `send` envelopes between kernels
//! over a pluggable transport. Weak binding: remote agents are
//! addressed by URL + path only; no shared Rust types across kernels.
//!
//! WS-only, asymmetric (matches the canonical Python kernel). Transports:
//! - `memory`  — in-process paired channels (tests only)
//! - `ws`      — `tokio-tungstenite` client against a remote `web_ws` surface
//! - `ssh+ws`  — spawns `ssh -L local:localhost:remote -N <host>` as a
//!   subprocess, then layers `ws` over the local tunnel port. Gated by
//!   `feature = "full"` — the embedded slice (iOS) does not spawn
//!   subprocesses.
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
//! inbound   {type:"event", payload}      — read loop re-emits on this bridge's inbox
//! ```

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

// The authorization rule registries live in the shared `fantastic-io-bridge` base
// (every io derivation imports them); re-export so existing `authorizer::` paths hold.
pub use fantastic_io_bridge::authorizer;
pub mod transport;

use authorizer::{Action, Decision, EgressRule, IngressRule};
use transport::memory::MemoryTransport;
use transport::relay::RelayTransport;
#[cfg(feature = "full")]
use transport::ssh::SshTransport;
use transport::ws::WsTransport;
use transport::{BridgeTransport, TransportError};

/// `handler_module` of the WS derivation (ws / ssh+ws / memory transports).
pub const WS_HANDLER_MODULE: &str = "ws_bridge.tools";
/// `handler_module` of the RELAY derivation (the relay-kernel router transport).
pub const RELAY_HANDLER_MODULE: &str = "relay_connector.tools";

/// Which transport family a bundle admits — the only behavioural difference
/// between the two derivations (they share one engine, mirroring py's
/// io_bridge engine + thin ws_bridge / relay_connector derivations).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// ws / ssh+ws / memory (the `ws_bridge` derivation).
    Ws,
    /// the relay-kernel router (`relay_connector` derivation).
    Relay,
}

impl Family {
    fn label(self) -> &'static str {
        match self {
            Family::Ws => "ws_bridge",
            Family::Relay => "relay_connector",
        }
    }
    fn admits(self, transport_kind: &str) -> bool {
        match self {
            Family::Ws => matches!(transport_kind, "memory" | "ws" | "ssh+ws"),
            Family::Relay => matches!(transport_kind, "memory" | "relay"),
        }
    }
}

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default forward timeout in seconds. Caller can override per call
/// via `payload.timeout`.
pub const DEFAULT_FORWARD_TIMEOUT_SECS: f64 = 30.0;

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
type PendingReply = oneshot::Sender<Result<(Value, Vec<u8>), String>>;

/// Per-agent bridge runtime. Cloneable through an `Arc` so the read
/// loop can hold one reference and the verb handlers can hold
/// another without lifetime gymnastics.
///
/// The fields are intentionally `pub(crate)` — outside callers
/// interact exclusively via verbs.
pub(crate) struct BridgeState {
    pub(crate) transport: Arc<dyn BridgeTransport>,
    pub(crate) transport_kind: String,
    pub(crate) read_task: AsyncMutex<Option<JoinHandle<()>>>,
    pub(crate) pending: Mutex<HashMap<String, PendingReply>>,
    pub(crate) corr_counter: AtomicU64,
    /// The per-leg INGRESS rule (default `AllowAll`); consulted by the read loop
    /// before dispatching an inbound `call` — the single auth choke point.
    pub(crate) ingress: Arc<dyn IngressRule>,
    /// The per-leg EGRESS rule (default `Silent`); `forward` stamps its credential
    /// on the outbound frame envelope.
    pub(crate) egress: Arc<dyn EgressRule>,
}

impl BridgeState {
    fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending poisoned").len()
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The WS derivation of io_bridge — ws / ssh+ws / memory transports. Registered
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
    let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let reply = match verb {
        "reflect" => reflect_reply(agent_id, kernel),
        "boot" => boot_reply(family, agent_id, kernel).await,
        "shutdown" => shutdown_reply(agent_id, kernel).await,
        "reconnect" => reconnect_reply(family, agent_id, kernel).await,
        "forward" => forward_reply(agent_id, payload, kernel).await,
        "watch_remote" => watch_remote_reply(agent_id, payload, "watch").await,
        "unwatch_remote" => watch_remote_reply(agent_id, payload, "unwatch").await,
        "list_peers" | "watch_directory" | "unwatch_directory" => {
            directory_reply(agent_id, payload, verb).await
        }
        "set_identity" => set_identity_reply(agent_id, payload, kernel).await,
        other => json!({"error": format!("{}: unknown verb {other:?}", family.label())}),
    };
    Ok(Some(reply))
}

/// The relay directory verbs (`relay_connector` only) — addressed to the relay's
/// own `relay` agent (`target:"relay"`), not the partner. `list_peers` is a
/// one-shot snapshot; `watch_directory` subscribes so `peer_*` events re-emit on
/// this connector's inbox; `unwatch_directory` stops it.
async fn directory_reply(agent_id: &AgentId, payload: &Value, op: &str) -> Value {
    let Some(state) = BRIDGES.lock().get(agent_id).cloned() else {
        return json!({"error": format!("relay_connector.{op}: not connected (call boot first)")});
    };
    let timeout = payload
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(30.0);
    match op {
        "list_peers" => state.transport.list_peers(timeout).await,
        "watch_directory" => state.transport.watch_directory(timeout).await,
        _ => state.transport.unwatch_directory().await,
    }
}

/// `set_identity` (`relay_connector` only): advertise/update this peer's directory
/// typing — `role`/`owner_guid`/`exposes` (all optional, merged over the record).
/// Persists the change so it re-announces on the next boot, then pushes it to the
/// relay live (→ `peer_updated`). Mirrors py `_set_identity`.
async fn set_identity_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(state) = BRIDGES.lock().get(agent_id).cloned() else {
        return json!({"error": "relay_connector.set_identity: not connected (call boot first)"});
    };
    if let Some(role) = payload.get("role") {
        if !role.is_null() && role.as_str() != Some("manager") && role.as_str() != Some("kernel") {
            return json!({"error": "relay_connector.set_identity: role must be manager|kernel"});
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
) -> Result<(Reply, Vec<u8>), BundleError> {
    let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
    if verb != "forward" {
        let reply = dispatch(family, agent_id, &header, kernel).await?;
        return Ok((reply, Vec::new()));
    }
    let target = header
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let inner = header.get("payload").cloned().unwrap_or(Value::Null);
    if target.is_empty() || !inner.is_object() {
        return Ok((
            Some(json!({"error": "bridge.forward: target (str) + payload (object) required"})),
            Vec::new(),
        ));
    }
    let timeout_secs = header
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS);
    let (v, body) = forward_core(agent_id, &target, inner, blob, true, timeout_secs).await;
    Ok((Some(v), body))
}

#[async_trait]
impl Bundle for WsBridgeBundle {
    fn name(&self) -> &str {
        "ws_bridge"
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
        dispatch(Family::Ws, agent_id, payload, kernel).await
    }
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), BundleError> {
        dispatch_binary(Family::Ws, agent_id, header, blob, kernel).await
    }
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id, kernel).await;
        Ok(())
    }
}

#[async_trait]
impl Bundle for RelayConnectorBundle {
    fn name(&self) -> &str {
        "relay_connector"
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
        dispatch(Family::Relay, agent_id, payload, kernel).await
    }
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), BundleError> {
        dispatch_binary(Family::Relay, agent_id, header, blob, kernel).await
    }
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id, kernel).await;
        Ok(())
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Mint an opaque relay GUID (32 hex). Not cryptographic — it only needs to be
/// unique within a relay group (the relay rejects duplicate GUIDs); composed from
/// time + pid + a stack address + a process-monotonic counter, mirroring the
/// kernel's own `mint_id` philosophy (no extra crate dep).
fn mint_guid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let lo = (std::process::id() as u64).rotate_left(32)
        ^ stack_ptr
        ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{nanos:016x}{lo:016x}")
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
/// legacy `auth` shorthand (so `auth:"password"` sets both sides).
fn rule_spec(agent_id: &AgentId, kernel: &Kernel, primary: &str) -> Option<Value> {
    meta_value(agent_id, kernel, primary).or_else(|| meta_value(agent_id, kernel, "auth"))
}

fn meta_u64(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<u64> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_u64)
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let bridge = BRIDGES.lock().get(agent_id).cloned();
    let connected = bridge.is_some();
    let transport_kind = bridge
        .as_ref()
        .map(|b| b.transport_kind.clone())
        .or_else(|| meta_string(agent_id, kernel, "transport"))
        .unwrap_or_else(|| "memory".to_string());
    let pending = bridge.as_ref().map(|b| b.pending_count()).unwrap_or(0);
    let is_relay = transport_kind == "relay";
    let sentence = if is_relay {
        "Cross-kernel comms through a relay-kernel router — dial-out WS, group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC tunneled over the relay."
    } else {
        "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); memory/ws/ssh+ws; weak proxy."
    };
    json!({
        "id": agent_id.as_str(),
        "sentence": sentence,
        "transport": transport_kind,
        "connected": connected,
        // ws-flavored fields (null on a relay leg) + relay fields (null on a ws leg).
        "host": meta_string(agent_id, kernel, "host"),
        "port": meta_u64(agent_id, kernel, "port"),
        "peer_id": meta_string(agent_id, kernel, "peer_id"),
        "local_port": meta_u64(agent_id, kernel, "local_port"),
        "remote_port": meta_u64(agent_id, kernel, "remote_port"),
        "relay_url": meta_string(agent_id, kernel, "relay_url"),
        "guid": meta_string(agent_id, kernel, "guid"),
        "partner_guid": meta_string(agent_id, kernel, "partner_guid"),
        // Directory typing (relay legs) — what we advertise to the relay's directory.
        "role": meta_string(agent_id, kernel, "role").unwrap_or_else(|| "kernel".into()),
        "owner_guid": meta_value(agent_id, kernel, "owner_guid").unwrap_or(Value::Null),
        "exposes": meta_value(agent_id, kernel, "exposes").unwrap_or_else(|| json!([])),
        // Read-key == write-key (py parity): `ingress_rule`/`egress_rule`, so a
        // `reflect` → `update_agent` edit is a direct mirror. No `auth` alias in
        // the reflect (it's a legacy WRITE shorthand only). `sealed` = the leg
        // still denies inbound (the conscious-open signal).
        "ingress_rule": authorizer::rule_name(rule_spec(agent_id, kernel, "ingress_rule").as_ref(), "deny_inbound"),
        "egress_rule": authorizer::rule_name(rule_spec(agent_id, kernel, "egress_rule").as_ref(), "silent"),
        "sealed": authorizer::rule_name(rule_spec(agent_id, kernel, "ingress_rule").as_ref(), "deny_inbound") != "allow_all",
        "pending_count": pending,
        "verbs": {
            "reflect": "Identity + transport + connectivity. No args.",
            "boot": "Open the transport, spawn the read loop, emit bridge_up. Idempotent.",
            "shutdown": "Cancel read loop, close transport, reject pending forwards.",
            "reconnect": "shutdown + boot — no auto-reconnect by design.",
            "forward": "args: target:str, payload:dict, timeout:float? (default 30s). Ships a raw call frame to the remote, awaits the reply, returns it unwrapped.",
            "watch_remote": "args: target:str. Streams a remote agent's emits onto this bridge's inbox via the remote's watch protocol.",
            "unwatch_remote": "args: target:str. Stops a watch_remote subscription.",
        },
        "emits": {
            "bridge_up": "{type:'bridge_up'} on this agent's inbox after a successful boot",
            "bridge_down": "{type:'bridge_down'} when the transport drops",
            "<remote event>": "events from watch_remote are re-emitted on this agent's inbox",
        }
    })
}

async fn boot_reply(family: Family, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    // Idempotent: re-booting a connected bridge is a no-op.
    if let Some(existing) = BRIDGES.lock().get(agent_id).cloned() {
        return json!({"already": true, "transport": existing.transport_kind});
    }

    let kind = meta_string(agent_id, kernel, "transport").unwrap_or_else(|| {
        if family == Family::Relay {
            "relay"
        } else {
            "memory"
        }
        .to_string()
    });
    // The derivation only opens transports in its own family — a `relay_connector`
    // can't open a ws socket and vice-versa (the two-bundle split).
    if !family.admits(&kind) {
        return json!({
            "error": format!("{}: transport {kind:?} not in this derivation", family.label())
        });
    }
    let transport: Arc<dyn BridgeTransport> = match kind.as_str() {
        "memory" => match take_injected(agent_id) {
            Some(t) => t,
            None => {
                return json!({
                    "error": "bridge: memory transport requires inject_pair (test seam)"
                })
            }
        },
        "ws" => {
            let peer_id = match meta_string(agent_id, kernel, "peer_id") {
                Some(p) => p,
                None => return json!({"error": "bridge: ws transport requires peer_id"}),
            };
            // Canonical field is `local_port` (Python parity);
            // accept `remote_port` as a fallback.
            let port = match meta_u64(agent_id, kernel, "local_port")
                .or_else(|| meta_u64(agent_id, kernel, "remote_port"))
            {
                Some(p) => p as u16,
                None => return json!({"error": "bridge: ws transport requires local_port"}),
            };
            let host =
                meta_string(agent_id, kernel, "host").unwrap_or_else(|| "localhost".to_string());
            let url = format!("ws://{host}:{port}/{peer_id}/ws");
            match WsTransport::connect(&url).await {
                Ok(t) => t,
                Err(e) => return json!({"error": format!("bridge: ws connect failed: {e}")}),
            }
        }
        #[cfg(feature = "full")]
        "ssh+ws" => {
            let peer_id = match meta_string(agent_id, kernel, "peer_id") {
                Some(p) => p,
                None => return json!({"error": "bridge: ssh+ws transport requires peer_id"}),
            };
            let host = match meta_string(agent_id, kernel, "host") {
                Some(h) => h,
                None => return json!({"error": "bridge: ssh+ws transport requires host"}),
            };
            let remote_port = match meta_u64(agent_id, kernel, "remote_port") {
                Some(p) if p > 0 && p <= u16::MAX as u64 => p as u16,
                _ => {
                    return json!({
                        "error": "bridge: ssh+ws transport requires remote_port"
                    })
                }
            };
            // local_port is optional — 0 means "pick an ephemeral
            // loopback port" inside the transport.
            let local_port = meta_u64(agent_id, kernel, "local_port")
                .filter(|p| *p <= u16::MAX as u64)
                .map(|p| p as u16)
                .unwrap_or(0);
            match SshTransport::open(&host, &peer_id, remote_port, local_port).await {
                Ok(t) => t,
                Err(e) => return json!({"error": format!("bridge: ssh+ws failed: {e}")}),
            }
        }
        #[cfg(not(feature = "full"))]
        "ssh+ws" => {
            return json!({
                "error": "bridge: ssh+ws transport requires the `full` feature"
            })
        }
        "relay" => {
            // Relay-kernel router: dial ws://<relay_url>/<guid> (group password in
            // X-Fantastic-Auth, checked once at the WS upgrade) and tunnel to a
            // fixed partner GUID. No certs/mTLS/issue_url — the relay auths the
            // connection and routes by `target`.
            let relay_url = match meta_string(agent_id, kernel, "relay_url") {
                Some(u) => u,
                None => return json!({"error": "relay_connector: relay_url required"}),
            };
            // `guid` (our WS path) is auto-minted ONCE on first boot if absent and
            // persisted into the record, so every later hydration re-dials the SAME
            // path; an explicit guid always wins, and a minted one is never
            // regenerated (read verbatim next boot). Mirrors py/swift.
            let guid = match meta_string(agent_id, kernel, "guid") {
                Some(g) => g,
                None => {
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
            let partner = match meta_string(agent_id, kernel, "partner_guid") {
                Some(p) => p,
                None => return json!({"error": "relay_connector: partner_guid required"}),
            };
            let token = meta_string(agent_id, kernel, "relay_token").unwrap_or_default();
            // `reconnect` (s) backoff before each re-dial; default 10, 0 = one-shot.
            let reconnect = meta_value(agent_id, kernel, "reconnect")
                .and_then(|v| v.as_f64())
                .unwrap_or(10.0);
            // Directory typing: validate `role` and advertise the attrs blob on connect.
            if let Some(role) = meta_string(agent_id, kernel, "role") {
                if role != "manager" && role != "kernel" {
                    return json!({"error": format!("relay_connector: role must be manager|kernel, got {role:?}")});
                }
            }
            let identity = identity_from_meta(agent_id, kernel);
            match RelayTransport::connect(&relay_url, &guid, &token, &partner, reconnect, identity)
                .await
            {
                Ok(t) => t,
                Err(e) => return json!({"error": format!("relay_connector: relay dial: {e}")}),
            }
        }
        other => return json!({"error": format!("bridge: unknown transport {other:?}")}),
    };

    // Resolve the per-leg ingress + egress rules from the record (`ingress_rule` /
    // `egress_rule`, else the legacy `auth` shorthand). A bad rule fails the boot
    // loudly rather than silently mis-securing.
    let ingress =
        match authorizer::ingress::resolve(rule_spec(agent_id, kernel, "ingress_rule").as_ref()) {
            Ok(r) => r,
            Err(e) => {
                transport.close().await;
                return json!({"error": format!("bridge: bad ingress rule: {e}")});
            }
        };
    let egress =
        match authorizer::egress::resolve(rule_spec(agent_id, kernel, "egress_rule").as_ref()) {
            Ok(r) => r,
            Err(e) => {
                transport.close().await;
                return json!({"error": format!("bridge: bad egress rule: {e}")});
            }
        };

    let state = Arc::new(BridgeState {
        transport: Arc::clone(&transport),
        transport_kind: kind.clone(),
        read_task: AsyncMutex::new(None),
        pending: Mutex::new(HashMap::new()),
        corr_counter: AtomicU64::new(0),
        ingress,
        egress,
    });

    // Spawn the read loop BEFORE we publish the state so the
    // very first inbound frame can land.
    let read_state = Arc::clone(&state);
    let read_agent = agent_id.clone();
    let read_kernel = Arc::clone(kernel);
    let task = tokio::spawn(async move {
        read_loop(read_agent, read_state, read_kernel).await;
    });
    *state.read_task.lock().await = Some(task);

    BRIDGES.lock().insert(agent_id.clone(), Arc::clone(&state));

    kernel.emit(agent_id, json!({"type": "bridge_up"})).await;
    json!({"booted": true, "transport": kind})
}

async fn shutdown_reply(agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Value {
    let removed = BRIDGES.lock().remove(agent_id);
    let Some(state) = removed else {
        return json!({"stopped": true, "reason": "not running"});
    };
    // Tear down the read task — abort first so the close below
    // doesn't race a frame mid-parse.
    if let Some(task) = state.read_task.lock().await.take() {
        task.abort();
    }
    state.transport.close().await;
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
            "error": "bridge.forward: target (str) + payload (object) required"
        });
    }
    let inner = inner.cloned().unwrap_or(Value::Null);
    let timeout_secs = payload
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS);
    let _ = kernel; // peer_id no longer wraps the frame (asymmetric raw call)
                    // TEXT forward — no request body; the reply body (if any) is dropped here
                    // because the text channel can't carry it (a binary forward uses
                    // `handle_binary`, which preserves the reply body).
    forward_core(agent_id, target, inner, Vec::new(), false, timeout_secs)
        .await
        .0
}

/// The shared forward engine — ships a raw `call` frame (text or binary) to the
/// remote's web_ws and awaits the correlated reply. Returns `(reply, body)`;
/// `body` is empty for a text reply, the raw chunk for a binary one
/// (`read_stream` over the wire). Errors surface as `{error}` in the reply.
async fn forward_core(
    agent_id: &AgentId,
    target: &str,
    inner: Value,
    body: Vec<u8>,
    is_binary: bool,
    timeout_secs: f64,
) -> (Value, Vec<u8>) {
    let state = match BRIDGES.lock().get(agent_id).cloned() {
        Some(s) => s,
        None => {
            return (
                json!({"error": "bridge.forward: not connected (call boot first)"}),
                Vec::new(),
            )
        }
    };
    let n = state.corr_counter.fetch_add(1, Ordering::SeqCst) + 1;
    let corr = format!("{}:{}", agent_id, n);

    let (tx, rx) = oneshot::channel::<Result<(Value, Vec<u8>), String>>();
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
            json!({"error": format!("bridge.forward: send failed: {e}")}),
            Vec::new(),
        );
    }

    // Wait for the reply with a timeout. On EITHER timeout OR
    // close-rejection we clean the pending slot so it doesn't leak.
    let dur = std::time::Duration::from_secs_f64(timeout_secs.max(0.001));
    let err = match tokio::time::timeout(dur, rx).await {
        Ok(Ok(Ok((v, b)))) => return (v, b),
        Ok(Ok(Err(e))) => format!("bridge.forward: {e}"),
        Ok(Err(_canceled)) => "bridge.forward: pending dropped".to_string(),
        Err(_elapsed) => format!("bridge.forward: timeout after {timeout_secs}s"),
    };
    state
        .pending
        .lock()
        .expect("pending poisoned")
        .remove(&corr);
    (json!({ "error": err }), Vec::new())
}

/// `watch_remote` / `unwatch_remote`: send `{type:"watch"|"unwatch",
/// src:target}` over the transport. Inbound `event` frames arrive via
/// the read loop and are re-emitted on this bridge agent's inbox.
async fn watch_remote_reply(agent_id: &AgentId, payload: &Value, kind: &str) -> Value {
    let target = payload.get("target").and_then(Value::as_str).unwrap_or("");
    if target.is_empty() {
        return json!({"error": format!("bridge.{kind}_remote: target (str) required")});
    }
    let state = match BRIDGES.lock().get(agent_id).cloned() {
        Some(s) => s,
        None => {
            return json!({
                "error": format!("bridge.{kind}_remote: not connected (call boot first)")
            })
        }
    };
    let frame = json!({ "type": kind, "src": target });
    if let Err(e) = state.transport.send_frame(frame).await {
        return json!({"error": format!("bridge.{kind}_remote: send failed: {e}")});
    }
    let key = if kind == "watch" {
        "watching"
    } else {
        "unwatched"
    };
    json!({ "ok": true, key: target })
}

// ── read loop ───────────────────────────────────────────────────────

async fn read_loop(agent_id: AgentId, state: Arc<BridgeState>, kernel: Arc<Kernel>) {
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
                // Inbound raw call. In production (WS) the bridge is a
                // pure client and never sees inbound calls — its peer
                // is the remote's `web_ws`, not another bridge. This
                // branch serves the memory-paired tests, where two
                // in-process bridges shake hands (each read loop plays
                // the `web_ws._on_call` role).
                let corr_id = frame.get("id").cloned().unwrap_or(Value::Null);
                let target = frame
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let inner = frame.get("payload").cloned().unwrap_or(Value::Null);
                // AUTH GATE — the single choke point. The leg's policy decides
                // whether this inbound call dispatches locally; on deny we reply
                // {error, reason:"unauthorized"} (fail-fast, not a silent drop).
                let verb = inner.get("type").and_then(Value::as_str).unwrap_or("");
                let decision = state.ingress.authorize(&Action {
                    kind: "call",
                    target: &target,
                    verb,
                    // the `auth_token` rides the frame envelope, not the payload
                    token: frame.get("auth_token").and_then(Value::as_str),
                });
                if target.is_empty() {
                    let _ = state
                        .transport
                        .send_frame(json!({"type":"reply","id":corr_id,
                            "data":{"error":"bridge: empty call target"}}))
                        .await;
                } else if let Decision::Deny(reason) = decision {
                    let _ = state
                        .transport
                        .send_frame(json!({"type":"reply","id":corr_id,
                            "data":{"error":reason,"reason":"unauthorized"}}))
                        .await;
                } else if is_binary {
                    // Binary call — the body is the request chunk. Dispatch on the
                    // binary channel; the reply may carry a body (read_stream).
                    let (data, reply_body) = kernel
                        .send_with_binary(&AgentId::from(target.as_str()), inner, body)
                        .await;
                    if reply_body.is_empty() {
                        let _ = state
                            .transport
                            .send_frame(json!({"type":"reply","id":corr_id,"data":data}))
                            .await;
                    } else {
                        // Binary reply frame — body rides raw at `data.bytes`.
                        let mut env = json!({"type":"reply","id":corr_id,"data":data});
                        env["_binary_path"] = json!("data.bytes");
                        let _ = state.transport.send_binary(env, reply_body).await;
                    }
                } else {
                    let reply = kernel.send(&AgentId::from(target.as_str()), inner).await;
                    let _ = state
                        .transport
                        .send_frame(json!({"type":"reply","id":corr_id,"data":reply}))
                        .await;
                }
            }
            "event" => {
                // Remote `watch` delivery — re-emit on this bridge's
                // own inbox so local watchers see the remote stream
                // via the standard kernel.watch(<bridge_id>, ...).
                let payload = frame.get("payload").cloned().unwrap_or(Value::Null);
                kernel.emit(&agent_id, payload).await;
            }
            "reply" => {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let data = frame.get("data").cloned().unwrap_or(Value::Null);
                let tx = state.pending.lock().expect("pending poisoned").remove(id);
                if let Some(tx) = tx {
                    // Carry the reply body alongside (empty for a text reply).
                    let _ = tx.send(Ok((data, body)));
                }
            }
            "error" => {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let err = frame
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("remote error")
                    .to_string();
                let tx = state.pending.lock().expect("pending poisoned").remove(id);
                if let Some(tx) = tx {
                    let _ = tx.send(Err(err));
                }
            }
            _ => {
                // Unknown frame type — ignore so the loop stays robust.
                // (call/reply/error/event are handled above.)
            }
        }
    }

    // Read loop exit — emit bridge_down + fail every pending oneshot.
    kernel.emit(&agent_id, json!({"type": "bridge_down"})).await;
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
/// [`MemoryTransport`]. After calling this, `kernel.send(<id>,
/// {type:"boot"})` on either side will pick up its half from the
/// injection table.
///
/// Test-only helper; real WS / HTTP bridges build their transport
/// from agent.json fields at boot time.
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
