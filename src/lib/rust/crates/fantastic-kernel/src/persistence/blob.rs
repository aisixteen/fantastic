//! THE BLOB CHANNEL — an agent's sidecars, persisted THROUGH the loader.
//!
//! Port of python `kernel_state._persist_blob`/`_load_blob` (canonical),
//! `tmp/pychanges.md` #10.
//!
//! A dumb BYTE authority: no data semantics — the caller owns its format. What
//! the consumer gets is that it never computes a path and never wires a provider
//! of its own; it names ITSELF and a filename, and the loader puts the bytes at
//! `agents/<agent_id>/<name>` in whatever store is discovered.
//!
//! No store wired ⇒ an ERROR, not RAM. The tree's auto-flush law says "no
//! provider, no persistence"; a sidecar verb that silently succeeded into
//! nowhere would be the opposite of that — the caller believes it saved.

use crate::agent::AgentId;
use crate::kernel::Kernel;
use serde_json::{json, Value};
use std::sync::Arc;

/// Files the KERNEL manages. A sidecar may not overwrite them.
const RESERVED_BLOB_NAMES: [&str; 2] = ["agent.json", "readme.md"];

/// Validate a blob filename: one bare segment, not kernel-managed, no escape or
/// hidden name.
///
/// Defence in depth over the provider's own clamp, so a bad `name` fails AT the
/// loader with a loader-scoped message instead of surfacing as a deep fs error
/// the caller cannot act on.
fn guard_name(name: Option<&str>) -> Result<&str, String> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err("name (non-empty str) required".to_string());
    };
    if name.contains('/') || name.contains('\\') {
        return Err(format!(
            "name {name:?} must be a single filename (no path segments)"
        ));
    }
    if name.starts_with('.') || name.contains("..") {
        return Err(format!("name {name:?} must not be hidden or contain '..'"));
    }
    if RESERVED_BLOB_NAMES.contains(&name.to_lowercase().as_str()) {
        return Err(format!("name {name:?} is reserved (kernel-managed)"));
    }
    Ok(name)
}

/// Refuse a blob verb that names SOMEONE ELSE'S namespace.
///
/// `agent_id` is the namespace and it arrives in the PAYLOAD — so without this,
/// any agent granted `persist_blob` on the loader could write any OTHER agent's
/// sidecar. Measured 2026-08-23 (python): an agent holding exactly
/// `can_call: {kernel_state: [persist_blob]}` overwrote another agent's
/// `state.yaml`, and the victim read the forged content back as its own memory.
///
/// The defence was believed to be POSITION — the root accepts `$children`, and a
/// grandchild is not one — but a DIRECT child passes that check, and every agent
/// an operator creates at the root is a direct child. Position bounds who can
/// REACH the verb; it says nothing about which namespace they may NAME.
///
/// An OPERATOR send (origin `None`: the loader's own flush, an agent persisting
/// itself under operator authority, the CLI) is the bookkeeping this verb exists
/// for and stays ungated — that is the origin root of trust, and an agent cannot
/// forge it. An AGENT may only name ITSELF.
fn guard_owner(aid: &str) -> Result<(), String> {
    match crate::send::current_origin() {
        None => Ok(()),
        Some(caller) if caller.as_str() == aid => Ok(()),
        Some(caller) => Err(format!(
            "{caller} may not use another agent's namespace {aid:?} — an agent's \
             blob verbs are scoped to itself; only an operator may name a \
             namespace it does not own"
        )),
    }
}

/// Store-relative dir for an agent's sidecars — `agents/<id>`, or `""` for the
/// root itself.
fn blob_reldir(kernel: &Arc<Kernel>, aid: &str) -> Option<String> {
    let root = kernel.root()?;
    let agent = kernel.agents.get(&AgentId::from(aid))?;
    Some(super::store_reldir(&root.root_path, &agent.root_path))
}

fn refuse(verb: &str, why: String, unauthorized: bool) -> Value {
    let mut r = crate::refusal::refusal(
        if unauthorized {
            crate::refusal::UNAUTHORIZED
        } else {
            crate::refusal::BAD_ARG
        },
        format!("{verb}: {why}"),
    );
    if unauthorized {
        crate::refusal::with(&mut r, "side", json!("ingress"));
    }
    r
}

/// Shared preamble: validate, authorize, resolve the path.
fn resolve(kernel: &Arc<Kernel>, verb: &str, payload: &Value) -> Result<(AgentId, String), Value> {
    let aid = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| refuse(verb, "agent_id (str) required".into(), false))?;
    let name = guard_name(payload.get("name").and_then(Value::as_str))
        .map_err(|e| refuse(verb, e, false))?;
    guard_owner(aid).map_err(|e| refuse(verb, e, true))?;
    let store = super::find_store(kernel).ok_or_else(|| {
        crate::refusal::refusal(
            crate::refusal::NOT_HERE,
            format!("{verb}: no store wired — persistence not available"),
        )
    })?;
    let reldir = blob_reldir(kernel, aid).ok_or_else(|| {
        crate::refusal::unknown_agent(
            aid,
            kernel.agents.iter().map(|e| e.key().0.clone()).collect(),
        )
    })?;
    let path = if reldir.is_empty() {
        name.to_string()
    } else {
        format!("{reldir}/{name}")
    };
    Ok((store, path))
}

/// `persist_blob` — whole-file write of one sidecar.
pub(crate) async fn persist_blob(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let (store, path) = match resolve(kernel, "persist_blob", payload) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(content) = payload.get("content").and_then(Value::as_str) else {
        return refuse("persist_blob", "content (str) required".into(), false);
    };
    // The loader writing to its OWN discovered store is persistence bookkeeping —
    // a side effect of this already-authorized verb, not the loader reaching out
    // as an agent — so it runs with operator authority, like create_agent's boot.
    let (reply, _) = Box::pin(crate::send::with_operator(kernel.send_with_binary(
        &store,
        json!({"type": "write_stream", "path": path, "truncate": true}),
        content.as_bytes().to_vec(),
    )))
    .await;
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        let mut out = crate::refusal::refusal(
            crate::refusal::TRANSPORT_ERROR,
            format!("persist_blob: {err}"),
        );
        // Carry the provider's own machine field through — a gate denial on the
        // store must not be relabelled as a transport fault.
        if let Some(reason) = reply.get("reason") {
            out["reason"] = reason.clone();
        }
        if let Some(hint) = reply.get("hint") {
            out["hint"] = hint.clone();
        }
        return out;
    }
    json!({
        "ok": true,
        "name": payload.get("name").cloned().unwrap_or(Value::Null),
        "size": reply.get("size").cloned().unwrap_or(Value::Null),
    })
}

/// `load_blob` — read one sidecar back.
///
/// `content: null` when the file is absent: a never-written sidecar reads empty
/// rather than erroring, because "nothing saved yet" is the normal first state.
pub(crate) async fn load_blob(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let (store, path) = match resolve(kernel, "load_blob", payload) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let bytes = super::read_via_store(kernel, &store, &path).await;
    let name = payload.get("name").cloned().unwrap_or(Value::Null);
    if bytes.is_empty() {
        return json!({ "name": name, "content": Value::Null });
    }
    match String::from_utf8(bytes) {
        Ok(text) => json!({ "name": name, "content": text }),
        Err(_) => crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            "load_blob: sidecar is not valid UTF-8".to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "blob/tests.rs"]
mod tests;
