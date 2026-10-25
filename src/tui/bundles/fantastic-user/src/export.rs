//! EXPORT A SUBTREE AS A BLUEPRINT (#268): an agent and every agent under it,
//! written in the shape of `src/tui/blueprint.yaml` — nesting under `agents:` is
//! the parent (no `parent_id`), grants are written by id on both ends, `@root`
//! stands for the root.
//!
//! Two layers:
//! - [`build`] is PURE: records in, blueprint text, owned files and ports out.
//!   It takes records only, so row text (conversation history) is never read.
//! - [`export_subtree`] is the verb on `user`: it collects the kernel's records,
//!   writes the folder (temp dir, then rename) and scans it for secrets.
//!
//! What it writes:
//! - every record minus runtime state ([`RUNTIME_FIELDS`] and the `_error`,
//!   `_at`, `_ts` suffixes and the `last_` prefix) and minus any key value
//!   ([`SECRET_KEYS`], at any depth); `credentials.ref` stays;
//! - a reference that points OUTSIDE the subtree becomes a PORT: `@memory`,
//!   `@credentials`, `@exec`, `@files`, `@model` or `@connector:<id>`, numbered
//!   `_2`, `_3`… when two outside agents share a kind (sorted by outside id); an
//!   outside absolute path becomes `@path:<field>`; the root is `@root`;
//! - an absolute path under the subtree's own folder becomes relative to it;
//! - an agent's `system_prompt_file` (relative, readable) goes beside the
//!   blueprint as `<agent id>/directive.md`, and the record points there.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use fantastic_kernel::{AgentId, AgentRecord, Kernel};
use serde_json::{json, Map, Value};

use crate::refuse;

/// The blueprint's file name in the export folder.
pub const BLUEPRINT_FILE: &str = "blueprint.yaml";

/// How the root is named in a blueprint (as `seed.rs` reads it).
pub const ROOT_PORT: &str = "@root";

/// Runtime state a record carries that a blueprint never does.
pub const RUNTIME_FIELDS: [&str; 13] = [
    "parent_id",
    "guid",
    "pid",
    "version",
    "rows",
    "history",
    "cache",
    "usage",
    "created_at",
    "updated_at",
    "ts",
    "last_error",
    "error",
];

/// Keys whose value is a secret: dropped wherever they appear.
pub const SECRET_KEYS: [&str; 4] = ["api_key", "token", "secret", "password"];

/// Record fields that hold one agent id.
const REF_FIELDS: [&str; 3] = ["memory_store_id", "file_bridge_id", "backend_id"];

/// Record fields whose KEYS are agent ids.
const GRANT_FIELDS: [&str; 2] = ["can_call", "callable_by"];

/// The owned directive file.
const DIRECTIVE_FIELD: &str = "system_prompt_file";

/// One port: an outside thing the subtree reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    /// `@memory`, `@connector:<id>`, `@path:<field>`…
    pub name: String,
    /// `memory`, `credentials`, `exec`, `files`, `model`, `connector`, `path`.
    pub kind: String,
    /// What it stood for at export (an outside agent id; empty for a path —
    /// an absolute path is never kept).
    pub target: String,
    /// Why it is needed, one line per use.
    pub why: Vec<String>,
    /// The subtree agents that reach it.
    pub used_by: Vec<String>,
}

/// What [`build`] makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// The blueprint text.
    pub yaml: String,
    /// The owned files: `(path relative to the folder, text)`.
    pub files: Vec<(String, String)>,
    /// The ports, sorted by name.
    pub ports: Vec<Port>,
    /// What could not be exported as asked (also in the blueprint's header).
    pub notes: Vec<String>,
    /// How many agents were written.
    pub agents: usize,
}

/// Whether a key is runtime state.
#[must_use]
pub fn is_runtime(key: &str) -> bool {
    RUNTIME_FIELDS.contains(&key)
        || key == "agents"
        || key.starts_with("last_")
        || key.ends_with("_error")
        || key.ends_with("_at")
        || key.ends_with("_ts")
}

fn drop_secrets(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| !SECRET_KEYS.contains(&k.as_str()));
            m.values_mut().for_each(drop_secrets);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_secrets),
        _ => {}
    }
}

