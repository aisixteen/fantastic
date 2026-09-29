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
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

/// The escalation line every DISTILLED reflect carries — python `_MORE_HINT`,
/// verbatim. Escalation rides in the reply itself, not in any caller's prompt.
pub const MORE_HINT: &str = "Distilled: every agent and what it is FOR. Reflect one agent for its \
verbs (<type>reflect</type>). Manual: <type>reflect</type> <readme>index</readme> - keys include \
system-verbs (ps/cancel/create_agent args) and self-extension (author your own durable tools). \
<detail>full</detail> on a reflect adds wiring and posture - ask for it only when you need to see \
how agents are plumbed, not to find one.";

/// PURPOSE, not wiring — the reflect keys a roster node copies beyond its
/// `sentence` (python `_PURPOSE_KEYS`).
const PURPOSE_KEYS: [&str; 1] = ["mode"];

/// `display_name or id` — python's truthiness: an empty name is no name.
fn display_name_or_id(target: &Agent) -> Value {
    match target.display_name() {
        Some(s) if !s.is_empty() => json!(s),
        _ => json!(target.id.0),
    }
}

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
/// first prose line (skipping headings). Python `_read_desc` splits like
/// `str.splitlines()` (the `\x1c`-`\x1e`/`\x85`/U+2028/U+2029 boundaries
/// included, not just `\n`) and strips like `str.strip()` (python's wider
/// whitespace set) — C59.
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

/// A doc file's raw bytes read in PYTHON TEXT MODE: UTF-8 decode, universal
/// newlines (`\r\n` and lone `\r` → `\n`) — mirrors
/// `pathlib.Path.read_text(encoding="utf-8")` (C59).
///
/// `Io` = the file could not be read at all (permission, races after an
/// `is_file()` check) — every `_readme_*` call site in python catches ONLY
/// `OSError` here and treats it as "no doc". `Decode` = the bytes are not
/// valid UTF-8: python's `except OSError` does NOT catch
/// `UnicodeDecodeError`, so this is NOT "unreadable" — it propagates out of
/// the verb and fails the WHOLE reflect (C60). Callers turn it into that
/// raise via [`decode_raise`].
enum DocRead {
    Ok(String),
    Io,
    Decode(String),
}

fn read_doc_text(path: &Path) -> DocRead {
    let Ok(bytes) = std::fs::read(path) else {
        return DocRead::Io;
    };
    match String::from_utf8(bytes) {
        Ok(s) => DocRead::Ok(translate_newlines(&s)),
        Err(e) => {
            let bytes = e.as_bytes();
            DocRead::Decode(utf8_error_text(bytes, &e.utf8_error()))
        }
    }
}

/// `\r\n` and a lone `\r` both become `\n` — python's universal-newlines text
/// mode, applied to every doc/readme read (C59; rust/swift used to KEEP the
/// CRs, so `tokens`/`desc` diverged from python's).
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

/// CPython's `UnicodeDecodeError` message for a `'utf-8' codec` failure:
/// `"'utf-8' codec can't decode byte 0x.. in position N: <reason>"` (C60).
fn utf8_error_text(bytes: &[u8], e: &std::str::Utf8Error) -> String {
    let pos = e.valid_up_to();
    let reason = match e.error_len() {
        None => "unexpected end of data",
        Some(1) => "invalid start byte",
        Some(_) => "invalid continuation byte",
    };
    let byte = bytes.get(pos).copied().unwrap_or(0);
    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {pos}: {reason}")
}

/// The reply python's dispatch builds for an uncaught exception — here a
/// `UnicodeDecodeError` propagating out of a doc read (C60). Same shape as
/// every other raised handler: `{"error": "<Class>: <message>", "raised":
/// true}`, which settles the whole call `failed`.
fn decode_raise(message: &str) -> Value {
    json!({"error": format!("UnicodeDecodeError: {message}"), "raised": true})
}

/// Token-annotated topic index under `dir` (recursive; nesting preserved). Each
/// entry `{key, desc, tokens, children?}` — NO body. Absent dir → empty.
///
/// `Err` = a doc under `dir` is not valid UTF-8 — python's `except OSError`
/// does not catch that, so scanning the tree (index, keys-on-error, a
/// directory view's children) raises and fails the whole reflect (C60).
pub fn readme_index(dir: &Path, prefix: &str) -> Result<Vec<Value>, Value> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        if !(path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md")) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let body = match read_doc_text(&path) {
            DocRead::Ok(s) => s,
            DocRead::Io => continue,
            DocRead::Decode(msg) => return Err(decode_raise(&msg)),
        };
        let key = format!("{prefix}{stem}");
        let mut entry = Map::new();
        entry.insert("key".to_string(), json!(key));
        entry.insert("desc".to_string(), json!(read_desc(&body)));
        entry.insert("tokens".to_string(), json!(est_tokens(&body)));
        let sub = dir.join(stem);
        if sub.is_dir() {
            let children = readme_index(&sub, &format!("{key}/"))?;
            if !children.is_empty() {
                entry.insert("children".to_string(), Value::Array(children));
            }
        }
        entries.push(Value::Object(entry));
    }
    Ok(entries)
}

