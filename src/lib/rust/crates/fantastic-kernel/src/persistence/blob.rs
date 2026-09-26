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
const RESERVED_BLOB_NAMES: [&str; 2] = [crate::persistence::RECORD_FILE, "readme.md"];

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
            "name {} must be a single filename (no path segments)",
            fantastic_bundle::py_repr(name)
        ));
    }
    if name.starts_with('.') || name.contains("..") {
        return Err(format!(
            "name {} must not be hidden or contain '..'",
            fantastic_bundle::py_repr(name)
        ));
    }
    if RESERVED_BLOB_NAMES.contains(&name.to_lowercase().as_str()) {
        return Err(format!(
            "name {} is reserved (kernel-managed)",
            fantastic_bundle::py_repr(name)
        ));
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
            "{} may not use another agent's namespace {} — an agent's blob \
             verbs are scoped to itself; only an operator may name a namespace \
             it does not own",
            fantastic_bundle::py_repr(&caller.0),
            fantastic_bundle::py_repr(aid)
        )),
    }
}

/// Store-relative dir for an agent's sidecars — `agents/<id>`, or `""` for the
/// root itself. Prefers the LIVE agent's own `root_path`; a record that is no
/// longer (or not yet) live resolves by walking its parent chain, as python's
/// `_blob_reldir` → `_resolve_path` does.
fn blob_reldir(kernel: &Arc<Kernel>, aid: &str) -> Option<String> {
    let root = kernel.root()?;
    if let Some(agent) = kernel.agents.get(&AgentId::from(aid)) {
        return Some(super::store_reldir(&root.root_path, &agent.root_path));
    }
    let abs = super::resolve_record_path(kernel, aid, None);
    Some(super::store_reldir(&root.root_path, &abs))
}

/// A BARE `{error}`, python's shape. The blob verbs are the loader BUNDLE's, not
/// the kernel's, and python's bundle returns plain dicts with no refusal code:
/// `{"error": "<verb>: <why>"}`, plus `reason` only where python sets one.
fn err(verb: &str, why: impl std::fmt::Display) -> Value {
    json!({ "error": format!("{verb}: {why}") })
}

/// Shared preamble: validate, authorize, resolve the path.
///
/// PYTHON'S ORDER. `persist_blob` checks `content` BETWEEN the owner guard and
/// the store lookup, so a bad payload is named before "no store wired" — the
/// caller is told what IT got wrong first. `content` is the caller's to pass, so
/// the check rides in `persist_blob` and this returns before the store lookup.
fn resolve(kernel: &Arc<Kernel>, verb: &str, payload: &Value) -> Result<(AgentId, String), Value> {
    guard_args(verb, payload)?;
    let aid = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(verb, "agent_id (str) required"))?;
    let name = guard_name(payload.get("name").and_then(Value::as_str)).map_err(|e| err(verb, e))?;
    if let Err(e) = guard_owner(aid) {
        let mut out = err(verb, e);
        out["reason"] = json!("unauthorized");
        return Err(out);
    }
    let store = super::find_store(kernel)
        .ok_or_else(|| err(verb, "no store wired — persistence not available"))?;
    let reldir = blob_reldir(kernel, aid).ok_or_else(|| err(verb, "no store wired"))?;
    let path = if reldir.is_empty() {
        name.to_string()
    } else {
        format!("{reldir}/{name}")
    };
    Ok((store, path))
}

/// agent_id → name → owner, python's order, shared by both verbs.
fn guard_args(verb: &str, payload: &Value) -> Result<(), Value> {
    let aid = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(verb, "agent_id (str) required"))?;
    guard_name(payload.get("name").and_then(Value::as_str)).map_err(|e| err(verb, e))?;
    if let Err(e) = guard_owner(aid) {
        let mut out = err(verb, e);
        out["reason"] = json!("unauthorized");
        return Err(out);
    }
    Ok(())
}