/// A record's meta minus runtime state and secret values.
#[must_use]
pub fn strip_runtime(meta: &Map<String, Value>) -> Map<String, Value> {
    let mut out: Map<String, Value> = meta
        .iter()
        .filter(|(k, _)| !is_runtime(k) && !SECRET_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.values_mut().for_each(drop_secrets);
    out
}

/// Whether a string is an absolute path (`/…` or `C:\…`).
#[must_use]
pub fn is_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
}

/// `s` relative to `folder` when it lies under it.
fn relative_to(s: &str, folder: &str) -> Option<String> {
    let f = folder.trim_end_matches(['/', '\\']);
    if f.is_empty() {
        return None;
    }
    if s == f {
        return Some(".".into());
    }
    let rest = s.strip_prefix(f)?;
    let rest = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\'))?;
    Some(if rest.is_empty() {
        ".".into()
    } else {
        rest.into()
    })
}

/// The port kind for an outside agent, from its handler and the field that
/// reaches it.
#[must_use]
pub fn port_kind(handler_module: Option<&str>, field: &str) -> &'static str {
    let hm = handler_module.unwrap_or("");
    if hm == "credential_store.tools" || field == "credentials.store" {
        "credentials"
    } else if field == "memory_store_id" || hm.ends_with("_store.tools") {
        "memory"
    } else if hm == "exec_bridge.tools" {
        "exec"
    } else if hm == "file_bridge.tools" {
        "files"
    } else if hm.ends_with("_backend.tools") || field == "backend_id" {
        "model"
    } else {
        "connector"
    }
}

/// One outside reach, as the walk sees it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Reach {
    /// an agent outside the subtree
    Agent { target: String, field: String },
    /// an absolute path outside the subtree's folder
    Path { field: String, value: String },
}

struct Ctx<'a> {
    inside: BTreeSet<&'a str>,
    root_id: &'a str,
    folder: Option<&'a str>,
}

impl Ctx<'_> {
    /// An id as the blueprint writes it; `None` = an outside agent (a port).
    fn id(&self, id: &str) -> Option<String> {
        if self.inside.contains(id) {
            Some(id.to_string())
        } else if id == self.root_id {
            Some(ROOT_PORT.to_string())
        } else if id == "*" || id.starts_with('$') || id.starts_with('@') || id.contains('|') {
            // the baseline grants (`$self`…), wildcards, ports and page grants stay
            Some(id.to_string())
        } else {
            None
        }
    }
}

/// Rewrite every absolute path string under `v`; `field` is the record field.
fn paths(v: &mut Value, field: &str, ctx: &Ctx, port: &mut dyn FnMut(Reach) -> String) {
    match v {
        Value::String(s) if is_absolute(s) => {
            *s = match ctx.folder.and_then(|f| relative_to(s, f)) {
                Some(rel) => rel,
                None => port(Reach::Path {
                    field: field.to_string(),
                    value: s.clone(),
                }),
            };
        }
        Value::Array(a) => a.iter_mut().for_each(|x| paths(x, field, ctx, port)),
        Value::Object(m) => m.values_mut().for_each(|x| paths(x, field, ctx, port)),
        _ => {}
    }
}

/// One record as the blueprint writes it (meta only; id, handler and children
/// are placed by the caller). `port` names an outside reach; `owned` holds the
/// directive's export path when its text was read.
fn rewrite(
    rec: &AgentRecord,
    ctx: &Ctx,
    owned: Option<&str>,
    port: &mut dyn FnMut(&str, Reach) -> String,
) -> Map<String, Value> {
    let me = rec.id.as_str();
    let mut m = strip_runtime(&rec.meta);
    let mut name = |field: &str, id: &str| -> String {
        ctx.id(id).unwrap_or_else(|| {
            port(
                me,
                Reach::Agent {
                    target: id.to_string(),
                    field: field.to_string(),
                },
            )
        })
    };
    for g in GRANT_FIELDS {
        if let Some(Value::Object(o)) = m.get(g) {
            let out: Map<String, Value> = o.iter().map(|(k, v)| (name(g, k), v.clone())).collect();
            m.insert(g.to_string(), Value::Object(out));
        }
    }
    for f in REF_FIELDS {
        if let Some(Value::String(t)) = m.get(f) {
            let n = name(f, &t.clone());
            m.insert(f.to_string(), Value::String(n));
        }
    }
    if let Some(Value::String(t)) = m.get("credentials").and_then(|c| c.get("store")).cloned() {
        let n = name("credentials.store", &t);
        m["credentials"]["store"] = Value::String(n);
    }
    if let Some(rel) = owned {
        m.insert(DIRECTIVE_FIELD.to_string(), Value::String(rel.to_string()));
    }
    let mut pport = |r: Reach| port(me, r);
    for (k, v) in &mut m {
        if !GRANT_FIELDS.contains(&k.as_str()) {
            paths(v, k, ctx, &mut pport);
        }
    }
    m
}

