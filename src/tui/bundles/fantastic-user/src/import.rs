//! IMPORT A BLUEPRINT SUBTREE (#269): the other half of [`crate::export`] — a
//! blueprint folder (`blueprint.yaml` + owned files) becomes a live team under
//! any parent.
//!
//! Two layers:
//! - [`plan`] is PURE: the blueprint text, the folder's files and what the
//!   kernel holds in; the agents to create (fresh ids, grants re-wired, ports
//!   bound), the grant edges to add on the bound agents, and what the team will
//!   reach out; or the refusal. Nothing is created before it is whole.
//! - [`import_subtree`] is the verb on `user`: it reads the folder, plans, and
//!   — only with `approve: true` — creates the agents depth first and writes the
//!   other end of every bound edge; a failure deletes what it made.
//!
//! The rules, in the order they are checked:
//! - one top entry under `agents:`; nesting is the parent (`parent_id` is
//!   refused, as `seed.rs` refuses it);
//! - NO SECRETS: a key value ([`SECRET_KEYS`], at any depth), row text
//!   (`rows`, `history`), an `sk-` token in any file, or an absolute path;
//! - PORTS: every `@…` name used must be declared under `ports:`; every
//!   declared port must be bound (`bind: {port: agent id}`) to an existing agent
//!   of the port's kind; `@path:*` ports are not bindable yet;
//! - FRESH IDS: every id gets the same suffix `_<k>`, the smallest `k` for
//!   which no new id is taken (a second import under the same parent gets
//!   `_2`); `@root` becomes the root's id; `$self`, `$parent`, `$subtree`, `*`
//!   and page grants stay;
//! - CODE: a handler that is not a registered bundle, or a file other than
//!   `blueprint.yaml` and `<agent id>/directive.md`, is code — refused unless
//!   `allow_code: true`.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::export::{
    has_key_token, is_absolute, port_kind, BLUEPRINT_FILE, ROOT_PORT, SECRET_KEYS,
};
use crate::refuse;

/// Record fields that hold one agent id.
const REF_FIELDS: [&str; 3] = ["memory_store_id", "file_bridge_id", "backend_id"];

/// Record fields whose KEYS are agent ids.
const GRANT_FIELDS: [&str; 2] = ["can_call", "callable_by"];

/// The largest file the import reads from a blueprint folder.
pub const MAX_FILE: u64 = 1 << 20;

/// The most files a blueprint folder may hold.
pub const MAX_FILES: usize = 512;

/// Why a plan was refused: a `reason` to branch on and a message naming the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportError {
    /// `bad_arg`, `unbound_ports`, `secret_found`, `code_not_allowed`, `bad_binding`.
    pub reason: &'static str,
    /// What was found and how to fix it (never a secret's value).
    pub message: String,
}

fn err(reason: &'static str, message: impl Into<String>) -> ImportError {
    ImportError {
        reason,
        message: format!("user.import_subtree: {}", message.into()),
    }
}

/// One agent to create.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// Its id in the blueprint.
    pub old_id: String,
    /// Its fresh id.
    pub new_id: String,
    /// Its parent: the import's parent for the top, a new id below.
    pub parent: String,
    /// Its handler.
    pub handler_module: String,
    /// Its record fields, ids re-wired.
    pub meta: Map<String, Value>,
}

/// The other end of a bound edge: `agent`'s `field` gains `key` (a new id).
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    /// The existing agent (a bound port, or the root).
    pub agent: String,
    /// `callable_by` (the new agent calls it) or `can_call` (it calls the new agent).
    pub field: &'static str,
    /// The new agent's id.
    pub key: String,
    /// The grant, as the blueprint wrote it.
    pub grant: Value,
}

/// What [`plan`] makes.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The agents, depth first (a parent before its children).
    pub agents: Vec<Planned>,
    /// Blueprint id → fresh id.
    pub ids: BTreeMap<String, String>,
    /// The grants to add on existing agents.
    pub edges: Vec<Edge>,
    /// Per new agent: what it can call, what can call it, its ports.
    pub reaches: Vec<Value>,
    /// The code it carries (handlers and files), one line each.
    pub code: Vec<String>,
    /// Port → the agent it is bound to.
    pub ports: BTreeMap<String, String>,
}

