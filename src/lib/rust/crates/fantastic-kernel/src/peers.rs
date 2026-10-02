//! CONNECTED PEER KERNELS — cross-kernel addressing (owner, 2026-09-30). Python
//! `PeerRoute` + `Kernel.peers` / `register_peer` / `unregister_peer` /
//! `peer_rows` (`kernel/_kernel.py`).
//!
//! An agent on another kernel is addressed `<guid>|<agent id>` and called with an
//! ordinary `send`. When that guid is registered here, the send hands the call to
//! its route and the peer's reply is the call's ordinary outcome. A TRANSPORT
//! registers the peer it is connected to (the web ws leg, when a page says
//! `hello`) and unregisters it when the connection ends — the transport only
//! carries frames; addressing is the kernel's. A guid that is not here is an
//! unknown target like any other.

use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use crate::outcome::Blob;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// What one peer call comes back with: the peer's reply, and the raw body AT ITS
/// SLOT of the reply when the reply carried one (the binary channel rides beside
/// the JSON, as it does everywhere in this runtime — python's reply dict simply
/// holds the bytes, at whatever key the peer's handler put them).
pub type PeerReply = (Value, Option<Blob>);

/// The future a route returns for one call.
pub type PeerCallFuture = Pin<Box<dyn Future<Output = PeerReply> + Send>>;

/// `call(agent_id, payload, caller, secret, body)` — deliver ONE call to the
/// peer's `agent_id` as `caller` (`<this guid>|<sender>`), with the item's
/// `secret` marks ([`crate::secret`], empty when none) and the raw request body
/// at its payload path when the call carries one.
type PeerCallFn =
    dyn Fn(String, Value, String, Vec<String>, Option<Blob>) -> PeerCallFuture + Send + Sync;

/// How a connected PEER KERNEL is reached. `via` names the transport agent
/// carrying it (the root reflect reports it); the call delivers one call and
/// resolves with the peer's reply — or a refusal when the connection ends first.
///
/// `secret` is the item's marked paths, rooted at the item exactly as the peer's
/// item is rooted, so the transport puts them on the call frame verbatim and the
/// PEER masks its own records of the call too (python `PeerRoute.call(agent_id,
/// payload, caller, secret)`).
///
/// Held as an `Arc`: a route is compared by IDENTITY (python `is`), so only the
/// connection that registered a guid can unregister it.
pub struct PeerRoute {
    /// The transport agent carrying this peer.
    pub via: String,
    call: Box<PeerCallFn>,
}

impl PeerRoute {
    /// A route through `via` whose calls `call` delivers.
    pub fn new<F>(via: impl Into<String>, call: F) -> Arc<Self>
    where
        F: Fn(String, Value, String, Vec<String>, Option<Blob>) -> PeerCallFuture
            + Send
            + Sync
            + 'static,
    {
        Arc::new(Self {
            via: via.into(),
            call: Box::new(call),
        })
    }

    /// Deliver one call through this route.
    pub fn call(
        &self,
        agent_id: String,
        payload: Value,
        caller: String,
        secret: Vec<String>,
        body: Option<Blob>,
    ) -> PeerCallFuture {
        (self.call)(agent_id, payload, caller, secret, body)
    }
}

/// An agent on a CONNECTED PEER KERNEL, as ONE send sees it — python's
/// `RemoteTarget`. Carried on an unregistered [`Agent`] so the send pipeline
/// treats it like any target (a ledger row, a task-table row, a settled outcome),
/// except that its dispatch hands the call to the peer's route instead of a
/// handler here, carrying the caller as `<this guid>|<sender>` so the peer's gate
/// can judge it, and the item's `secret` marks so the peer masks its records as
/// this kernel does.
pub(crate) struct RemoteRef {
    /// The peer kernel's guid.
    pub(crate) guid: String,
    /// The agent's id on that kernel.
    pub(crate) agent_id: String,
    /// The route the send hands the call to.
    pub(crate) route: Arc<PeerRoute>,
}

/// The kernel's peer table — guid → route, SORTED so `peer_rows` reads in
/// python's `sorted(self.peers.items())` order.
pub(crate) type PeerTable = std::sync::Mutex<BTreeMap<String, Arc<PeerRoute>>>;