/// JSON → YAML without going through serde (a kernel number may be an
/// arbitrary-precision one, which serde would write as a map).
fn to_yaml(v: &Value) -> serde_yaml::Value {
    match v {
        Value::Null => serde_yaml::Value::Null,
        Value::Bool(b) => serde_yaml::Value::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_yaml::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_yaml::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_yaml::Value::Number(f.into())
            } else {
                serde_yaml::Value::String(n.to_string())
            }
        }
        Value::String(s) => serde_yaml::Value::String(s.clone()),
        Value::Array(a) => serde_yaml::Value::Sequence(a.iter().map(to_yaml).collect()),
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            serde_yaml::Value::Mapping(
                keys.into_iter()
                    .map(|k| (serde_yaml::Value::String(k.clone()), to_yaml(&m[k])))
                    .collect(),
            )
        }
    }
}

/// The subtree's ids under `top`, depth first, children in record order.
fn subtree<'a>(records: &'a [AgentRecord], top: &str) -> Vec<&'a AgentRecord> {
    let mut out = Vec::new();
    let mut stack = vec![top.to_string()];
    while let Some(id) = stack.pop() {
        if out.iter().any(|r: &&AgentRecord| r.id == id) {
            continue;
        }
        if let Some(r) = records.iter().find(|r| r.id == id) {
            out.push(r);
        }
        let kids: Vec<String> = records
            .iter()
            .filter(|r| r.parent_id.as_deref() == Some(id.as_str()))
            .map(|r| r.id.clone())
            .collect();
        stack.extend(kids.into_iter().rev());
    }
    out
}

