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
//! - `readme=true` — attach the agent's readme.md (string or null).
//!
//! There is NO `primer`: transport/wire docs moved into the root readme
//! (`reflect readme=true`); `available_bundles` is now the `bundles` flag.

use crate::agent::Agent;
#[cfg(test)]
use crate::agent::AgentId;
use crate::kernel::Kernel;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;

// ─── keyed / tiered doc tree ───────────────────────────────────
// The root manual is a SHORT `readme.md` orientation + a `docs/` tree of topic
// files (nested dirs = tiers, `/` in the key). `reflect readme=index` lists the
// tree token-annotated; `reflect readme=<key>` drills one topic. Read from the
// seeded on-disk tree (fantastic_core::seed_root_readme), mirroring the readme.

/// ~tokens for a doc: ceil(chars/4), matching ai_core's estimate (CHARS, not
/// bytes — Unicode-safe so the advertised cost is stable across runtimes).
fn est_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// A topic's one-line descriptor: the leading `> ` line if present, else the
/// first prose line (skipping headings).
fn read_desc(text: &str) -> String {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("> ") {
            return rest.trim().to_string();
        }
    }
    for line in text.lines() {
        let s = line.trim();
        if !s.is_empty() && !s.starts_with('#') && !s.starts_with('>') {
            return s.to_string();
        }
    }
    String::new()
}

/// Token-annotated topic index under `dir` (recursive; nesting preserved). Each
/// entry `{key, desc, tokens, children?}` — NO body. Absent dir → empty.
fn readme_index(dir: &Path, prefix: &str) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        if !(path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md")) {
            continue;
        }
        let (Some(stem), Ok(body)) = (
            path.file_stem().and_then(|s| s.to_str()),
            std::fs::read_to_string(&path),
        ) else {
            continue;
        };
        let key = format!("{prefix}{stem}");
        let mut entry = Map::new();
        entry.insert("key".to_string(), json!(key));
        entry.insert("desc".to_string(), json!(read_desc(&body)));
        entry.insert("tokens".to_string(), json!(est_tokens(&body)));
        let sub = dir.join(stem);
        if sub.is_dir() {
            let children = readme_index(&sub, &format!("{key}/"));
            if !children.is_empty() {
                entry.insert("children".to_string(), Value::Array(children));
            }
        }
        entries.push(Value::Object(entry));
    }
    entries
}

/// Flatten every doc key (for an unknown-key error reply).
fn readme_keys(dir: &Path) -> Vec<String> {
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
    walk(&readme_index(dir, ""), &mut out);
    out
}

