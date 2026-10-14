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

use crate::agent::{Agent, AgentId};
use crate::bundle::BundleError;
use crate::kernel::Kernel;
use serde_json::{json, Value};
use std::sync::Arc;

/// Files the KERNEL manages. A sidecar may not overwrite them.
const RESERVED_BLOB_NAMES: [&str; 1] = [crate::persistence::RECORD_FILE];

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

/// Store-relative dir for an agent's sidecars — `agents/<id>`, or `""` for the
/// loader itself — relative to THE ADDRESSED LOADER's root (python
/// `_blob_reldir(agent, aid)`). Prefers the LIVE agent's own `root_path`; a
/// record that is no longer (or not yet) live resolves by walking its parent
/// chain under that loader, as python's `_resolve_path` does.
///
/// A WALK THAT REFUSES RAISES. Python's `_blob_reldir` → `_resolve_path` →
/// `_disk_parents` → `_walk` has no `try`, so an entry that escapes the loader's
/// root fails the verb (`ValueError: fs: path '<rel>' escapes root`, settled
/// `failed`) — it is not a reply the handler composed.
fn blob_reldir(kernel: &Arc<Kernel>, loader: &Agent, aid: &str) -> Result<String, BundleError> {
    let abs = match kernel.agents.get(&AgentId::from(aid)) {
        Some(agent) => agent.root_path.clone(),
        None => super::RecordPlaces::new(
            kernel,
            super::loader_dir(loader),
            super::loader_children(loader),
            loader.id.0.clone(),
        )
        .resolve(aid, None)
        .map_err(|e| -> BundleError {
            // A clamp refusal is a `ValueError`; the cwd's `OSError` leads with
            // `[Errno N]`, and its class follows from the errno.
            let class = e
                .strip_prefix("[Errno ")
                .and_then(|r| r.split(']').next())
                .and_then(|n| n.parse::<i32>().ok())
                .map_or("ValueError", fantastic_bundle::os_error_class);
            format!("{class}: {e}").into()
        })?,
    };
    Ok(super::loader_reldir(kernel, loader, &abs))
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
///
/// `Ok(Err(reply))` = the bare `{error}` the verb answers with; `Err` = the walk
/// RAISED ([`blob_reldir`]).
fn resolve(
    kernel: &Arc<Kernel>,
    loader: &Agent,
    verb: &str,
    payload: &Value,
) -> Result<Result<(AgentId, String), Value>, BundleError> {
    let (aid, name) = match guard_args(verb, payload) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    // THE ADDRESSED LOADER's store (python `_find_store(agent)`), never another's.
    let Some(store) = super::find_store_for(kernel, loader) else {
        return Ok(Err(err(verb, "no store wired — persistence not available")));
    };
    let reldir = blob_reldir(kernel, loader, aid)?;
    let path = if reldir.is_empty() {
        name.to_string()
    } else {
        format!("{reldir}/{name}")
    };
    Ok(Ok((store, path)))
}

/// agent_id → name → owner, python's order, shared by both verbs. Answers the
/// validated `(agent_id, name)`.
fn guard_args<'a>(verb: &str, payload: &'a Value) -> Result<(&'a str, &'a str), Value> {
    let aid = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(verb, "agent_id (str) required"))?;
    let name = guard_name(payload.get("name").and_then(Value::as_str)).map_err(|e| err(verb, e))?;
    Ok((aid, name))
}

/// `persist_blob` — whole-file write of one sidecar, through `loader`'s store.
///
/// # Errors
/// The walk to a record that is not live RAISED ([`blob_reldir`]).
pub async fn persist_blob(
    kernel: &Arc<Kernel>,
    loader: &Agent,
    payload: &Value,
) -> Result<Value, BundleError> {
    if let Err(e) = guard_args("persist_blob", payload) {
        return Ok(e);
    }
    // CONTENT BEFORE THE STORE (python's order): what the caller got wrong is
    // named before what the kernel does not have.
    let Some(content) = payload.get("content").and_then(Value::as_str) else {
        return Ok(err("persist_blob", "content (str) required"));
    };
    let (store, path) = match resolve(kernel, loader, "persist_blob", payload)? {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    // The loader writing to its OWN discovered store is persistence bookkeeping —
    // a side effect of this already-authorized verb, not the loader reaching out
    // as an agent — so the store's handler runs directly, like create_agent's boot.
    let reply = super::store_write(
        kernel,
        &store,
        json!({"type": "write_stream", "path": path, "bytes": null, "truncate": true}),
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
        return Ok(out);
    }
    Ok(json!({
        "ok": true,
        "name": payload.get("name").cloned().unwrap_or(Value::Null),
        "size": reply.get("size").cloned().unwrap_or(Value::Null),
    }))
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
///
/// # Errors
/// The walk to a record that is not live RAISED ([`blob_reldir`]).
pub async fn load_blob(
    kernel: &Arc<Kernel>,
    loader: &Agent,
    payload: &Value,
) -> Result<Value, BundleError> {
    let (store, path) = match resolve(kernel, loader, "load_blob", payload)? {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let name = payload.get("name").cloned().unwrap_or(Value::Null);
    let mut chunks: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    let mut last;
    loop {
        let (reply, body) = super::store_read(
            kernel,
            &store,
            json!({"type": "read_stream", "path": path, "offset": offset}),
        )
        .await;
        last = reply.clone();
        if reply.get("error").is_some() {
            break;
        }
        chunks.extend_from_slice(&body.unwrap_or_default());
        if reply.get("eof").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(decoded_blob(&name, chunks));
        }
        match reply.get("next_offset").and_then(Value::as_u64) {
            Some(n) if n > offset => offset = n,
            _ => {
                return Ok(json!({
                    "error": format!(
                        "load_blob: {} stalled at offset {offset} — the store \
                         returned no forward progress",
                        fantastic_bundle::py_str(&name)
                    ),
                    "name": name,
                    "content": Value::Null,
                }))
            }
        }
    }
    if !chunks.is_empty() {
        return Ok(decoded_blob(&name, chunks));
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
        return Ok(out);
    }
    Ok(json!({ "name": name, "content": Value::Null }))
}

#[cfg(test)]
#[path = "blob/tests.rs"]
mod tests;