/// BUILD the export of `top` and everything under it. `folder` is the
/// subtree's own folder (absolute paths under it become relative);
/// `read_file(agent id, path)` reads an owned file, `None` when it cannot.
///
/// # Errors
/// `top` is the root, unknown, or sits in no tree (no parent).
pub fn build(
    records: &[AgentRecord],
    top: &str,
    root_id: &str,
    folder: Option<&str>,
    read_file: &dyn Fn(&str, &str) -> Option<String>,
) -> Result<Exported, String> {
    if top == root_id {
        return Err("the root is the kernel's own: export an agent under it".into());
    }
    let Some(t) = records.iter().find(|r| r.id == top) else {
        return Err(format!("no agent {top:?}"));
    };
    if t.parent_id.is_none() {
        return Err(format!("{top:?} sits in no tree (no parent)"));
    }
    let sub = subtree(records, top);
    let ctx = Ctx {
        inside: sub.iter().map(|r| r.id.as_str()).collect(),
        root_id,
        folder,
    };
    let hm_of = |id: &str| {
        records
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.handler_module.as_deref())
    };

    // OWNED FILES: a relative, readable directive goes beside the blueprint
    let mut files = Vec::new();
    let mut notes = Vec::new();
    let mut owned: BTreeMap<&str, String> = BTreeMap::new();
    for r in &sub {
        if let Some(p) = r.meta.get(DIRECTIVE_FIELD).and_then(Value::as_str) {
            match (!is_absolute(p)).then(|| read_file(&r.id, p)).flatten() {
                Some(text) => {
                    let rel = format!("{}/directive.md", r.id);
                    files.push((rel.clone(), text));
                    owned.insert(r.id.as_str(), rel);
                }
                None => notes.push(format!(
                    "agent {}: {DIRECTIVE_FIELD} could not be read; the field is kept without its text",
                    r.id
                )),
            }
        }
    }

    // PASS 1: every outside reach, so ports are numbered by outside id
    let mut uses: Vec<(String, Reach)> = Vec::new();
    for r in &sub {
        let mut collect = |me: &str, x: Reach| {
            uses.push((me.to_string(), x));
            String::new()
        };
        rewrite(
            r,
            &ctx,
            owned.get(r.id.as_str()).map(String::as_str),
            &mut collect,
        );
    }
    // the kind of each outside target: a memory_store_id use decides it
    let mut kinds: BTreeMap<Reach, (&'static str, String)> = BTreeMap::new();
    for (_, x) in &uses {
        let key = canon(x);
        let (kind, label) = match x {
            Reach::Agent { target, field } => {
                let decided = uses.iter().any(|(_, y)| {
                    matches!(y, Reach::Agent { target: t, field: f } if t == target && f == "memory_store_id")
                });
                let f = if decided {
                    "memory_store_id"
                } else {
                    field.as_str()
                };
                (port_kind(hm_of(target), f), target.clone())
            }
            Reach::Path { field, .. } => ("path", field.clone()),
        };
        kinds.entry(key).or_insert((kind, label));
    }
    // names: per kind (per field for paths), in outside-id order
    let mut names: BTreeMap<Reach, String> = BTreeMap::new();
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for (key, (kind, label)) in &kinds {
        let base = match *kind {
            "connector" => format!("@connector:{label}"),
            "path" => format!("@path:{label}"),
            k => format!("@{k}"),
        };
        let n = count.entry(base.clone()).or_insert(0);
        *n += 1;
        names.insert(
            key.clone(),
            if *n == 1 { base } else { format!("{base}_{n}") },
        );
    }

    // PASS 2: the records, ports named
    let mut ports: BTreeMap<String, Port> = BTreeMap::new();
    let mut metas: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    for r in &sub {
        let mut name = |me: &str, x: Reach| {
            let key = canon(&x);
            let n = names[&key].clone();
            let (kind, _) = kinds[&key];
            let (target, why) = match &x {
                Reach::Agent { target, field } => (
                    target.clone(),
                    format!("agent {me} field {field} reaches {target}"),
                ),
                Reach::Path { field, .. } => (
                    String::new(),
                    format!("agent {me} field {field} names a path outside the subtree"),
                ),
            };
            let p = ports.entry(n.clone()).or_insert_with(|| Port {
                name: n.clone(),
                kind: kind.to_string(),
                target,
                why: Vec::new(),
                used_by: Vec::new(),
            });
            if !p.why.contains(&why) {
                p.why.push(why);
            }
            if !p.used_by.iter().any(|u| u == me) {
                p.used_by.push(me.to_string());
            }
            n
        };
        let m = rewrite(
            r,
            &ctx,
            owned.get(r.id.as_str()).map(String::as_str),
            &mut name,
        );
        metas.insert(r.id.as_str(), m);
    }

    // THE TEXT: `agents:` nested (no parent_id), then `ports:`
    let entry = |id: &str| -> serde_yaml::Mapping {
        let r = sub.iter().find(|r| r.id == id).expect("in the subtree");
        let mut e = serde_yaml::Mapping::new();
        e.insert("id".into(), id.into());
        if let Some(hm) = &r.handler_module {
            e.insert("handler_module".into(), hm.as_str().into());
        }
        if let serde_yaml::Value::Mapping(m) = to_yaml(&Value::Object(metas[id].clone())) {
            e.extend(m);
        }
        e
    };
    fn nest(
        id: &str,
        sub: &[&AgentRecord],
        entry: &dyn Fn(&str) -> serde_yaml::Mapping,
    ) -> serde_yaml::Value {
        let mut e = entry(id);
        let kids: Vec<serde_yaml::Value> = sub
            .iter()
            .filter(|r| r.parent_id.as_deref() == Some(id))
            .map(|r| nest(&r.id, sub, entry))
            .collect();
        if !kids.is_empty() {
            e.insert("agents".into(), serde_yaml::Value::Sequence(kids));
        }
        serde_yaml::Value::Mapping(e)
    }
    let mut doc = serde_yaml::Mapping::new();
    doc.insert(
        "agents".into(),
        serde_yaml::Value::Sequence(vec![nest(top, &sub, &entry)]),
    );
    let mut pm = serde_yaml::Mapping::new();
    for p in ports.values() {
        let mut one = serde_yaml::Mapping::new();
        one.insert("kind".into(), p.kind.as_str().into());
        one.insert("why".into(), p.why.join("; ").into());
        one.insert(
            "used_by".into(),
            serde_yaml::Value::Sequence(p.used_by.iter().map(|u| u.as_str().into()).collect()),
        );
        pm.insert(p.name.as_str().into(), serde_yaml::Value::Mapping(one));
    }
    doc.insert("ports".into(), serde_yaml::Value::Mapping(pm));
    let body = serde_yaml::to_string(&doc).map_err(|e| format!("blueprint: {e}"))?;
    let mut yaml = format!(
        "# EXPORTED SUBTREE: agent {top} and every agent under it ({n}), as a\n\
         # blueprint (the shape of src/tui/blueprint.yaml). Nesting under `agents:`\n\
         # is the parent; `@root` is the root; every other `@…` name is a PORT\n\
         # (`ports:` below) for something outside the subtree. Ids are the names;\n\
         # the importer makes fresh ones. No runtime state, key values or rows.\n",
        n = sub.len()
    );
    for n in &notes {
        yaml.push_str(&format!("# NOTE: {n}\n"));
    }
    yaml.push_str(&body);
    Ok(Exported {
        yaml,
        files,
        ports: ports.into_values().collect(),
        notes,
        agents: sub.len(),
    })
}

