//! `reflect` verb — one uniform shape on every agent.
//!
//! A reflect reply is the ADDRESSED agent's identity:
//! `{id, sentence, display_name, parent_id, handler_module,
//! description?, ...flat meta}` (bundle agents answer it from their own
//! handler; bare agents via [`reflect_identity`]). The composable flags
//! are appended uniformly by [`apply_reflect_flags`] in `send`:
//!
//! - `detail=distilled|full` (default distilled) — distilled carries the
//!   gated ROSTER and the `more` hint; full carries the wiring tree.
//! - `tree=all|ids` (default `all`; any other PRESENT value omits it) — the
//!   nested subtree / the flat descendant-id index, paged with `next`.
//! - `bundles=all|ids` (default none) — the INSTALLED `{name, handler_module}`
//!   catalog / its names, paged with `bundles_next`.
//! - `readme=true` — attach the agent's readme.md (string or null).
//!
//! Transport/wire docs live in the root readme (`reflect readme=true`).

use crate::agent::Agent;
#[cfg(test)]
use crate::agent::AgentId;
use crate::kernel::Kernel;
use crate::outcome::Status;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// PURPOSE, not wiring — the reflect keys a roster node copies beyond its
/// `sentence` (python `_PURPOSE_KEYS`).
const PURPOSE_KEYS: [&str; 1] = ["mode"];

// ─── keyed / tiered doc tree ───────────────────────────────────
// The root manual is a SHORT `readme.md` orientation + a `docs/` tree of topic
// files (nested dirs = tiers, `/` in the key). `reflect readme=index` lists the
// tree token-annotated; `reflect readme=<key>` drills one topic. Served from the
// bundle's compiled `readme()`/`docs()` — nothing is copied to disk.

/// ~tokens for a doc: ceil(chars/4), matching ai_core's estimate (CHARS, not
/// bytes — Unicode-safe so the advertised cost is stable across runtimes).
fn est_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// A topic's one-line descriptor: the leading `> ` line if present, else the
/// first prose line (skipping headings). Python `_read_desc` splits like
/// `str.splitlines()` (the `\x1c`-`\x1e`/`\x85`/U+2028/U+2029 boundaries
/// included, not just `\n`) and strips like `str.strip()` (python's wider
/// whitespace set).
fn read_desc(text: &str) -> String {
    let lines = fantastic_bundle::py_splitlines(text);
    for line in &lines {
        if let Some(rest) = line.strip_prefix("> ") {
            return fantastic_bundle::py_strip(rest).to_string();
        }
    }
    for line in &lines {
        let s = fantastic_bundle::py_strip(line);
        if !s.is_empty() && !s.starts_with('#') && !s.starts_with('>') {
            return s.to_string();
        }
    }
    String::new()
}

/// `\r\n` and a lone `\r` both become `\n` — python's universal-newlines text
/// mode, applied to every doc/readme read, so `tokens`/`desc` match python's.
fn translate_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Token-annotated topic index of a bundle's compiled `docs` table at the tier
/// under `prefix` (recursive; nesting preserved). Each entry `{key, desc,
/// tokens, children?}` — NO body. A tier is ordered as python orders the files
/// of a `docs/` dir: by `<stem>.md`. The docs ship IN the bundle and are never
/// copied to disk.
pub fn readme_index(docs: &[(&str, &str)], prefix: &str) -> Vec<Value> {
    let mut tier: Vec<(&str, &str)> = docs
        .iter()
        .filter_map(|(key, body)| {
            let stem = key.strip_prefix(prefix)?;
            (!stem.is_empty() && !stem.contains('/')).then_some((stem, *body))
        })
        .collect();
    tier.sort_by_key(|(stem, _)| format!("{stem}.md"));
    tier.into_iter()
        .map(|(stem, body)| {
            let body = translate_newlines(body);
            let key = format!("{prefix}{stem}");
            let mut entry = Map::new();
            entry.insert("key".to_string(), json!(key));
            entry.insert("desc".to_string(), json!(read_desc(&body)));
            entry.insert("tokens".to_string(), json!(est_tokens(&body)));
            let children = readme_index(docs, &format!("{key}/"));
            if !children.is_empty() {
                entry.insert("children".to_string(), Value::Array(children));
            }
            Value::Object(entry)
        })
        .collect()
}