/// Flatten every doc key (for an unknown-key error reply).
fn readme_keys(dir: &Path) -> Result<Vec<String>, Value> {
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
    walk(&readme_index(dir, "")?, &mut out);
    Ok(out)
}

/// Polymorphic `readme=<str>`: `"index"` → the token-annotated tree; a `<key>` →
/// that topic's body + its immediate sub-docs' index; unknown/escaping key →
/// `{error, keys}` (the reply stays well-formed).
///
/// PORT OF PYTHON'S `_readme_view`, r2 correction R2-RFL-5: `safe` is the RAW
/// (unrejected) string after `sel.strip("/").replace("\\", "/")`; `f = dir /
/// (safe + ".md")` by plain concatenation, so `Path::join`'s own rules apply
/// (an absolute `safe` REPLACES `dir` entirely — `\topic` → `/topic.md` →
/// escapes and is a `bad key`; `//` inside `safe` collapses like any POSIX
/// path). The CLAMP check is [`clamped_within`] (lexical, tolerates
/// nonexistent components); the EXISTENCE check is a real `is_file()` on the
/// UNRESOLVED `f` — the OS itself refuses to walk a `..` through a segment
/// that is not really a directory (`frontend/panel-recipe/../routing` is
/// `no doc` because `panel-recipe` is a FILE, not a dir, even though the
/// clamp check alone would have allowed it).
fn readme_view(dir: &Path, sel: &str) -> Result<Value, Value> {
    if sel == "index" {
        return Ok(json!({ "index": readme_index(dir, "")? }));
    }
    let safe = sel.trim_matches('/').replace('\\', "/");
    // LEXICAL, then the key list — never the filesystem (python 2026-09-20):
    // a `..` segment is a bad key whether or not the path it walked through
    // exists, and a key is a doc only if the index lists it.
    if safe.is_empty() || safe.split('/').any(|p| matches!(p, "" | "." | "..")) {
        return Ok(json!({
            "error": format!("bad key {}", fantastic_bundle::py_repr(sel)),
            "keys": readme_keys(dir)?,
        }));
    }
    let keys = readme_keys(dir)?;
    if !keys.contains(&safe) {
        return Ok(json!({
            "error": format!("no doc {}", fantastic_bundle::py_repr(sel)),
            "keys": keys,
        }));
    }
    let file = dir.join(format!("{safe}.md"));
    let body = match read_doc_text(&file) {
        DocRead::Ok(s) => s,
        DocRead::Io => {
            return Ok(json!({
                "error": format!("unreadable {}", fantastic_bundle::py_repr(sel)),
                "keys": readme_keys(dir)?,
            }))
        }
        DocRead::Decode(msg) => return Err(decode_raise(&msg)),
    };
    let mut view = Map::new();
    view.insert("key".to_string(), json!(safe));
    view.insert("desc".to_string(), json!(read_desc(&body)));
    view.insert("tokens".to_string(), json!(est_tokens(&body)));
    view.insert("body".to_string(), json!(body));
    let sub = dir.join(&safe);
    if sub.is_dir() {
        let children = readme_index(&sub, &format!("{safe}/"))?;
        if !children.is_empty() {
            view.insert("children".to_string(), Value::Array(children));
        }
    }
    Ok(Value::Object(view))
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
    obj.insert("display_name".to_string(), display_name_or_id(target));
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
/// GATED PER NODE. A caller allowed to reflect ONE agent must not read every
/// descendant's `sentence`/`description` through the tree. A denied node keeps
/// its `id` and `children` (the shape is public through `tree=ids`) and is not
/// dispatched at all. An operator sees everything.
///
/// `Err` carries a node's RAISED reflect: python's exception propagates out of
/// `_roster` and fails the whole reflect.
fn roster(
    kernel: Arc<Kernel>,
    a: Arc<Agent>,
) -> Pin<Box<dyn Future<Output = Result<Value, Value>> + Send>> {
    Box::pin(async move {
        let may_reflect = match crate::send::current_sender() {
            None => true,
            Some(sender) => kernel
                .gate(&sender, "reflect", &a, &Value::Object(Map::new()))
                .is_none(),
        };
        let raw = if a.handler_module.is_some() && may_reflect {
            let (reply, status) = kernel
                .internal_dispatch(&a, &json!({"type": "reflect"}))
                .await;
            if status == Status::Failed {
                return Err(reply);
            }
            reply
        } else if may_reflect {
            reflect_identity(&a)
        } else {
            Value::Object(Map::new())
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
        if may_reflect {
            if let Some(d) = a.description() {
                node.insert("description".to_string(), json!(d));
            }
        }
        for k in PURPOSE_KEYS {
            if let Some(v) = raw.get(k).filter(|v| !v.is_null()) {
                node.insert(k.to_string(), v.clone());
            }
        }
        let mut children = Vec::new();
        for c in a.children_snapshot() {
            children.push(roster(Arc::clone(&kernel), c).await?);
        }
        node.insert("children".to_string(), Value::Array(children));
        Ok(Value::Object(node))
    })
}

/// Append the composable reflect flags to any reflect reply — applied
/// uniformly to bare-agent and bundle reflects (python
/// `_apply_reflect_flags`). Non-object replies and refusals pass through.
///
/// Key order is python's: identity, `description`, gate fields,
/// `kernel_verbs`, the root frame, `tree`, `next`, `more`, `bundles`,
/// `bundles_next`, `readme`.
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
    // A REFUSAL IS NOT A REFLECT. Decorating one with kernel_verbs/tree/more
    // produced a reply that reads like a healthy reflect with a stray error
    // field, and a caller could not tell the two apart (2026-09-02).
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
    // The KERNEL's own verbs — answered by the ROOT alone (they act on the TREE
    // and the kernel-wide task table), so they appear in no bundle's `verbs`
    // map. Advertised on EVERY agent, not just the root: root-only was tried
    // and reverted — a model that never reflects the root never learns they
    // exist, guesses, and gives up.
    obj.insert(
        "kernel_verbs".to_string(),
        crate::send::kernel_verbs_view(full),
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
        obj.insert("runtime".to_string(), json!("rust"));
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
        // runtime simply contributes nothing.
        if let Some(extra) = target
            .handler_module
            .as_deref()
            .and_then(|hm| kernel.bundles.get(hm))
            .and_then(|b| b.reflect_root_extra(target, full, kernel))
        {
            for (k, v) in extra {
                obj.insert(k, v);
            }
        }
    }
    // DECIDED BY KEY PRESENCE (python `payload.get("tree", "all")`): absent is
    // `all`; a present `all`/`ids` acts; any other present value — null, a bool,
    // a number, `none` — omits the tree.
    match payload.get("tree") {
        None => insert_tree(kernel, target, full, obj).await?,
        Some(Value::String(t)) if t == "all" => insert_tree(kernel, target, full, obj).await?,
        Some(Value::String(t)) if t == "ids" => {
            // The FLAT id index is the one tree shape that is a plain list, so it
            // is the one that pages. The nested tree is never sliced — a page of
            // it would be a forest with no root.
            let (ids, next) = crate::paging::page(descendant_ids(target), payload, Clone::clone);
            obj.insert("tree".to_string(), json!(ids));
            if let Some(next) = next {
                obj.insert("next".to_string(), json!(next));
            }
        }
        _ => {}
    }
    if !full {
        obj.insert("more".to_string(), json!(MORE_HINT));
    }
    // The catalog pages with the SAME `limit`/`after` as `tree=ids`, and names
    // its cursor `bundles_next` only when a further page exists.
    match payload.get("bundles") {
        Some(Value::String(b)) if b == "all" => {
            let (cat, next) = crate::paging::page(available_bundles(kernel), payload, |b| {
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
            let names: Vec<String> = available_bundles(kernel)
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
    // body + sub-docs' index. Read from the seeded on-disk tree, one agent at a time.
    match payload.get("readme") {
        Some(Value::Bool(true)) => {
            // python: `if p.exists(): try: return p.read_text(...) except
            // OSError: return None`; a `UnicodeDecodeError` is not an
            // `OSError` and propagates (C60).
            let path = target.readme_file();
            let readme = if path.exists() {
                match read_doc_text(&path) {
                    DocRead::Ok(s) => Some(s),
                    DocRead::Io => None,
                    DocRead::Decode(msg) => return Err(decode_raise(&msg)),
                }
            } else {
                None
            };
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
            obj.insert("readme".to_string(), readme_view(&docs_dir, sel)?);
        }
        _ => {}
    }
    Ok(())
}

/// `tree=all`: the wiring tree under `detail:full`, the gated roster otherwise.
async fn insert_tree(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    full: bool,
    obj: &mut Map<String, Value>,
) -> Result<(), Value> {
    let tree = if full {
        tree_node(target)
    } else {
        roster(Arc::clone(kernel), Arc::clone(target)).await?
    };
    obj.insert("tree".to_string(), tree);
    Ok(())
}

/// Walk an agent and its descendants, producing a nested
/// `{id, parent_id, handler_module, display_name, description?, ...posture,
/// children:[...]}` dict — the `detail:full` wiring tree (python `_tree`),
/// children in creation order.
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
    obj.insert("display_name".to_string(), display_name_or_id(target));
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
            // what a file_bridge exposes under its root (audit d7)
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
    // CREATION ORDER, not sorted: python walks `_children`, a plain dict.
    let children: Vec<Value> = target
        .children_snapshot()
        .iter()
        .map(|c| tree_node(c))
        .collect();
    obj.insert("children".to_string(), Value::Array(children));
    Value::Object(obj)
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
fn available_bundles(kernel: &Arc<Kernel>) -> Vec<Value> {
    let mut out: Vec<Value> = kernel
        .bundles
        .catalog()
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