/// What the kernel holds, as [`plan`] asks it.
pub struct World<'a> {
    /// Where the top agent goes.
    pub parent_id: &'a str,
    /// The root's id (what `@root` becomes).
    pub root_id: &'a str,
    /// `{port: agent id}`.
    pub bind: &'a Map<String, Value>,
    /// Whether code may come in.
    pub allow_code: bool,
    /// An existing agent's handler (`Some("")` when it has none); `None` = no such agent.
    pub agent: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a handler is a registered (built-in) bundle.
    pub known_bundle: &'a dyn Fn(&str) -> bool,
}

/// One blueprint entry, flattened.
struct Entry {
    id: String,
    parent: Option<String>,
    hm: String,
    meta: Map<String, Value>,
}

fn walk(v: &Value, parent: Option<&str>, out: &mut Vec<Entry>) -> Result<(), ImportError> {
    let Some(m) = v.as_object() else {
        return Err(err("bad_arg", "every entry under `agents:` must be a map"));
    };
    let Some(id) = m
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err(err("bad_arg", "every entry under `agents:` needs an id"));
    };
    if m.contains_key("parent_id") {
        return Err(err(
            "bad_arg",
            format!("agent {id}: parent_id is not written in a blueprint — nesting under `agents:` is the parent"),
        ));
    }
    let Some(hm) = m
        .get("handler_module")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err(err(
            "bad_arg",
            format!("agent {id}: handler_module is required"),
        ));
    };
    if fantastic_kernel::refusal::guard_agent_id("import_subtree", id).is_some() {
        return Err(err("bad_arg", format!("agent {id:?}: not a safe agent id")));
    }
    if out.iter().any(|e| e.id == id) {
        return Err(err("bad_arg", format!("agent {id} appears twice")));
    }
    let meta: Map<String, Value> = m
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "id" | "handler_module" | "agents"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.push(Entry {
        id: id.to_string(),
        parent: parent.map(str::to_string),
        hm: hm.to_string(),
        meta,
    });
    match m.get("agents") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(kids)) => kids.iter().try_for_each(|k| walk(k, Some(id), out)),
        Some(_) => Err(err(
            "bad_arg",
            format!("agent {id}: `agents:` must be a list"),
        )),
    }
}

/// Every secret under `v`, by key or path (never the value).
fn secrets(v: &Value, id: &str, field: &str, hits: &mut Vec<String>) {
    match v {
        Value::String(s) if is_absolute(s) => {
            hits.push(format!("agent {id} field {field}: an absolute path"));
        }
        Value::Array(a) => a.iter().for_each(|x| secrets(x, id, field, hits)),
        Value::Object(m) => {
            for (k, x) in m {
                if SECRET_KEYS.contains(&k.as_str()) {
                    hits.push(format!("agent {id}: a {k} key"));
                }
                if k == "rows" || k == "history" {
                    hits.push(format!("agent {id}: row text ({k})"));
                }
                secrets(x, id, field, hits);
            }
        }
        _ => {}
    }
}

/// Every `@path:` string under `v`.
fn path_ports(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) if s.starts_with("@path:") => {
            out.insert(s.clone());
        }
        Value::Array(a) => a.iter().for_each(|x| path_ports(x, out)),
        Value::Object(m) => m.values().for_each(|x| path_ports(x, out)),
        _ => {}
    }
}

/// The kind of agent a handler is, in port words.
fn handler_kind(hm: &str) -> &'static str {
    port_kind(Some(hm), "")
}

/// A name that stays as it is: a baseline token, a wildcard, a page grant.
fn is_kept(name: &str) -> bool {
    name == "*" || name.starts_with('$') || name.contains('|')
}

/// Every id-holding place of a record: `(field, the name there)`.
fn names_in(meta: &Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for g in GRANT_FIELDS {
        if let Some(Value::Object(o)) = meta.get(g) {
            out.extend(o.keys().map(|k| (g.to_string(), k.clone())));
        }
    }
    for f in REF_FIELDS {
        if let Some(Value::String(t)) = meta.get(f) {
            out.push((f.to_string(), t.clone()));
        }
    }
    if let Some(Value::String(t)) = meta.get("credentials").and_then(|c| c.get("store")) {
        out.push(("credentials.store".into(), t.clone()));
    }
    out
}

