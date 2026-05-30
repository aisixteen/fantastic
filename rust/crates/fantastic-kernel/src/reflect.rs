//! `reflect` verb — one uniform shape on every agent.
//!
//! A reflect reply is the ADDRESSED agent's identity:
//! `{id, sentence, display_name, parent_id, handler_module,
//! description?, ...flat meta}` (bundle agents answer it from their own
//! handler; bare agents via [`reflect_identity`]). The composable flags
//! are appended uniformly by [`apply_reflect_flags`] in `send`:
//!
//! - `tree=all|ids|none` (default `all`) — nested distilled subtree /
//!   flat descendant-id index / omitted.
//! - `bundles=all|ids|none` (default `none`) — `{name, handler_module}`
//!   catalog / names / omitted.
//! - `readme=true` (legacy `return_readme` honored) — attach the agent's
//!   readme.md (string or null).
//!
//! There is NO `primer`: transport/wire docs moved into the root readme
//! (`reflect readme=true`); `available_bundles` is now the `bundles` flag.

use crate::agent::Agent;
#[cfg(test)]
use crate::agent::AgentId;
use crate::kernel::Kernel;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Uniform reflect identity for a bare agent (the root, or any node with
/// no handler_module). Bundle agents build their own identity in their
/// handler; the substrate appends tree/bundles/readme to BOTH via
/// [`apply_reflect_flags`], so root is not special-cased.
pub fn reflect_identity(target: &Arc<Agent>) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    obj.insert("sentence".to_string(), json!(sentence_for(target)));
    obj.insert(
        "parent_id".to_string(),
        target
            .parent_id
            .as_ref()
            .map(|p| json!(p.0))
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "handler_module".to_string(),
        target
            .handler_module
            .as_ref()
            .map(|s| json!(s))
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "display_name".to_string(),
        target
            .display_name()
            .map(|s| json!(s))
            .unwrap_or_else(|| json!(target.id.0)),
    );
    if let Some(d) = target.description() {
        obj.insert("description".to_string(), json!(d));
    }
    // Flatten current meta into the reply for visibility.
    if let Ok(meta) = target.meta.read() {
        for (k, v) in meta.iter() {
            if obj.contains_key(k) {
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
    }
    Value::Object(obj)
}

fn sentence_for(target: &Arc<Agent>) -> &'static str {
    if target.parent_id.is_none() {
        "Fantastic kernel. Everything is reachable by sending messages to agents."
    } else {
        "Bare agent (no handler_module) — answers substrate verbs only."
    }
}

/// Append the composable reflect flags to any reflect reply — applied
/// uniformly to bare-agent and bundle reflects. `tree` defaults to
/// `all`, `bundles` to `none`, `readme` to false (legacy `return_readme`
/// also honored). Non-object replies (errors / null) pass through.
pub fn apply_reflect_flags(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    payload: &Value,
    reply: &mut Value,
) {
    let Some(obj) = reply.as_object_mut() else {
        return;
    };
    // `description` is a substrate meta field — surface it on every
    // reflect (bundle handlers don't know about it) unless already set.
    if !obj.contains_key("description") {
        if let Some(d) = target.description() {
            obj.insert("description".to_string(), json!(d));
        }
    }
    match payload.get("tree").and_then(Value::as_str).unwrap_or("all") {
        "all" => {
            obj.insert("tree".to_string(), tree_node(target));
        }
        "ids" => {
            obj.insert("tree".to_string(), json!(descendant_ids(target)));
        }
        _ => {} // "none" → omit
    }
    match payload
        .get("bundles")
        .and_then(Value::as_str)
        .unwrap_or("none")
    {
        "all" => {
            obj.insert(
                "bundles".to_string(),
                Value::Array(available_bundles(kernel)),
            );
        }
        "ids" => {
            let names: Vec<Value> = available_bundles(kernel)
                .into_iter()
                .filter_map(|b| b.get("name").cloned())
                .collect();
            obj.insert("bundles".to_string(), Value::Array(names));
        }
        _ => {} // "none" → omit
    }
    let want_readme = payload
        .get("readme")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || payload
            .get("return_readme")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if want_readme {
        let readme = std::fs::read_to_string(target.readme_file()).ok();
        obj.insert(
            "readme".to_string(),
            match readme {
                Some(s) => Value::String(s),
                None => Value::Null,
            },
        );
    }
}

/// Walk an agent and its descendants, producing a nested
/// `{id, parent_id, handler_module, display_name, description?,
/// children:[...]}` dict (the `tree=all` shape; children sorted by id).
pub fn tree_node(target: &Agent) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    if let Some(p) = target.parent_id.as_ref() {
        obj.insert("parent_id".to_string(), json!(p.0));
    } else {
        obj.insert("parent_id".to_string(), Value::Null);
    }
    obj.insert(
        "handler_module".to_string(),
        match &target.handler_module {
            Some(s) => json!(s),
            None => Value::Null,
        },
    );
    obj.insert(
        "display_name".to_string(),
        target
            .display_name()
            .map(|s| json!(s))
            .unwrap_or_else(|| json!(target.id.0)),
    );
    if let Some(d) = target.description() {
        obj.insert("description".to_string(), json!(d));
    }
    let mut children: Vec<Value> = target
        .children
        .iter()
        .map(|entry| tree_node(entry.value()))
        .collect();
    children.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("id").and_then(Value::as_str).unwrap_or(""))
    });
    obj.insert("children".to_string(), Value::Array(children));
    Value::Object(obj)
}

/// Flat id index of an agent + all descendants (DFS, self first,
/// children sorted by id). The cheap `tree=ids` tier.
pub fn descendant_ids(target: &Agent) -> Vec<String> {
    let mut out = vec![target.id.0.clone()];
    let mut kids: Vec<_> = target
        .children
        .iter()
        .map(|e| Arc::clone(e.value()))
        .collect();
    kids.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    for c in kids {
        out.extend(descendant_ids(&c));
    }
    out
}

/// The installable-bundle catalog as `{name, handler_module}`, sorted by
/// name. The `bundles=all` tier (`bundles=ids` maps to just the names).
fn available_bundles(kernel: &Arc<Kernel>) -> Vec<Value> {
    let mut out: Vec<Value> = kernel
        .bundles
        .iter()
        .map(|(handler_module, b)| {
            json!({
                "name": b.name(),
                "handler_module": handler_module,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    out
}

#[cfg(test)]
mod tests;