/// A reach as a port key: an agent by target, a path by (field, value).
fn canon(x: &Reach) -> Reach {
    match x {
        Reach::Agent { target, .. } => Reach::Agent {
            target: target.clone(),
            field: String::new(),
        },
        p @ Reach::Path { .. } => p.clone(),
    }
}

/// THE SECRETS SCAN over an export folder: every hit names the file and what
/// it found (never the value). The blueprint is checked by structure (secret
/// keys, row keys, absolute paths); every file for `sk-…` tokens.
#[must_use]
pub fn scan_secrets(dir: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let name = p.strip_prefix(dir).unwrap_or(&p).display().to_string();
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            if has_key_token(&text) {
                hits.push(format!("{name}: an sk- key token"));
            }
            if name == BLUEPRINT_FILE {
                match serde_yaml::from_str::<serde_yaml::Value>(&text) {
                    Ok(v) => scan_yaml(&v, &name, &mut hits),
                    Err(e) => hits.push(format!("{name}: not yaml ({e})")),
                }
            }
        }
    }
    hits.sort();
    hits
}

fn has_key_token(text: &str) -> bool {
    let b = text.as_bytes();
    text.match_indices("sk-").any(|(i, _)| {
        let before = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let run = b[i + 3..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_' || **c == b'-')
            .count();
        before && run >= 4
    })
}

fn scan_yaml(v: &serde_yaml::Value, file: &str, hits: &mut Vec<String>) {
    match v {
        serde_yaml::Value::String(s) if is_absolute(s) => {
            hits.push(format!("{file}: an absolute path"));
        }
        serde_yaml::Value::Sequence(a) => a.iter().for_each(|x| scan_yaml(x, file, hits)),
        serde_yaml::Value::Mapping(m) => {
            for (k, x) in m {
                if let Some(k) = k.as_str() {
                    if SECRET_KEYS.contains(&k) {
                        hits.push(format!("{file}: a {k} key"));
                    }
                    if k == "rows" || k == "history" {
                        hits.push(format!("{file}: row text ({k})"));
                    }
                }
                scan_yaml(x, file, hits);
            }
        }
        _ => {}
    }
}

/// Every record of the tree, depth first from the root, children in creation
/// order (the order `seed.rs` writes them back in).
fn records(kernel: &Kernel) -> (Vec<AgentRecord>, String) {
    let Some(root) = kernel.root() else {
        return (Vec::new(), String::new());
    };
    let mut out = Vec::new();
    let mut stack = vec![root.id.clone()];
    while let Some(id) = stack.pop() {
        let Some(a) = kernel.agents.get(&id).map(|a| Arc::clone(a.value())) else {
            continue;
        };
        if out.iter().any(|r: &AgentRecord| r.id == id.0) {
            continue;
        }
        out.push(a.record());
        stack.extend(a.child_ids().into_iter().rev());
    }
    (out, root.id.0.clone())
}

fn absolute(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

#[cfg(unix)]
fn mkdir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(p)
}

#[cfg(not(unix))]
fn mkdir(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir(p)
}

fn write_file(p: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)?.write_all(text.as_bytes())
}