/// Flatten every doc key the index lists (for an unknown-key error reply).
fn readme_keys(docs: &[(&str, &str)]) -> Vec<String> {
    fn walk(entries: &[Value], out: &mut Vec<String>) {
        for e in entries {
            if let Some(k) = e.get("key").and_then(Value::as_str) {
                out.push(k.to_string());
            }
            if let Some(ch) = e.get("children").and_then(Value::as_array) {
                walk(ch, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&readme_index(docs, ""), &mut out);
    out
}

/// Polymorphic `readme=<str>`: `"index"` → the token-annotated tree; a `<key>` →
/// that topic's body + its immediate sub-docs' index; unknown/escaping key →
/// `{error, keys}` (the reply stays well-formed). Python's `_readme_view`: the
/// key is checked LEXICALLY, then against the keys the index lists.
fn readme_view(docs: &[(&str, &str)], sel: &str) -> Value {
    if sel == "index" {
        return json!({ "index": readme_index(docs, "") });
    }
    let safe = sel.trim_matches('/').replace('\\', "/");
    if safe.is_empty() || safe.split('/').any(|p| matches!(p, "" | "." | "..")) {
        return json!({
            "error": format!("bad key {}", fantastic_bundle::py_repr(sel)),
            "keys": readme_keys(docs),
        });
    }
    let keys = readme_keys(docs);
    let Some(body) = keys
        .contains(&safe)
        .then(|| docs.iter().find(|(k, _)| *k == safe).map(|(_, b)| *b))
        .flatten()
    else {
        return json!({
            "error": format!("no doc {}", fantastic_bundle::py_repr(sel)),
            "keys": keys,
        });
    };
    let body = translate_newlines(body);
    let mut view = Map::new();
    view.insert("key".to_string(), json!(safe));
    view.insert("desc".to_string(), json!(read_desc(&body)));
    view.insert("tokens".to_string(), json!(est_tokens(&body)));
    view.insert("body".to_string(), json!(body));
    let children = readme_index(docs, &format!("{safe}/"));
    if !children.is_empty() {
        view.insert("children".to_string(), Value::Array(children));
    }
    Value::Object(view)
}

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
    obj.insert("display_name".to_string(), json!(target.name_or_id()));
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

/// `{id, sentence?, description?, mode?, children:[...]}` — the tree, carrying
/// what each agent is FOR instead of how it is wired (python `_roster`).
///
/// The sentence is ASKED FROM THE AGENT ITSELF: a node with a handler is
/// reflected through [`Kernel::internal_dispatch`] (a real call — row, task,
/// watcher echo — but no reflect flags, so a roster never recurses into
/// rosters); a bare node answers its identity directly.
///
/// A reflect that passed the gate returns the tree WHOLE — no per-node hiding.
///
/// `Err` carries a node's RAISED reflect: python's exception propagates out of
/// `_roster` and fails the whole reflect.
fn roster(
    kernel: Arc<Kernel>,
    a: Arc<Agent>,
    ignore: Arc<std::collections::HashSet<String>>,
) -> Pin<Box<dyn Future<Output = Result<Value, Value>> + Send>> {
    Box::pin(async move {
        let raw = if a.handler_module.is_some() {
            let (reply, status) = kernel
                .internal_dispatch(&a, &json!({"type": "reflect"}))
                .await;
            if status == Status::Failed {
                return Err(reply);
            }
            reply
        } else {
            reflect_identity(&a)
        };
        let mut node = Map::new();
        node.insert("id".to_string(), json!(a.id.0));
        if let Some(sentence) = raw
            .get("sentence")
            .filter(|v| fantastic_bundle::py_truthy(v))
        {
            node.insert("sentence".to_string(), sentence.clone());
        }
        // Operator-set purpose — more specific than the bundle sentence.
        if let Some(d) = a.description() {
            node.insert("description".to_string(), json!(d));
        }
        for k in PURPOSE_KEYS {
            if let Some(v) = raw.get(k).filter(|v| !v.is_null()) {
                node.insert(k.to_string(), v.clone());
            }
        }
        // A TASK LINE is state the agent was given (`agentic_resources` `spawn` /
        // `report`), not a purpose its bundle states: the tree reads it off the record,
        // so a lead sees its subtree's plan and progress in one `reflect tree=all`.
        if let Some(t) = a
            .meta
            .read()
            .ok()
            .and_then(|m| m.get("task").filter(|t| !t.is_null()).cloned())
        {
            node.insert("task".to_string(), t);
        }
        let mut children = Vec::new();
        for c in a.children_snapshot() {
            if ignore.contains(&c.id.0) {
                children.push(crate::docs::pruned_stub(&c));
                continue;
            }
            children.push(roster(Arc::clone(&kernel), c, Arc::clone(&ignore)).await?);
        }
        node.insert("children".to_string(), Value::Array(children));
        Ok(Value::Object(node))
    })
}

/// Append the composable reflect flags to any reflect reply — applied
/// uniformly to bare-agent and bundle reflects (python
/// `_apply_reflect_flags`). Non-object replies and refusals pass through.
///
/// Key order is python's: identity, `description`, gate fields, the root
/// frame, `tree`, `next`, `bundles`, `bundles_next`, `readme`. A reflect says
/// what an agent IS, never how to call it: verb docs are
/// the `docs` kernel verb ([`crate::docs`]).
///
/// `Err` = a roster node's reflect raised; the caller settles the whole
/// reflect `failed` with that reply.
pub async fn apply_reflect_flags(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    payload: &Value,
    reply: &mut Value,
) -> Result<(), Value> {
    let Some(obj) = reply.as_object_mut() else {
        return Ok(());
    };
    // A REFUSAL IS NOT A REFLECT. Decorating one with a tree
    // produces a reply that reads like a healthy reflect with a stray error
    // field, and a caller could not tell the two apart.
    if obj.contains_key("error") {
        return Ok(());
    }
    let full = payload.get("detail").and_then(Value::as_str) == Some("full");
    // `description` is a substrate meta field — surface it on every
    // reflect (bundle handlers don't know about it) unless already set.
    if !obj.contains_key("description") {
        if let Some(d) = target.description() {
            obj.insert("description".to_string(), json!(d));
        }
    }
    // THE CALL GATE on the agent's OWN reflect. A bundle agent builds its own
    // identity and would otherwise never report its authority, so an agent
    // could not read what it may call or who may call it — only discover it by
    // trying. Same two keys the distilled tree nodes carry.
    if let Ok(meta) = target.meta.read() {
        for k in ["can_call", "callable_by"] {
            if let Some(v) = meta.get(k) {
                obj.entry(k.to_string()).or_insert_with(|| v.clone());
            }
        }
    }
    // Kernel runtime identity + deployment context — surfaced on the ROOT
    // reflect so a client that hops to this kernel learns, in one round-trip:
    // which runtime (`runtime`), WHERE it runs (`env` — "container" when
    // launched from the image, else "host"), and which build (`version`).
    // env/version come from the optional FANTASTIC_ENV / FANTASTIC_VERSION
    // envs the container bakes in; RUN-scoped (never persisted to the portable
    // .fantastic workdir). Same field names + key order (runtime → env →
    // version) across all four runtimes.
    if target.parent_id.is_none() {
        obj.insert("runtime".to_string(), json!("rust"));
        // The kernels this one is CONNECTED to — each agent there is reachable
        // as `<guid>|<agent id>` (reflect `<guid>|kernel` for its tree).
        obj.insert("peers".to_string(), kernel.peer_rows());
        obj.insert(
            "env".to_string(),
            json!(std::env::var("FANTASTIC_ENV").unwrap_or_else(|_| "host".to_string())),
        );
        obj.insert(
            "version".to_string(),
            json!(std::env::var("FANTASTIC_VERSION").ok()),
        );
        // DUCK-TYPED ROOT EXTENSION (python `_agent.py:1697-1709`). The root's
        // handler_module — the kernel_state loader — contributes root-only
        // fields the substrate cannot compute without coupling to bundle logic:
        // `persistence: {provider}`, the instance `guid`, the doc-tree frame
        // and the `self_extension` shape. No bundle import here: the registry
        // already holds it, and a root whose bundle is not installed in this
        // runtime simply contributes nothing. A hook that RAISES fails the
        // reflect.
        if let Some(bundle) = target
            .handler_module
            .as_deref()
            .and_then(|hm| kernel.bundles.get(hm))
        {
            if let Some(extra) = bundle.reflect_root_extra(target, full, kernel)? {
                for (k, v) in extra {
                    obj.insert(k, v);
                }
            }
        }
    }
    // DECIDED BY KEY PRESENCE (python `payload.get("tree", "all")`): absent is
    // `all`; a present `all`/`ids` acts; any other present value — null, a bool,
    // a number, `none` — omits the tree.
    let tree_mode = match payload.get("tree") {
        None => Some("all"),
        Some(Value::String(t)) if t == "all" => Some("all"),
        Some(Value::String(t)) if t == "ids" => Some("ids"),
        _ => None,
    };
    if let Some(mode) = tree_mode {
        // SUBTREE FILTERS: `show_subtrees` answers with only
        // those branches, `ignore_subtrees` prunes each named branch to a stub
        // `{id, pruned: n}`; together, show first and ignore inside it.
        let (show, ignore) = match crate::docs::subtree_filter(kernel, payload, target) {
            Ok(f) => f,
            Err(refused) => {
                *reply = refused;
                return Ok(());
            }
        };
        let roots: Vec<Arc<Agent>> = if show.is_empty() {
            vec![Arc::clone(target)]
        } else {
            show.clone()
        };
        if mode == "all" {
            let mut nodes = Vec::new();
            for r in &roots {
                nodes.push(if ignore.contains(&r.id.0) {
                    crate::docs::pruned_stub(r)
                } else if full {
                    tree_node_pruned(r, &ignore)
                } else {
                    roster(Arc::clone(kernel), Arc::clone(r), Arc::new(ignore.clone())).await?
                });
            }
            let tree = if show.is_empty() {
                nodes.remove(0)
            } else {
                Value::Array(nodes)
            };
            obj.insert("tree".to_string(), tree);
        } else {
            // The FLAT id index is the one tree shape that is a plain list, so it
            // is the one that pages. The nested tree is never sliced — a page of
            // it would be a forest with no root. A pruned branch stands as its stub.
            let flat: Vec<Value> = roots
                .iter()
                .flat_map(|r| descendant_entries(r, &ignore))
                .collect();
            let (ids, next) = crate::paging::page(flat, payload, |v| match v {
                Value::String(s) => s.clone(),
                other => other
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
            obj.insert("tree".to_string(), Value::Array(ids));
            if let Some(next) = next {
                obj.insert("next".to_string(), json!(next));
            }
        }
    }
    // The catalog pages with the SAME `limit`/`after` as `tree=ids`, and names
    // its cursor `bundles_next` only when a further page exists.
    match payload.get("bundles") {
        Some(Value::String(b)) if b == "all" => {
            let (cat, next) = crate::paging::page(available_bundles(kernel, true), payload, |b| {
                b.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
            obj.insert("bundles".to_string(), Value::Array(cat));
            if let Some(next) = next {
                obj.insert("bundles_next".to_string(), json!(next));
            }
        }
        Some(Value::String(b)) if b == "ids" => {
            let names: Vec<String> = available_bundles(kernel, false)
                .iter()
                .filter_map(|b| b.get("name").and_then(Value::as_str).map(str::to_string))
                .collect();
            let (names, next) = crate::paging::page(names, payload, Clone::clone);
            obj.insert("bundles".to_string(), json!(names));
            if let Some(next) = next {
                obj.insert("bundles_next".to_string(), json!(next));
            }
        }
        _ => {}
    }
    // Polymorphic `readme`: `true` → the SHORT orientation readme.md (string or
    // null); `"index"` → the token-annotated topic tree; `"<key>"` → one topic's
    // body + sub-docs' index. Served from the agent's BUNDLE (compiled in), never
    // from a copy on disk.
    let bundle = target
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm));
    match payload.get("readme") {
        Some(Value::Bool(true)) => {
            let readme = bundle.and_then(|b| b.readme()).map(translate_newlines);
            obj.insert(
                "readme".to_string(),
                readme.map_or(Value::Null, Value::String),
            );
        }
        Some(Value::String(sel)) if !sel.is_empty() => {
            let docs = bundle.map_or(&[][..], |b| b.docs());
            obj.insert("readme".to_string(), readme_view(docs, sel));
        }
        _ => {}
    }
    Ok(())
}

/// Walk an agent and its descendants, producing a nested
/// `{id, parent_id, handler_module, display_name, description?, ...posture,
/// children:[...]}` dict — the `detail:full` wiring tree (python `_tree`),
/// children in creation order.
///
/// A reflect that passed the gate returns it WHOLE — no per-node hiding.
pub fn tree_node(target: &Arc<Agent>) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    tree_fields(target, &mut obj);
    // CREATION ORDER, not sorted: python walks `_children`, a plain dict.
    let children: Vec<Value> = target.children_snapshot().iter().map(tree_node).collect();
    obj.insert("children".to_string(), Value::Array(children));
    Value::Object(obj)
}

/// One reflectable node's fields after its `id` (python `_node_summary`).
fn tree_fields(target: &Agent, obj: &mut Map<String, Value>) {
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
    obj.insert("display_name".to_string(), json!(target.name_or_id()));
    if let Some(d) = target.description() {
        obj.insert("description".to_string(), json!(d));
    }
    // Wiring/posture meta surfaced inline (mirrors py _POSTURE_KEYS in kernel/_agent.py):
    // a client reads the IO landscape — which legs are open vs sealed-by-default, what a
    // bridge exposes — from ONE root reflect. Same keys + order across runtimes.
    if let Ok(meta) = target.meta.read() {
        for k in [
            "ingress_rule",
            "egress_rule",
            "auth",
            "root",
            // what a file_bridge exposes under its root
            "allow",
            // THE CALL GATE, made readable. Without these an agent cannot read
            // its own authority — only discover it by trying, which cost whole
            // passes of empirical probing every run. They also make the
            // replace-not-merge warning on `update_agent` actionable: you must
            // send the map WHOLE, so you must be able to read it.
            "can_call",
            "callable_by",
        ] {
            if let Some(v) = meta.get(k) {
                obj.insert(k.to_string(), v.clone());
            }
        }
    }
}

/// `tree_node` with `ignore`d branches pruned to their stub.
fn tree_node_pruned(target: &Arc<Agent>, ignore: &std::collections::HashSet<String>) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    tree_fields(target, &mut obj);
    let children: Vec<Value> = target
        .children_snapshot()
        .iter()
        .map(|c| {
            if ignore.contains(&c.id.0) {
                crate::docs::pruned_stub(c)
            } else {
                tree_node_pruned(c, ignore)
            }
        })
        .collect();
    obj.insert("children".to_string(), Value::Array(children));
    Value::Object(obj)
}

/// `descendant_ids` as reply entries: an id in `ignore` stands as its stub
/// `{id, pruned: n}`, its branch unwalked. Python `_descendant_ids(ignore)`.
fn descendant_entries(target: &Agent, ignore: &std::collections::HashSet<String>) -> Vec<Value> {
    if ignore.contains(&target.id.0) {
        return vec![crate::docs::pruned_stub(target)];
    }
    let mut out = vec![json!(target.id.0)];
    for c in target.children_snapshot() {
        out.extend(descendant_entries(&c, ignore));
    }
    out
}

/// Flat id index of an agent + all descendants (DFS, self first, children in
/// CREATION ORDER — python's dict walk). The cheap `tree=ids` tier.
pub fn descendant_ids(target: &Agent) -> Vec<String> {
    let mut out = vec![target.id.0.clone()];
    for c in target.children_snapshot() {
        out.extend(descendant_ids(&c));
    }
    out
}

/// The INSTALLABLE-bundle catalog as `{name, handler_module}`, sorted by
/// name (python's entry-point scan) — unlisted fixtures are not in it. The
/// `bundles=all` tier (`bundles=ids` maps to just the names).
///
/// With `schemas`, an entry also carries `schema` when its bundle publishes a
/// [`crate::bundle::Bundle::record_schema`] — what that KIND's record takes,
/// readable before any agent of it exists. Python
/// `_available_bundles(schemas=True)`; the cheap `bundles=ids` listing never asks.
fn available_bundles(kernel: &Arc<Kernel>, schemas: bool) -> Vec<Value> {
    let mut out: Vec<Value> = kernel
        .bundles
        .catalog()
        .map(|(handler_module, b)| {
            let mut entry = json!({
                "name": b.name(),
                "handler_module": handler_module,
            });
            if schemas {
                if let Some(sch) = b.record_schema() {
                    entry["schema"] = sch;
                }
            }
            entry
        })
        .collect();
    // FOLDER BUNDLES of this kernel's root, beside the installed ones — a refused
    // folder too, with its `error`.
    out.extend(kernel.bundles.folder_rows());
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
