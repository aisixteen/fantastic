//! `proxy_agent.tools` — host-implemented agent bundle.
//!
//! Every verb dispatched to a `proxy_agent` instance forwards to a
//! host implementation of [`ProxyAgentHost`] keyed by the agent's id.
//! Hosts live in the embedding app (Swift via UniFFI callback in
//! production; plain-Rust impls in tests). The bundle itself is
//! stateless — all per-agent state lives in the host.
//!
//! The primary use is **SwiftUI views as first-class agents** —
//! addressable, in the reflect tree, lifecycle-managed by standard
//! `create_agent` / `delete_agent`. The same mechanism cleanly
//! serves any host-driven feature: AppIntents bridges, Vision
//! adapters, Clipboard helpers, JavaScript runtimes, future things.
//! Naming is `ProxyAgent` because UI is one consumer, not the only
//! one.
//!
//! ## Verb behaviour
//!
//! Bundle defaults handle a few verbs for graceful-degrade; the rest
//! forward to `host.handle`:
//!
//! | verb | no host | with host |
//! |---|---|---|
//! | `reflect` | self-describes, `host_registered: false` | forward to host; overlay `host_registered: true` |
//! | `boot` | `{ok: true}` | call `host.on_boot()` + forward `boot` |
//! | `shutdown` | `{ok: true}` | forward to host |
//! | anything else | `{error, reason: "no_host"}` | forward to host |
//!
//! `Bundle::on_delete` (lifecycle hook fired during cascade-delete)
//! calls `host.on_delete()` if present + drops the host from the
//! per-agent-id registry.
//!
//! ## Two-way streaming
//!
//! UniFFI 0.29 doesn't allow nested callback interfaces, so host
//! callbacks are sync (`handle(payload_json) -> reply_json`). For
//! UI-originated streams back to the kernel, the host returns
//! `{queued: true, stream_id}` synchronously, then fires async
//! events via `Kernel::proxy_emit(agent_id, event_json)` (in
//! `fantastic-uniffi`). For sender-attributed UI-to-agent sends,
//! the UI calls `Kernel::send_json_as`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "proxy_agent.tools";

/// readme.md auto-seeded into the agent's dir on creation (Disk mode).
pub const README: &str = include_str!("readme.md");

// ── host trait ────────────────────────────────────────────────────

/// Trait the embedding host implements. Swift via UniFFI in
/// production; plain-Rust impls drive the unit tests.
///
/// All methods are sync — UniFFI 0.29 callback-interface methods
/// can't be async. Implementations that do async work internally
/// (SwiftUI updates on `MainActor`, network calls) should kick off
/// a task inside `handle` and return a sync ack. Async events back
/// to the kernel ride through `Kernel::proxy_emit` (in
/// `fantastic-uniffi`).
pub trait ProxyAgentHost: Send + Sync {
    /// Verb dispatch. JSON payload in, JSON reply out. The reply
    /// can be `{"ok":true}` as a fire-and-forget ack or a real
    /// response when the caller expects one (e.g. `reflect`).
    fn handle(&self, payload_json: String) -> String;

    /// Fired when the agent's `boot` verb dispatches AND a host is
    /// registered. Default: noop.
    fn on_boot(&self) {}

    /// Fired during cascade-delete (before the agent unregisters).
    /// Default: noop. The bundle ALSO drops this host from the
    /// global registry after this hook — there's no need to call
    /// `unregister_host` from inside.
    fn on_delete(&self) {}
}

// ── process-global registry ────────────────────────────────────────

/// Per-agent-id host map. Multiple proxy_agent instances in one
/// kernel (or across kernels in the same process) each have their
/// own entry.
type HostMap = HashMap<AgentId, Arc<dyn ProxyAgentHost>>;

static HOSTS: OnceLock<RwLock<HostMap>> = OnceLock::new();

fn hosts() -> &'static RwLock<HostMap> {
    HOSTS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Install a host for `agent_id`. Replaces any previously-registered
/// host for the same id.
pub fn register_host(agent_id: AgentId, host: Arc<dyn ProxyAgentHost>) {
    hosts()
        .write()
        .expect("hosts lock poisoned")
        .insert(agent_id, host);
}

/// Drop the host for `agent_id`. No-op if nothing was registered.
pub fn unregister_host(agent_id: &AgentId) {
    hosts()
        .write()
        .expect("hosts lock poisoned")
        .remove(agent_id);
}

/// Read the registered host for `agent_id`. `None` if no host has
/// been installed for that id yet.
pub fn host_for(agent_id: &AgentId) -> Option<Arc<dyn ProxyAgentHost>> {
    hosts()
        .read()
        .expect("hosts lock poisoned")
        .get(agent_id)
        .map(Arc::clone)
}

/// Drop every registered host. Primarily for tests — production code
/// rarely needs to detach in bulk.
pub fn clear_hosts() {
    hosts().write().expect("hosts lock poisoned").clear();
}