/// Write the export into `tmp` (fresh), then scan it.
fn write_folder(tmp: &Path, ex: &Exported) -> Result<(), String> {
    mkdir(tmp).map_err(|e| format!("could not create the export folder: {e}"))?;
    write_file(&tmp.join(BLUEPRINT_FILE), &ex.yaml).map_err(|e| e.to_string())?;
    for (rel, text) in &ex.files {
        let p = tmp.join(rel);
        if let Some(parent) = p.parent() {
            if !parent.exists() {
                mkdir(parent).map_err(|e| e.to_string())?;
            }
        }
        write_file(&p, text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `export_subtree {agent_id, path}` — write the agent and everything under it
/// as a blueprint folder at `path` (relative to the running dir), which must be
/// new or empty.
pub async fn export_subtree(kernel: &Arc<Kernel>, p: &Value) -> Value {
    let (Some(agent_id), Some(path)) = (
        p.get("agent_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
        p.get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
    ) else {
        return refuse(
            "bad_arg",
            "user.export_subtree: agent_id (str) + path (str) required",
        );
    };
    let rel = Path::new(path);
    if rel.components().any(|c| c == Component::ParentDir) {
        return refuse("bad_arg", "user.export_subtree: path may not hold `..`");
    }
    let base = kernel
        .workdir()
        .or_else(|_| std::env::current_dir().map_err(|e| e.to_string()))
        .unwrap_or_default();
    let out = absolute(&base, rel);
    if out.exists() {
        let empty = out.is_dir()
            && std::fs::read_dir(&out)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
        if !empty {
            return refuse(
                "exists",
                format!("user.export_subtree: {path} exists and is not an empty folder"),
            );
        }
    }
    let Some(parent) = out.parent().filter(|p| p.is_dir()) else {
        return refuse(
            "not_found",
            format!("user.export_subtree: the folder that holds {path} does not exist"),
        );
    };

    let (recs, root_id) = records(kernel);
    let folder_of = |id: &str| {
        kernel
            .agents
            .get(&AgentId::from(id))
            .map(|a| a.value().root_path.clone())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| absolute(&base, &p))
    };
    let folder = folder_of(agent_id).map(|p| p.display().to_string());
    // an owned file: relative, no `..`, under the agent's folder or the running dir
    let read = |id: &str, f: &str| -> Option<String> {
        let fp = Path::new(f);
        if fp.components().any(|c| c == Component::ParentDir) {
            return None;
        }
        folder_of(id)
            .map(|d| d.join(fp))
            .filter(|p| p.is_file())
            .or_else(|| Some(base.join(fp)).filter(|p| p.is_file()))
            .and_then(|p| std::fs::read_to_string(p).ok())
    };
    let ex = match build(&recs, agent_id, &root_id, folder.as_deref(), &read) {
        Ok(x) => x,
        Err(e) => return refuse("bad_arg", format!("user.export_subtree: {e}")),
    };

    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.export-{}-{nanos}", std::process::id()));
    if let Err(e) = write_folder(&tmp, &ex) {
        let _ = std::fs::remove_dir_all(&tmp);
        return refuse("failed", format!("user.export_subtree: {e}"));
    }
    let hits = scan_secrets(&tmp);
    if !hits.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        return refuse(
            "secret_found",
            format!(
                "user.export_subtree: the output was not written — {}",
                hits.join("; ")
            ),
        );
    }
    if out.exists() {
        let _ = std::fs::remove_dir(&out);
    }
    if let Err(e) = std::fs::rename(&tmp, &out) {
        let _ = std::fs::remove_dir_all(&tmp);
        return refuse("failed", format!("user.export_subtree: {e}"));
    }
    let mut files = vec![BLUEPRINT_FILE.to_string()];
    files.extend(ex.files.iter().map(|(f, _)| f.clone()));
    json!({
        "exported": ex.agents,
        "path": path,
        "ports": ex.ports.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
        "files": files,
    })
}