impl Kernel {
    fn peer_table(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<PeerRoute>>> {
        self.peers.lock().expect("peers lock poisoned")
    }

    /// Make `<guid>|<agent>` reachable through `route`. Refused (the refusal
    /// returned) when that guid is already connected through ANOTHER route —
    /// re-pointing a live address silently would hand one kernel's calls to
    /// another; the second connection is told so instead.
    pub fn register_peer(&self, guid: &str, route: &Arc<PeerRoute>) -> Option<Value> {
        let mut table = self.peer_table();
        if let Some(held) = table.get(guid) {
            if !Arc::ptr_eq(held, route) {
                let mut r = crate::refusal::refusal(
                    crate::refusal::BAD_ARG,
                    format!(
                        "peer {guid} is already connected via {} — one kernel, one live \
                         route; close the other connection first",
                        held.via
                    ),
                );
                crate::refusal::with(&mut r, "arg", json!("guid"));
                return Some(r);
            }
        }
        table.insert(guid.to_string(), Arc::clone(route));
        None
    }

    /// Forget `guid` — only if `route` is still the one registered.
    pub fn unregister_peer(&self, guid: &str, route: &Arc<PeerRoute>) {
        let mut table = self.peer_table();
        if table.get(guid).is_some_and(|held| Arc::ptr_eq(held, route)) {
            table.remove(guid);
        }
    }

    /// The route `guid` is connected through, if it is connected.
    #[must_use]
    pub fn peer(&self, guid: &str) -> Option<Arc<PeerRoute>> {
        self.peer_table().get(guid).cloned()
    }

    /// The connected peer kernels, for the root reflect: `[{guid, via}]`.
    #[must_use]
    pub fn peer_rows(&self) -> Value {
        Value::Array(
            self.peer_table()
                .iter()
                .map(|(g, r)| json!({"guid": g, "via": r.via}))
                .collect(),
        )
    }

    /// This kernel's own guid — the root record's `guid`, once its loader minted
    /// it. `None` before the root has booted.
    pub(crate) fn own_guid(&self) -> Option<String> {
        self.root().and_then(|r| {
            r.meta
                .read()
                .expect("meta lock poisoned")
                .get("guid")
                .and_then(Value::as_str)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
        })
    }

    /// `<guid>|<agent id>`: this kernel's own guid names a LOCAL agent; the guid
    /// of a CONNECTED PEER names an agent there (a remote target the send hands
    /// to that peer's route); anything else names nothing. Python
    /// `Agent._qualified_target`.
    pub(crate) fn qualified_target(&self, target_id: &str) -> Option<Arc<Agent>> {
        let (guid, agent_id) = target_id.split_once('|')?;
        if guid.is_empty() || agent_id.is_empty() || agent_id.contains('|') {
            return None;
        }
        if self.own_guid().as_deref() == Some(guid) {
            return if agent_id == "kernel" {
                self.root()
            } else {
                self.agents
                    .get(&AgentId::from(agent_id))
                    .map(|e| Arc::clone(&e))
            };
        }
        let route = self.peer(guid)?;
        Some(Agent::remote(
            AgentId::from(target_id),
            RemoteRef {
                guid: guid.to_string(),
                agent_id: agent_id.to_string(),
                route,
            },
        ))
    }
}

/// The dispatch of a call to a remote target: hand it to the peer's route,
/// carrying the caller as `<this guid>|<sender>` so the peer's gate can judge it,
/// and the item's `secret` marks so the peer masks its records too. Python
/// `RemoteTarget._dispatch_task`.
pub(crate) async fn call_peer(
    kernel: &Kernel,
    remote: &RemoteRef,
    qualified: &AgentId,
    payload: Value,
    body: Option<Blob>,
) -> PeerReply {
    let Some(own) = kernel.own_guid() else {
        // The peer judges the caller by `<guid>|<agent>`; with no guid of our own
        // there is nothing honest to put on the frame.
        return (
            crate::refusal::refusal(
                crate::refusal::UNHANDLED,
                format!(
                    "send: this kernel has no guid yet (its root has not booted), so a \
                     call to {qualified} cannot say who is calling"
                ),
            ),
            None,
        );
    };
    let origin = crate::send::current_origin();
    let caller = format!("{own}|{}", origin.as_ref().map_or("kernel", |o| o.as_str()));
    // The item's `secret` marks ride along — this runs inside the dispatch's own
    // scope, so they are exactly the marks of THIS call. This kernel already
    // records the call as shown, and the peer must too: its call/settle rows,
    // ledger and state stream are records of the SAME call.
    let secret = crate::send::current_secret();
    remote
        .route
        .call(remote.agent_id.clone(), payload, caller, secret, body)
        .await
}

/// A remote target's meta — python's `{"callable_by": {}}`: it accepts nothing
/// HERE. Whether it accepts a call is its own kernel's decision.
pub(crate) fn remote_meta() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("callable_by".to_string(), json!({}));
    m
}