/// Polymorphic `readme=<str>`: `"index"` → the token-annotated tree; a `<key>` →
/// that topic's body + its immediate sub-docs' index; unknown/escaping key →
/// `{error, keys}` (the reply stays well-formed).
fn readme_view(dir: &Path, sel: &str) -> Value {
    if sel == "index" {
        return json!({ "index": readme_index(dir, "") });
    }
    let safe = sel.trim_matches('/').replace('\\', "/");
    if safe.is_empty() || safe.split('/').any(|c| c == ".." || c.is_empty()) {
        return json!({ "error": format!("bad key '{sel}'"), "keys": readme_keys(dir) });
    }
    let file = dir.join(format!("{safe}.md"));
    let Ok(body) = std::fs::read_to_string(&file) else {
        return json!({ "error": format!("no doc '{sel}'"), "keys": readme_keys(dir) });
    };
    let mut view = Map::new();
    view.insert("key".to_string(), json!(safe));
    view.insert("desc".to_string(), json!(read_desc(&body)));
    view.insert("tokens".to_string(), json!(est_tokens(&body)));
    view.insert("body".to_string(), json!(body));
    let sub = dir.join(&safe);
    if sub.is_dir() {
        let children = readme_index(&sub, &format!("{safe}/"));
        if !children.is_empty() {
            view.insert("children".to_string(), Value::Array(children));
        }
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
/// `all`, `bundles` to `none`, `readme` to false. Non-object replies
/// (errors / null) pass through.
pub fn apply_reflect_flags(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    payload: &Value,
    reply: &mut Value,
) {
    let Some(obj) = reply.as_object_mut() else {
        return;
    };
    // A REFUSAL IS NOT A REFLECT. Decorating one with kernel_verbs/tree/more
    // produced a reply that reads like a healthy reflect with a stray error
    // field, and a caller could not tell the two apart (2026-09-02).
    if obj.contains_key("error") {
        return;
    }
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
    // The KERNEL's own verbs — answered by the ROOT alone (they act on the TREE
    // and the kernel-wide task table), so they appear in no bundle's `verbs`
    // map. Advertised on EVERY agent, not just the root: root-only was tried
    // and reverted — a model that never reflects the root never learns they
    // exist, guesses, and gives up.
    obj.insert(
        "kernel_verbs".to_string(),
        crate::send::kernel_verbs_view(
            payload.get("detail").and_then(Value::as_str) == Some("full"),
        ),
    );
    // Kernel runtime identity + deployment context — surfaced on the ROOT
    // reflect so a client that hops to this kernel learns, in one round-trip:
    // which runtime (`runtime`), WHERE it runs (`env` — "container" when
    // launched from the image, else "host"), and which build (`version`).
    // env/version come from the optional FANTASTIC_ENV / FANTASTIC_VERSION
    // envs the container bakes in; RUN-scoped (never persisted to the portable
    // .fantastic workdir). Same field names + key order (runtime → env →
    // version) across all four runtimes.
    if target.parent_id.is_none() {
        // Stable kernel instance guid — PERSISTENCE-scoped (sourced from the
        // persisted root meta, minted once at boot), unlike runtime/env/version
        // which are run-scoped. Always present: disk boot reads it back, an
        // in-memory boot mints an ephemeral one.
        if let Ok(meta) = target.meta.read() {
            if let Some(guid) = meta.get("guid").and_then(Value::as_str) {
                obj.insert("guid".to_string(), json!(guid));
            }
        }
        obj.insert("runtime".to_string(), json!("rust"));
        obj.insert(
            "env".to_string(),
            json!(std::env::var("FANTASTIC_ENV").unwrap_or_else(|_| "host".to_string())),
        );
        obj.insert(
            "version".to_string(),
            json!(std::env::var("FANTASTIC_VERSION").ok()),
        );
        // WHICH file_bridge the loader auto-persists records THROUGH (the
        // discovered store), or null = nothing wired (state in RAM). With the
        // provider's posture inline in the tree, a client sees in one reflect
        // whether persistence is wired AND whether the wired leg is open.
        obj.insert(
            "persistence".to_string(),
            json!({ "provider": crate::persistence::find_store(kernel).map(|id| id.0) }),
        );
        // The EXECUTION MODEL rides here, not only the affordance. The SHAPE of a
        // durable program cannot be left to a readme, because a model that lacks it
        // writes something that cannot work — and the weak models are exactly the
        // ones that never follow the pointer to go read it. A durable program is a
        // FILE plus a command; there is NO `code` field (a program squeezed into a
        // JSON string is lost to one stray quote — the old `code` field's whole
        // failure). Word-for-word with python's full `self_extension`.
        obj.insert(
            "self_extension".to_string(),
            json!(
                "A DURABLE PROGRAM is a FILE you write plus a command that runs it — \
                 not a `code`-carrying agent. There is no `code` field: a program used \
                 to be embedded as a JSON string, which is exactly how a call gets lost \
                 to escaping. Assemble these THREE pieces and you have built something \
                 that persists and re-runs, with no `code` field at all:\n\
                 1. WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON \
                 string. Use the file leg's STREAM verbs — <type>write_stream</type> \
                 <path>…</path> <truncate>true</truncate> <bytes>…</bytes> to write, \
                 <type>read_stream</type> <path>…</path> <offset>0</offset> to read, one \
                 raw chunk at a time. `bytes` is VERBATIM: the program between \
                 `<bytes>` and `</bytes>` exactly as written, real newlines, nothing \
                 escaped. That is the only byte path; the string read/write verbs are \
                 gone.\n\
                 2. RUN IT: the executor is a general-purpose COMMAND runner. Reflect \
                 it, read its `allowed` regexes, then send \
                 <type>start</type> <command>path</command> <args><item>…</item></args>; \
                 poll <type>status</type>. \
                 It runs commands, not inline programs.\n\
                 3. RE-RUN: the file is durable on disk, so the SAME `start` command run \
                 again later re-executes the SAME program against whatever the inputs \
                 hold THEN — that is how a thing 'keeps itself up to date'.\n\
                 A WORKER THAT COMPUTES is an `exec_bridge.tools` agent (the EXISTING \
                 command runner) that owns a program file — you never author a new \
                 handler_module (you cannot add modules to the catalog and do not need \
                 to). 'A specialist with child workers' = a parent agent whose children \
                 are exec_bridge runners, each with its own program file.\n\
                 WIRING TO A SIBLING: you cannot raise your OWN authority, but if you \
                 hold `update_agent` (e.g. on the root) you may open ANY agent's ingress \
                 — including a sibling like `totals`/`readings` that is not your child. \
                 To let your worker call `totals`: grant the worker `can_call:{totals}` \
                 AND `update_agent id='totals'` to add the worker to `totals.callable_by` \
                 (send the WHOLE map — update replaces, not merges).\n\
                 A SPAWNED PROGRAM reaches agents over its control fd — a TWO-WAY socket \
                 (FANTASTIC_CTRL_FD): write {op:'send', rid, target, payload}; the reply \
                 comes back on the SAME fd as {op:'reply', rid, data}. It is not inline \
                 — read frames until one carries your `rid`, skipping event/inbox frames.\n\
                 COMPOSE the agents you were given for everything else (`list_agents`, \
                 reflect each, send). CREATE/DELETE plain agents (create_agent with a \
                 handler_module and grants, delete_agent) work as before. Read \
                 <type>reflect</type> <readme>self-extension</readme> — send it to \
                 'kernel' (the \
                 manual lives on the ROOT, not on any other agent) — for the fd wire + \
                 detail."
            ),
        );
        // Always-on doc-tree FRAME: `readmes` (the top-level token-annotated topic
        // index) + `docs` (a one-liner teaching keyed fetch). So the manual's map is
        // in front of a caller from the first root reflect — it drills with
        // readme=index / readme=<key> instead of eating a monolith.
        let docs_dir = target.root_path.join("docs");
        obj.insert(
            "readmes".to_string(),
            Value::Array(readme_index(&docs_dir, "")),
        );
        obj.insert(
            "docs".to_string(),
            json!(
                "This kernel's manual is a keyed doc tree. To read it, send 'kernel' \
                 a reflect with a string `readme` field: <type>reflect</type> \
                 <readme>index</readme> lists every topic with its ~token cost; \
                 <readme>key</readme> opens one (e.g. self-extension, streams); \
                 <readme>true</readme> is the short orientation. The field is named \
                 exactly `readme` (a string) - not 'readme_key'. Read only what a task \
                 needs."
            ),
        );
    }
    match payload.get("tree").and_then(Value::as_str).unwrap_or("all") {
        "all" => {
            obj.insert("tree".to_string(), tree_node(target));
        }
        "ids" => {
            // PAGED — the cheap id tier is the one most likely to be walked by a
            // caller with a small budget, so it is also the one that most needs
            // a bound. Same cursor law as `list_agents`/`ps`.
            let (ids, next) = crate::paging::page(descendant_ids(target), payload, Clone::clone);
            obj.insert("tree".to_string(), json!(ids));
            obj.insert("tree_next".to_string(), json!(next));
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
    // Polymorphic `readme`: `true` → the SHORT orientation readme.md (string or
    // null); `"index"` → the token-annotated topic tree; `"<key>"` → one topic's
    // body + sub-docs' index. Read from the seeded on-disk tree, one agent at a time.
    match payload.get("readme") {
        Some(Value::Bool(true)) => {
            let readme = std::fs::read_to_string(target.readme_file()).ok();
            obj.insert(
                "readme".to_string(),
                match readme {
                    Some(s) => Value::String(s),
                    None => Value::Null,
                },
            );
        }
        Some(Value::String(sel)) if !sel.is_empty() => {
            let docs_dir = target.root_path.join("docs");
            obj.insert("readme".to_string(), readme_view(&docs_dir, sel));
        }
        _ => {}
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
    // Wiring/posture meta surfaced inline (mirrors py _POSTURE_KEYS in kernel/_agent.py):
    // a client reads the IO landscape — which legs are open vs sealed-by-default, what a
    // consumer persists through — from ONE root reflect. Same keys + order across runtimes.
    if let Ok(meta) = target.meta.read() {
        for k in [
            "ingress_rule",
            "egress_rule",
            "auth",
            "root",
            "file_bridge_id",
            // THE CALL GATE, made readable. Without these an agent cannot read
            // its own authority — only discover it by trying, which cost whole
            // turns of empirical probing every run. They also make the
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
