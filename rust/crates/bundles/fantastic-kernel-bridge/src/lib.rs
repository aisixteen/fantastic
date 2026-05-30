//! kernel_bridge — cross-kernel comms relay.
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
//! | `reflect` | none | `{id, sentence, transport, connected, host?, port?, peer_id?, pending_count, verbs, emits}` |
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
//! Matches Python's `kernel_bridge` + `fantastic-web`'s WS server:
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

pub mod transport;

use transport::memory::MemoryTransport;
#[cfg(feature = "full")]
use transport::ssh::SshTransport;
use transport::ws::WsTransport;
use transport::{BridgeTransport, TransportError};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "kernel_bridge.tools";

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
    pub(crate) pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    pub(crate) corr_counter: AtomicU64,
}

impl BridgeState {
    fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending poisoned").len()
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The cross-kernel bridge bundle.
pub struct KernelBridgeBundle;

#[async_trait]
impl Bundle for KernelBridgeBundle {
    fn name(&self) -> &str {
        "kernel_bridge"
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
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" => boot_reply(agent_id, kernel).await,
            "shutdown" => shutdown_reply(agent_id, kernel).await,
            "reconnect" => reconnect_reply(agent_id, kernel).await,
            "forward" => forward_reply(agent_id, payload, kernel).await,
            "watch_remote" => watch_remote_reply(agent_id, payload, "watch").await,
            "unwatch_remote" => watch_remote_reply(agent_id, payload, "unwatch").await,
            other => json!({"error": format!("kernel_bridge: unknown verb {other:?}")}),
        };
        Ok(Some(reply))
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
    json!({
        "id": agent_id.as_str(),
        "sentence": "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); memory/ws/ssh+ws; weak proxy.",
        "transport": transport_kind,
        "connected": connected,
        "host": meta_string(agent_id, kernel, "host"),
        "port": meta_u64(agent_id, kernel, "port"),
        "peer_id": meta_string(agent_id, kernel, "peer_id"),
        "local_port": meta_u64(agent_id, kernel, "local_port"),
        "remote_port": meta_u64(agent_id, kernel, "remote_port"),
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

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    // Idempotent: re-booting a connected bridge is a no-op.
    if let Some(existing) = BRIDGES.lock().get(agent_id).cloned() {
        return json!({"already": true, "transport": existing.transport_kind});
    }

    let kind = meta_string(agent_id, kernel, "transport").unwrap_or_else(|| "memory".to_string());
    let transport: Arc<dyn BridgeTransport> = match kind.as_str() {
        "memory" => match take_injected(agent_id) {
            Some(t) => t,
            None => {
                return json!({
                    "error": "kernel_bridge: memory transport requires inject_pair (test seam)"
                })
            }
        },
        "ws" => {
            let peer_id = match meta_string(agent_id, kernel, "peer_id") {
                Some(p) => p,
                None => return json!({"error": "kernel_bridge: ws transport requires peer_id"}),
            };
            // Canonical field is `local_port` (Python parity);
            // accept `remote_port`/`port` as fallbacks.
            let port = match meta_u64(agent_id, kernel, "local_port")
                .or_else(|| meta_u64(agent_id, kernel, "remote_port"))
                .or_else(|| meta_u64(agent_id, kernel, "port"))
            {
                Some(p) => p as u16,
                None => return json!({"error": "kernel_bridge: ws transport requires local_port"}),
            };
            let host =
                meta_string(agent_id, kernel, "host").unwrap_or_else(|| "localhost".to_string());
            let url = format!("ws://{host}:{port}/{peer_id}/ws");
            match WsTransport::connect(&url).await {
                Ok(t) => t,
                Err(e) => return json!({"error": format!("kernel_bridge: ws connect failed: {e}")}),
            }
        }
        #[cfg(feature = "full")]
        "ssh+ws" => {
            let peer_id = match meta_string(agent_id, kernel, "peer_id") {
                Some(p) => p,
                None => return json!({"error": "kernel_bridge: ssh+ws transport requires peer_id"}),
            };
            let host = match meta_string(agent_id, kernel, "host") {
                Some(h) => h,
                None => return json!({"error": "kernel_bridge: ssh+ws transport requires host"}),
            };
            let remote_port = match meta_u64(agent_id, kernel, "remote_port") {
                Some(p) if p > 0 && p <= u16::MAX as u64 => p as u16,
                _ => {
                    return json!({
                        "error": "kernel_bridge: ssh+ws transport requires remote_port"
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
                Err(e) => return json!({"error": format!("kernel_bridge: ssh+ws failed: {e}")}),
            }
        }
        #[cfg(not(feature = "full"))]
        "ssh+ws" => {
            return json!({
                "error": "kernel_bridge: ssh+ws transport requires the `full` feature"
            })
        }
        other => return json!({"error": format!("kernel_bridge: unknown transport {other:?}")}),
    };

    let state = Arc::new(BridgeState {
        transport: Arc::clone(&transport),
        transport_kind: kind.clone(),
        read_task: AsyncMutex::new(None),
        pending: Mutex::new(HashMap::new()),
        corr_counter: AtomicU64::new(0),
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
    let pending: Vec<oneshot::Sender<Result<Value, String>>> = {
        let mut p = state.pending.lock().expect("pending poisoned");
        p.drain().map(|(_k, v)| v).collect()
    };
    for tx in pending {
        let _ = tx.send(Err("bridge shut down".to_string()));
    }
    json!({"stopped": true})
}

async fn reconnect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let _ = shutdown_reply(agent_id, kernel).await;
    boot_reply(agent_id, kernel).await
}

async fn forward_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let target = payload.get("target").and_then(Value::as_str).unwrap_or("");
    let inner = payload.get("payload");
    if target.is_empty() || !inner.map(Value::is_object).unwrap_or(false) {
        return json!({
            "error": "kernel_bridge.forward: target (str) + payload (object) required"
        });
    }
    let inner = inner.cloned().unwrap_or(Value::Null);
    let timeout_secs = payload
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS);

    let state = match BRIDGES.lock().get(agent_id).cloned() {
        Some(s) => s,
        None => {
            return json!({
                "error": "kernel_bridge.forward: not connected (call boot first)"
            })
        }
    };

    let _ = kernel; // peer_id no longer wraps the frame (asymmetric raw call)
    let n = state.corr_counter.fetch_add(1, Ordering::SeqCst) + 1;
    let corr = format!("{}:{}", agent_id, n);

    let (tx, rx) = oneshot::channel::<Result<Value, String>>();
    state
        .pending
        .lock()
        .expect("pending poisoned")
        .insert(corr.clone(), tx);

    // Raw call frame straight to the remote's web_ws — no `forward`
    // envelope, no peer_id addressing (asymmetric; matches the canonical Python kernel).
    let frame = json!({
        "type": "call",
        "id": corr,
        "target": target,
        "payload": inner,
    });
    if let Err(e) = state.transport.send_frame(frame).await {
        state
            .pending
            .lock()
            .expect("pending poisoned")
            .remove(&corr);
        return json!({"error": format!("kernel_bridge.forward: send failed: {e}")});
    }

    // Wait for the reply with a timeout. On EITHER timeout OR
    // close-rejection we clean the pending slot so it doesn't leak.
    let dur = std::time::Duration::from_secs_f64(timeout_secs.max(0.001));
    match tokio::time::timeout(dur, rx).await {
        Ok(Ok(Ok(v))) => v,
        Ok(Ok(Err(e))) => {
            state
                .pending
                .lock()
                .expect("pending poisoned")
                .remove(&corr);
            json!({"error": format!("kernel_bridge.forward: {e}")})
        }
        Ok(Err(_canceled)) => {
            state
                .pending
                .lock()
                .expect("pending poisoned")
                .remove(&corr);
            json!({"error": "kernel_bridge.forward: pending dropped"})
        }
        Err(_elapsed) => {
            state
                .pending
                .lock()
                .expect("pending poisoned")
                .remove(&corr);
            json!({"error": format!("kernel_bridge.forward: timeout after {timeout_secs}s")})
        }
    }
}

/// `watch_remote` / `unwatch_remote`: send `{type:"watch"|"unwatch",
/// src:target}` over the transport. Inbound `event` frames arrive via
/// the read loop and are re-emitted on this bridge agent's inbox.
async fn watch_remote_reply(agent_id: &AgentId, payload: &Value, kind: &str) -> Value {
    let target = payload.get("target").and_then(Value::as_str).unwrap_or("");
    if target.is_empty() {
        return json!({"error": format!("kernel_bridge.{kind}_remote: target (str) required")});
    }
    let state = match BRIDGES.lock().get(agent_id).cloned() {
        Some(s) => s,
        None => {
            return json!({
                "error": format!("kernel_bridge.{kind}_remote: not connected (call boot first)")
            })
        }
    };
    let frame = json!({ "type": kind, "src": target });
    if let Err(e) = state.transport.send_frame(frame).await {
        return json!({"error": format!("kernel_bridge.{kind}_remote: send failed: {e}")});
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
                let reply = if target.is_empty() {
                    json!({"error": "kernel_bridge: empty call target"})
                } else {
                    kernel.send(&AgentId::from(target.as_str()), inner).await
                };
                let _ = state
                    .transport
                    .send_frame(json!({
                        "type": "reply",
                        "id": corr_id,
                        "data": reply,
                    }))
                    .await;
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
                    let _ = tx.send(Ok(data));
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
    let drained: Vec<oneshot::Sender<Result<Value, String>>> = {
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
