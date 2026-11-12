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
/// hidden name. The argument's presence and type are the verb declaration's.
///
/// Defence in depth over the provider's own clamp, so a bad `name` fails AT the
/// loader with a loader-scoped message instead of surfacing as a deep fs error
/// the caller cannot act on.
fn guard_name(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("name must be a non-empty filename".to_string());
    }
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
            super::loader_dir(kernel, loader)
                .map_err(|e| -> BundleError { format!("RuntimeError: {e}").into() })?,
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

/// Shared preamble: the path rules, then the store, then the path.
///
/// `Ok(Err(reply))` = the bare `{error}` the verb answers with; `Err` = the walk
/// RAISED ([`blob_reldir`]).
fn resolve(
    kernel: &Arc<Kernel>,
    loader: &Agent,
    verb: &str,
    aid: &str,
    name: &str,
) -> Result<Result<(AgentId, String), Value>, BundleError> {
    if let Err(e) = guard_args(verb, aid, name) {
        return Ok(Err(e));
    }
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

/// The path rules of both verbs: the id BECOMES A DIRECTORY (`agents/<agent_id>/`),
/// one safe segment, or `../bundles/x` would write a folder bundle's `bundle.yaml`;
/// the name is one bare filename.
fn guard_args(verb: &str, aid: &str, name: &str) -> Result<(), Value> {
    if let Some(r) = crate::refusal::guard_agent_id_as(verb, "agent_id", aid) {
        return Err(r);
    }
    guard_name(name).map_err(|e| err(verb, e))?;
    Ok(())
}

/// `persist_blob` — whole-file write of one sidecar, through `loader`'s store.
///
/// # Errors
/// The walk to a record that is not live RAISED ([`blob_reldir`]).
pub async fn persist_blob(
    kernel: &Arc<Kernel>,
    loader: &Agent,
    agent_id: &str,
    name: &str,
    content: &str,
) -> Result<Value, BundleError> {
    let (store, path) = match resolve(kernel, loader, "persist_blob", agent_id, name)? {
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
    if let Some(e) = crate::refusal::error_text(&reply) {
        let mut out = json!({
            "error": format!("persist_blob: {e}"),
        });
        // Carry the provider's own machine fields through — a gate denial on the
        // store must not be relabelled, and python copies each only when truthy.
        for k in ["reason", "hint"] {
            if let Some(v) = reply.get(k).filter(|v| !v.is_null() && v.as_str() != Some("")) {
                out[k] = v.clone();
            }
        }
        return Ok(out);
    }
    Ok(json!({
        "ok": true,
        "name": name,
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
                "load_blob: {name} is not utf-8 at byte {}",
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
    agent_id: &str,
    name: &str,
) -> Result<Value, BundleError> {
    let (store, path) = match resolve(kernel, loader, "load_blob", agent_id, name)? {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let name = Value::String(name.to_string());
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
                        name = name
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
                crate::refusal::error_text(&last)
                    .unwrap_or_else(|| "unauthorized".to_string())
            ),
            "reason": "unauthorized",
        });
        if let Some(h) = last.get("hint").filter(|v| !v.is_null() && v.as_str() != Some("")) {
            out["hint"] = h.clone();
        }
        return Ok(out);
    }
    Ok(json!({ "name": name, "content": Value::Null }))
}

#[cfg(test)]
#[path = "blob/tests.rs"]
mod tests;