/// PLAN the import of `yaml` (+ the folder's other `files`: path → text) under
/// `w.parent_id`.
///
/// # Errors
/// The first rule the blueprint breaks, in the order of the module doc.
pub fn plan(yaml: &str, files: &BTreeMap<String, String>, w: &World) -> Result<Plan, ImportError> {
    // a. THE SHAPE
    let doc: Value = serde_yaml::from_str(yaml)
        .map_err(|e| err("bad_arg", format!("{BLUEPRINT_FILE} is not yaml ({e})")))?;
    let tops = match doc.get("agents") {
        Some(Value::Array(a)) => a,
        _ => {
            return Err(err(
                "bad_arg",
                format!("{BLUEPRINT_FILE} needs `agents:` with one top entry"),
            ))
        }
    };
    if tops.len() != 1 {
        return Err(err(
            "bad_arg",
            format!(
                "{BLUEPRINT_FILE} holds {} top entries under `agents:`; a subtree has exactly one",
                tops.len()
            ),
        ));
    }
    let mut entries = Vec::new();
    walk(&tops[0], None, &mut entries)?;
    let declared: BTreeMap<String, String> = match doc.get("ports") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(m)) => {
            let mut out = BTreeMap::new();
            for (name, p) in m {
                let Some(kind) = p.get("kind").and_then(Value::as_str) else {
                    return Err(err("bad_arg", format!("port {name}: kind is required")));
                };
                out.insert(name.clone(), kind.to_string());
            }
            out
        }
        Some(_) => return Err(err("bad_arg", "`ports:` must be a map")),
    };

    // b. NO SECRETS
    let mut hits = Vec::new();
    for e in &entries {
        for (k, v) in &e.meta {
            if SECRET_KEYS.contains(&k.as_str()) {
                hits.push(format!("agent {}: a {k} key", e.id));
            }
            if k == "rows" || k == "history" {
                hits.push(format!("agent {}: row text ({k})", e.id));
            }
            secrets(v, &e.id, k, &mut hits);
        }
    }
    if has_key_token(yaml) {
        hits.push(format!("{BLUEPRINT_FILE}: an sk- key token"));
    }
    for (name, text) in files {
        if has_key_token(text) {
            hits.push(format!("{name}: an sk- key token"));
        }
    }
    if !hits.is_empty() {
        hits.sort();
        hits.dedup();
        return Err(err(
            "secret_found",
            format!(
                "the blueprint carries secrets — {}; remove it and export again, or bind a credentials port",
                hits.join("; ")
            ),
        ));
    }

    // c. PORTS
    let inside: BTreeSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let mut paths = BTreeSet::new();
    for e in &entries {
        for v in e.meta.values() {
            path_ports(v, &mut paths);
        }
        for (field, name) in names_in(&e.meta) {
            if name.starts_with('@') && name != ROOT_PORT && !declared.contains_key(&name) {
                return Err(err(
                    "bad_arg",
                    format!(
                        "agent {} field {field} names port {name}, which `ports:` does not declare",
                        e.id
                    ),
                ));
            }
        }
    }
    paths.extend(
        declared
            .iter()
            .filter(|(n, k)| k.as_str() == "path" || n.starts_with("@path:"))
            .map(|(n, _)| n.clone()),
    );
    if !paths.is_empty() {
        return Err(err(
            "unbound_ports",
            format!(
                "path ports are not bindable yet: {} — set the field to a relative path and export again",
                paths.into_iter().collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    let missing: Vec<String> = declared
        .iter()
        .filter(|(n, _)| !w.bind.contains_key(n.as_str()))
        .map(|(n, k)| format!("{n} ({k})"))
        .collect();
    if !missing.is_empty() {
        let example: Vec<String> = declared
            .keys()
            .filter(|n| !w.bind.contains_key(n.as_str()))
            .map(|n| format!("{n:?}: \"<agent id>\""))
            .collect();
        return Err(err(
            "unbound_ports",
            format!(
                "unbound required ports: {}: pass bind {{{}}}",
                missing.join(", "),
                example.join(", ")
            ),
        ));
    }
    let mut ports = BTreeMap::new();
    for (name, v) in w.bind {
        let Some(kind) = declared.get(name) else {
            return Err(err(
                "bad_binding",
                format!(
                    "bind names {name}, which is not a port of this blueprint (ports: {})",
                    declared.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        };
        let Some(target) = v.as_str().filter(|s| !s.is_empty()) else {
            return Err(err(
                "bad_binding",
                format!("bind {name}: an agent id (str)"),
            ));
        };
        let Some(hm) = (w.agent)(target) else {
            return Err(err(
                "bad_binding",
                format!("bind {name}: no agent {target:?}"),
            ));
        };
        let found = handler_kind(&hm);
        if kind != "connector" && found != kind {
            return Err(err(
                "bad_binding",
                format!("bind {name}: the port wants a {kind} agent; {target} is {found} ({hm})"),
            ));
        }
        ports.insert(name.clone(), target.to_string());
    }

    // d. FRESH IDS: one suffix for all
    let mut k = 1usize;
    let ids: BTreeMap<String, String> = loop {
        let ids: BTreeMap<String, String> = entries
            .iter()
            .map(|e| (e.id.clone(), format!("{}_{k}", e.id)))
            .collect();
        if ids.values().all(|n| {
            (w.agent)(n).is_none()
                && n != w.root_id
                && fantastic_kernel::refusal::guard_agent_id("import_subtree", n).is_none()
        }) {
            break ids;
        }
        k += 1;
        if k > 100_000 {
            return Err(err("bad_arg", "no free id suffix"));
        }
    };

    // e. REWRITE: inside id → new id, @root → the root, port → bound agent
    let resolve = |id: &str, field: &str, name: &str| -> Result<String, ImportError> {
        if let Some(n) = ids.get(name) {
            Ok(n.clone())
        } else if name == ROOT_PORT {
            Ok(w.root_id.to_string())
        } else if let Some(b) = ports.get(name) {
            Ok(b.clone())
        } else if is_kept(name) {
            Ok(name.to_string())
        } else {
            Err(err(
                "bad_arg",
                format!("agent {id} field {field} names {name}, which is neither in the blueprint nor a port"),
            ))
        }
    };
    let outside = |name: &str| !inside.contains(name) && !is_kept(name);
    let mut agents = Vec::new();
    let mut edges = Vec::new();
    let mut reaches = Vec::new();
    for e in &entries {
        let new_id = ids[&e.id].clone();
        let mut meta = e.meta.clone();
        let mut reach = Map::new();
        let mut used = BTreeMap::new();
        for g in GRANT_FIELDS {
            if let Some(Value::Object(o)) = meta.get(g).cloned() {
                let mut out = Map::new();
                for (key, grant) in o {
                    let to = resolve(&e.id, g, &key)?;
                    // f. BOTH ENDS: the bound agent (or the root) grants back
                    if outside(&key) {
                        edges.push(Edge {
                            agent: to.clone(),
                            field: if g == "can_call" {
                                "callable_by"
                            } else {
                                "can_call"
                            },
                            key: new_id.clone(),
                            grant: grant.clone(),
                        });
                        used.insert(key.clone(), json!(to));
                    }
                    out.insert(to, grant);
                }
                let mut keys: Vec<&String> = out.keys().collect();
                keys.sort();
                reach.insert(g.to_string(), json!(keys));
                meta.insert(g.to_string(), Value::Object(out));
            }
        }
        for f in REF_FIELDS {
            if let Some(Value::String(t)) = meta.get(f).cloned() {
                let to = resolve(&e.id, f, &t)?;
                if outside(&t) {
                    used.insert(t.clone(), json!(to));
                }
                meta.insert(f.to_string(), Value::String(to));
            }
        }
        if let Some(Value::String(t)) = meta
            .get("credentials")
            .and_then(|c| c.get("store"))
            .cloned()
        {
            let to = resolve(&e.id, "credentials.store", &t)?;
            if outside(&t) {
                used.insert(t.clone(), json!(to));
            }
            meta["credentials"]["store"] = Value::String(to);
        }
        reach.insert("id".into(), json!(new_id));
        reach.insert("from".into(), json!(e.id));
        reach.insert("ports".into(), Value::Object(used.into_iter().collect()));
        reaches.push(Value::Object(reach));
        agents.push(Planned {
            old_id: e.id.clone(),
            new_id,
            parent: e
                .parent
                .as_ref()
                .map_or_else(|| w.parent_id.to_string(), |p| ids[p].clone()),
            handler_module: e.hm.clone(),
            meta,
        });
    }

    // g. CODE
    let mut code: Vec<String> = entries
        .iter()
        .filter(|e| !(w.known_bundle)(&e.hm))
        .map(|e| format!("agent {}: handler {} is not a built-in bundle", e.id, e.hm))
        .collect();
    for name in files.keys() {
        let owned = name
            .strip_suffix("/directive.md")
            .is_some_and(|id| inside.contains(id));
        if name != BLUEPRINT_FILE && !owned {
            code.push(format!("file {name}"));
        }
    }
    if !code.is_empty() && !w.allow_code {
        return Err(err(
            "code_not_allowed",
            format!(
                "the blueprint carries code — {}; pass allow_code: true to import code",
                code.join("; ")
            ),
        ));
    }

    Ok(Plan {
        agents,
        ids,
        edges,
        reaches,
        code,
        ports,
    })
}

/// The folder's files, `/`-separated relative path → text (lossy). No
/// symlinks, at most [`MAX_FILES`] files of [`MAX_FILE`] bytes each.
fn read_folder(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = std::fs::read_dir(&d).map_err(|e| e.to_string())?;
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(dir)
                .unwrap_or(&p)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let md = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
            if md.file_type().is_symlink() {
                return Err(format!(
                    "{rel} is a symlink; a blueprint folder holds plain files"
                ));
            }
            if md.is_dir() {
                stack.push(p);
                continue;
            }
            if md.len() > MAX_FILE {
                return Err(format!("{rel} is larger than {MAX_FILE} bytes"));
            }
            if out.len() >= MAX_FILES {
                return Err(format!("the folder holds more than {MAX_FILES} files"));
            }
            let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
            out.insert(rel, String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    Ok(out)
}

/// Only the literal `true` counts.
fn literal_true(p: &Value, k: &str) -> bool {
    p.get(k) == Some(&Value::Bool(true))
}

/// Delete what an import made (the top agent, cascading) and take back the
/// edges it wrote.
async fn roll_back(kernel: &Arc<Kernel>, top: Option<&str>, edges: &[Edge]) {
    for e in edges {
        let _ = kernel.edit_grants(&AgentId::from(e.agent.as_str()), |cc, cb| {
            let m = if e.field == "can_call" { cc } else { cb };
            let changed = m.remove(&e.key).is_some();
            (
                e.field == "can_call" && changed,
                e.field == "callable_by" && changed,
            )
        });
    }
    if let Some(top) = top {
        if let Some(a) = kernel
            .agents
            .get(&AgentId::from(top))
            .map(|a| Arc::clone(a.value()))
        {
            fantastic_kernel::lifecycle::cascade_delete(kernel, &a).await;
        }
    }
}

/// `import_subtree {path, parent_id, bind?, allow_code?, approve?}` — plan the
/// import of the blueprint folder at `path` (relative to the running dir) under
/// `parent_id`; without `approve: true` the reply is the preview and nothing is
/// created.
pub async fn import_subtree(kernel: &Arc<Kernel>, p: &Value) -> Value {
    let (Some(path), Some(parent_id)) = (
        p.get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
        p.get("parent_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
    ) else {
        return refuse(
            "bad_arg",
            "user.import_subtree: path (str) + parent_id (str) required",
        );
    };
    let empty = Map::new();
    let bind = match p.get("bind") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => {
            return refuse(
                "bad_arg",
                "user.import_subtree: bind is a map {\"@port\": \"<agent id>\"}",
            )
        }
    };
    let rel = Path::new(path);
    if rel.components().any(|c| c == Component::ParentDir) {
        return refuse("bad_arg", "user.import_subtree: path may not hold `..`");
    }
    let Some(root) = kernel.root() else {
        return refuse("failed", "user.import_subtree: the kernel has no root");
    };
    if !kernel.agents.contains_key(&AgentId::from(parent_id)) {
        return refuse(
            "not_found",
            format!("user.import_subtree: no agent {parent_id:?} to import under"),
        );
    }
    let base = kernel
        .workdir()
        .or_else(|_| std::env::current_dir().map_err(|e| e.to_string()))
        .unwrap_or_default();
    let dir = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    };
    if !dir.join(BLUEPRINT_FILE).is_file() {
        return refuse(
            "not_found",
            format!("user.import_subtree: {path} holds no {BLUEPRINT_FILE}"),
        );
    }
    let mut files = match read_folder(&dir) {
        Ok(f) => f,
        Err(e) => return refuse("bad_arg", format!("user.import_subtree: {e}")),
    };
    let yaml = files.remove(BLUEPRINT_FILE).unwrap_or_default();
    // the plan in its own scope: the lookups it takes are not held across an await
    let planned = {
        let agent = |id: &str| {
            kernel
                .agents
                .get(&AgentId::from(id))
                .map(|a| a.value().handler_module.clone().unwrap_or_default())
        };
        let known = |hm: &str| kernel.bundles.get(hm).is_some();
        let world = World {
            parent_id,
            root_id: &root.id.0,
            bind,
            allow_code: literal_true(p, "allow_code"),
            agent: &agent,
            known_bundle: &known,
        };
        plan(&yaml, &files, &world)
    };
    let plan = match planned {
        Ok(x) => x,
        Err(e) => return refuse(e.reason, e.message),
    };

    // THE PREVIEW: what it will reach and what code it carries; nothing made
    if !literal_true(p, "approve") {
        return json!({
            "approval_required": true,
            "agents": plan.agents.len(),
            "new_ids": plan.ids,
            "reaches": plan.reaches,
            "ports": plan.ports,
            "code": plan.code,
            "hint": "nothing was created: call again with approve: true to import",
        });
    }

    // APPROVED: depth first, then the other end of every bound edge
    let top = plan.agents.first().map(|a| a.new_id.clone());
    for a in &plan.agents {
        let mut payload = a.meta.clone();
        payload.insert("type".into(), json!("create_agent"));
        payload.insert("id".into(), json!(a.new_id));
        payload.insert("handler_module".into(), json!(a.handler_module));
        payload.insert("parent_id".into(), json!(a.parent));
        let r = fantastic_kernel::lifecycle::create_from_payload(
            kernel,
            &root,
            &Value::Object(payload),
        )
        .await;
        if let Some(e) = r.get("error") {
            let made = kernel
                .agents
                .contains_key(&AgentId::from(top.as_deref().unwrap_or("")));
            roll_back(kernel, made.then_some(top.as_deref()).flatten(), &[]).await;
            return refuse(
                "failed",
                format!(
                    "user.import_subtree: agent {} ({}) was not created — {}; nothing was kept",
                    a.old_id,
                    a.new_id,
                    e.as_str().map_or_else(|| e.to_string(), str::to_string)
                ),
            );
        }
    }
    let mut done: Vec<Edge> = Vec::new();
    for e in &plan.edges {
        let r = kernel.edit_grants(&AgentId::from(e.agent.as_str()), |cc, cb| {
            let m = if e.field == "can_call" { cc } else { cb };
            m.insert(e.key.clone(), e.grant.clone());
            (e.field == "can_call", e.field == "callable_by")
        });
        match r {
            Ok(_) => done.push(e.clone()),
            Err(why) => {
                roll_back(kernel, top.as_deref(), &done).await;
                return refuse(
                    "failed",
                    format!(
                        "user.import_subtree: the grant {}[{}] on {} was refused — {why}; nothing was kept",
                        e.field, e.key, e.agent
                    ),
                );
            }
        }
    }
    json!({
        "imported": plan.agents.len(),
        "ids": plan.ids,
        "parent_id": parent_id,
        "ports": plan.ports,
    })
}