/// `persist_blob` — whole-file write of one sidecar.
pub async fn persist_blob(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    if let Err(e) = guard_args("persist_blob", payload) {
        return e;
    }
    // CONTENT BEFORE THE STORE (python's order): what the caller got wrong is
    // named before what the kernel does not have.
    let Some(content) = payload.get("content").and_then(Value::as_str) else {
        return err("persist_blob", "content (str) required");
    };
    let (store, path) = match resolve(kernel, "persist_blob", payload) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // The loader writing to its OWN discovered store is persistence bookkeeping —
    // a side effect of this already-authorized verb, not the loader reaching out
    // as an agent — so it runs with operator authority, like create_agent's boot.
    let (reply, _) = super::store_send_binary(
        kernel,
        &store,
        json!({"type": "write_stream", "path": path, "truncate": true}),
        content.as_bytes().to_vec(),
    )
    .await;
    if let Some(e) = reply
        .get("error")
        .filter(|v| fantastic_bundle::py_truthy(v))
    {
        let mut out = json!({
            "error": format!("persist_blob: {}", fantastic_bundle::py_str(e)),
        });
        // Carry the provider's own machine fields through — a gate denial on the
        // store must not be relabelled, and python copies each only when truthy.
        for k in ["reason", "hint"] {
            if let Some(v) = reply.get(k).filter(|v| fantastic_bundle::py_truthy(v)) {
                out[k] = v.clone();
            }
        }
        return out;
    }
    json!({
        "ok": true,
        "name": payload.get("name").cloned().unwrap_or(Value::Null),
        "size": reply.get("size").cloned().unwrap_or(Value::Null),
    })
}

/// Decode a sidecar STRICTLY. `errors="replace"` turned a non-text file into
/// U+FFFD mojibake and handed it back as `content` — the caller could not tell a
/// corrupt read from a real one. A sidecar is text by contract; anything else is
/// a refusal naming the byte.
fn decoded_blob(name: &Value, raw: Vec<u8>) -> Value {
    match String::from_utf8(raw) {
        Ok(text) => json!({ "name": name, "content": text }),
        Err(e) => json!({
            "error": format!(
                "load_blob: {} is not utf-8 at byte {}",
                fantastic_bundle::py_str(name),
                e.utf8_error().valid_up_to()
            ),
            "reason": "bad_encoding",
            "name": name,
            "content": Value::Null,
        }),
    }
}

/// `load_blob` — read one sidecar back.
///
/// `content: null` when the file is absent: a never-written sidecar reads empty
/// rather than erroring, because "nothing saved yet" is the normal first state.
///
/// PUMPED TO EOF. `read_stream` is a CHUNK verb, so a single call returns a
/// PREFIX of any larger sidecar; returning that prefix is data loss dressed as a
/// successful load. A store that reports no forward progress is said, not spun on.
pub async fn load_blob(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let (store, path) = match resolve(kernel, "load_blob", payload) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let name = payload.get("name").cloned().unwrap_or(Value::Null);
    let mut chunks: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    let mut last;
    loop {
        let (reply, body) = super::store_send_binary(
            kernel,
            &store,
            json!({"type": "read_stream", "path": path, "offset": offset}),
            Vec::new(),
        )
        .await;
        last = reply.clone();
        if reply.get("error").is_some() {
            break;
        }
        chunks.extend_from_slice(&body.unwrap_or_default());
        if reply.get("eof").and_then(Value::as_bool).unwrap_or(false) {
            return decoded_blob(&name, chunks);
        }
        match reply.get("next_offset").and_then(Value::as_u64) {
            Some(n) if n > offset => offset = n,
            _ => {
                return json!({
                    "error": format!(
                        "load_blob: {} stalled at offset {offset} — the store \
                         returned no forward progress",
                        fantastic_bundle::py_str(&name)
                    ),
                    "name": name,
                    "content": Value::Null,
                })
            }
        }
    }
    if !chunks.is_empty() {
        return decoded_blob(&name, chunks);
    }
    if last.get("reason").and_then(Value::as_str) == Some("unauthorized") {
        let mut out = json!({
            "error": format!(
                "load_blob: {}",
                last.get("error")
                    .map_or_else(|| "unauthorized".to_string(), fantastic_bundle::py_str)
            ),
            "reason": "unauthorized",
        });
        if let Some(h) = last.get("hint").filter(|v| fantastic_bundle::py_truthy(v)) {
            out["hint"] = h.clone();
        }
        return out;
    }
    json!({ "name": name, "content": Value::Null })
}

#[cfg(test)]
#[path = "blob/tests.rs"]
mod tests;