// ── bundle ─────────────────────────────────────────────────────────

/// The host-implemented agent bundle. Stateless — all per-agent
/// state lives in the host implementation, behind the global
/// registry.
#[derive(Debug, Default)]
pub struct ProxyAgentBundle;

impl ProxyAgentBundle {
    /// Construct a fresh bundle. Stateless — `Default` works too.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Bundle for ProxyAgentBundle {
    fn name(&self) -> &str {
        "proxy_agent"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let host = host_for(agent_id);
        let reply = match (verb, host) {
            // ── reflect: bundle self-describes if no host; otherwise
            //    forward + overlay host_registered.
            ("reflect", None) => default_reflect(agent_id, false),
            ("reflect", Some(h)) => merge_reflect(agent_id, &h, payload),

            // ── boot: graceful ok; fire host.on_boot if registered;
            //    forward the verb so hosts can also dispatch on it.
            ("boot", None) => json!({"ok": true, "host_registered": false}),
            ("boot", Some(h)) => {
                h.on_boot();
                let forwarded = forward_to_host(&h, payload);
                merge_ok(forwarded)
            }

            // ── shutdown: graceful ok; forward to host if registered.
            ("shutdown", None) => json!({"ok": true}),
            ("shutdown", Some(h)) => forward_to_host(&h, payload),

            // ── anything else: structured no_host error, or forward.
            (_, None) => json!({
                "error": format!("no host registered for proxy_agent {:?}", agent_id.as_str()),
                "reason": "no_host",
            }),
            (_, Some(h)) => forward_to_host(&h, payload),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // Lifecycle hook fires BEFORE the agent unregisters. Call the
        // host's hook first so it can clean up (cancel tasks, drop
        // SwiftUI bindings, etc.), THEN drop the host from the
        // registry. If no host registered, both are no-ops.
        if let Some(h) = host_for(agent_id) {
            h.on_delete();
        }
        unregister_host(agent_id);
        Ok(())
    }
}

// ── helpers ────────────────────────────────────────────────────────

fn default_reflect(agent_id: &AgentId, host_registered: bool) -> Value {
    let sentence = if host_registered {
        "Host-implemented agent. See host_registered + verbs above for behaviour."
    } else {
        "Host-implemented agent — no host registered yet. Verbs other than reflect/boot/shutdown will return {error, reason: \"no_host\"}."
    };
    json!({
        "id": agent_id.as_str(),
        "sentence": sentence,
        "kind": "proxy_agent",
        "host_registered": host_registered,
        "verbs": {
            "reflect": "Identity + host_registered probe. Host overrides shape if registered.",
            "boot": "Fire host.on_boot() if registered. Returns {ok: true}.",
            "shutdown": "Forward to host if registered. Returns {ok: true}.",
            "*": "Any other verb forwards to host.handle(payload_json) -> reply_json. No host = {error, reason: \"no_host\"}.",
        },
    })
}

/// Parse the host's reply JSON. Wraps malformed JSON in a structured
/// error so callers see a clean message instead of "[object Object]"
/// or panic noise.
fn forward_to_host(host: &Arc<dyn ProxyAgentHost>, payload: &Value) -> Value {
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let reply_str = host.handle(payload_str);
    match serde_json::from_str::<Value>(&reply_str) {
        Ok(v) => v,
        Err(e) => json!({
            "error": format!("proxy_agent host returned non-JSON: {e}"),
            "reason": "host_reply_malformed",
            "reply_raw": reply_str,
        }),
    }
}

/// Merge the host's reflect reply with the bundle's identity probes.
/// The host's fields win for everything EXCEPT `host_registered`,
/// which the bundle sets to `true` because — by definition — we just
/// found a host.
fn merge_reflect(agent_id: &AgentId, host: &Arc<dyn ProxyAgentHost>, payload: &Value) -> Value {
    let host_reply = forward_to_host(host, payload);
    let Value::Object(mut map) = host_reply else {
        // Host didn't return an object; surface the reply alongside
        // a synthetic reflect so the consumer still gets identity.
        let mut fallback = default_reflect(agent_id, true);
        if let Value::Object(m) = &mut fallback {
            m.insert("host_reply".to_string(), host_reply);
        }
        return fallback;
    };
    map.insert("host_registered".to_string(), json!(true));
    if !map.contains_key("id") {
        map.insert("id".to_string(), json!(agent_id.as_str()));
    }
    if !map.contains_key("kind") {
        map.insert("kind".to_string(), json!("proxy_agent"));
    }
    Value::Object(map)
}

/// Default `boot` reply if the host's forwarded reply wasn't an
/// object. Otherwise overlay `ok: true` so all boot replies have a
/// stable shape.
fn merge_ok(reply: Value) -> Value {
    if let Value::Object(mut map) = reply {
        map.entry("ok").or_insert(json!(true));
        return Value::Object(map);
    }
    json!({"ok": true, "host_reply": reply})
}

#[cfg(test)]
mod tests;
