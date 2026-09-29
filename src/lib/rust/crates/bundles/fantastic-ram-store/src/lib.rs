//! ram_store — a MEMORY STORE agent held in RAM.
//!
//! A rule-for-rule port of python's `bundled_agents/memory/ram_store/tools.py`,
//! which is canonical.
//!
//! A memory store keeps conversation rows for MANY owners (multi-tenant). Every row
//! is keyed by `(owner, id)`; the owner is never a field a caller writes — it is
//! WHO CALLED: `<kernel guid>|<caller agent id>` for a local caller, the qualified
//! address a bridge leg relays for a caller on another kernel, and `<guid>|kernel`
//! for the operator (`sender=None`). So a caller writes ONLY under its own key, by
//! construction.
//!
//! THE TOPOLOGY IS SQLITE'S. The tables below are the ones a durable
//! `sqlite_store` keeps, with the same keys, the same uniqueness, the same indexes
//! and the same foreign-key cascades; only the query language is missing. A caller
//! cannot tell the two apart, and the durable port is a 1:1 mirror under the same
//! verbs:
//!
//! ```text
//!   rows       PRIMARY KEY (owner, id)            the row dict, minus `refs`
//!              INDEX (owner, id) ordered           paging, `from_last_kind`
//!   fold_refs  (owner, fold_id, member_id)         a fold's members, in order
//!              FK (owner, fold_id)   → rows ON DELETE CASCADE
//!              FK (owner, member_id) → rows ON DELETE CASCADE
//!   grants     PRIMARY KEY gid                     {owner, reader, scope, mode,
//!                                                   anchor, as_of, until, why, …}
//!              FK (owner, anchor) → rows ON DELETE CASCADE
//!              UNIQUE (owner, reader, scope, mode) an identical share is one grant
//!              INDEX owner, INDEX reader
//!   text_idx   (owner, id) → lowercased text       the FTS5 counterpart; derived
//! ```
//!
//! ACCESS, nothing implicit:
//!   - any caller: its OWN key (append / read / search / prune / share / unshare /
//!     grants), plus rows another owner GRANTED it (`read`/`search` with `grant`).
//!   - `operator: true` on the record serves the operator at all (as `kernel`).
//!   - `admins: [ids]` (`"kernel"` = the operator) may name another `owner` and may
//!     `inject` / `dump` the whole store.
//!
//! The kernel's call gate still decides who may reach this agent at all.
//!
//! Ids are the KERNEL's: a row carries the int the kernel minted, or none and this
//! store takes the next one from the kernel's counter — never an id of its own.
//!
//! Everything here dies with the process. That is what `ram` says.

#![deny(missing_docs)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_bundle::{
    py_json_dumps, py_repr, py_repr_value, py_split_whitespace, py_splitlines, py_str, py_strip,
    py_truthy,
};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    missing, refusal, refusal_hint, with, BAD_ARG, EXISTS, NOT_FOUND, UNAUTHORIZED, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

/// The `handler_module` this bundle answers to — python's package path.
pub const HANDLER_MODULE: &str = "ram_store.tools";

/// A reply object from `(key, value)` pairs, the values MOVED in verbatim.
///
/// A stored row is a caller's value, and the kernel keeps one number whose
/// text is not JSON: the `NaN` / `Infinity` token python's `json.dumps` writes
/// (a model's `<limit>inf</limit>`, or a hex id like `3e500123` that `float()`
/// reads as infinity). `json!` turns each operand into a `Value` through
/// `to_value`, which re-parses that text and PANICS — and a panic here fires
/// under the store's lock, so every later call answers "ram_store poisoned"
/// (measured: E30 subai on rust, 2026-09-29). So a reply that carries a row is
/// assembled with this, never with `json!` — the kernel's own
/// `jsonsafe::obj` rule, which a bundle cannot import.
fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut out = Map::with_capacity(N);
    for (k, v) in pairs {
        out.insert(k.to_string(), v);
    }
    Value::Object(out)
}

pub mod test_support;

/// The short orientation readme — python's, verbatim.
pub const README: &str = include_str!("readme.md");

const OPERATOR: &str = "kernel";
const MODES: [&str; 2] = ["frozen", "live"];
const SCOPE_KINDS: [&str; 4] = ["head_until", "range", "fold", "row"];
/// Kinds whose `refs` name members (the fold_refs table): a fold, a clear-fold.
const REF_KINDS: [&str; 2] = ["fold", "clear"];

/// One stored row (without `refs` — those live in `fold_refs`).
type Row = Map<String, Value>;

/// python `_as_id`: a positive int, or the decimal digits a caller wrote.
fn as_id(x: &Value) -> Option<i64> {
    match x {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_i64().filter(|i| *i > 0),
        Value::String(s) => {
            let t = py_strip(s);
            if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
                t.parse::<i64>().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// python `_text_of`: the content as text (a non-string JSON-dumped), a title
/// prefixed on its own line.
fn text_of(row: &Row) -> String {
    let text = match row.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_json_dumps(other, false, None),
        None => "null".to_string(),
    };
    match row.get("title") {
        Some(Value::String(t)) if !t.is_empty() => format!("{t}\n{text}"),
        _ => text,
    }
}

/// A deterministic text form of a scope, for the UNIQUE (owner, reader, scope,
/// mode) key — python `json.dumps(scope, sort_keys=True)`.
fn scope_key(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    py_json_dumps(&sorted(v), true, None)
}

type GrantKey = (String, String, String, String);

/// The store's tables — RAM maps in sqlite's shape (see the module docs).
#[derive(Default)]
struct Tables {
    rows: BTreeMap<String, HashMap<i64, Row>>,
    order: HashMap<String, Vec<i64>>,
    fold_refs: BTreeMap<(String, i64), Vec<i64>>,
    ref_by_member: HashMap<(String, i64), HashSet<i64>>,
    grants: BTreeMap<i64, Map<String, Value>>,
    grant_seq: i64,
    grant_by_anchor: HashMap<(String, i64), HashSet<i64>>,
    grant_key: HashMap<GrantKey, i64>,
    text: HashMap<(String, i64), String>,
}

fn grant_key_of(g: &Map<String, Value>) -> GrantKey {
    (
        g.get("owner").map(py_str).unwrap_or_default(),
        g.get("reader").map(py_str).unwrap_or_default(),
        scope_key(g.get("scope").unwrap_or(&Value::Null)),
        g.get("mode").map(py_str).unwrap_or_default(),
    )
}

impl Tables {
    fn has(&self, owner: &str, rid: i64) -> bool {
        self.rows.get(owner).is_some_and(|t| t.contains_key(&rid))
    }

    fn ids(&self, owner: &str) -> Vec<i64> {
        self.order.get(owner).cloned().unwrap_or_default()
    }

    fn row(&self, owner: &str, rid: i64) -> Option<&Row> {
        self.rows.get(owner).and_then(|t| t.get(&rid))
    }

    fn get(&self, owner: &str, rid: i64) -> Option<Row> {
        let mut out = self.row(owner, rid)?.clone();
        if let Some(refs) = self.fold_refs.get(&(owner.to_string(), rid)) {
            out.insert(
                "refs".to_string(),
                Value::Array(refs.iter().map(|m| Value::from(*m)).collect()),
            );
        }
        Some(out)
    }

    /// All-or-nothing: the caller has validated every row and every ref.
    fn insert(&mut self, owner: &str, rows: Vec<Row>) {
        for row in rows {
            let Some(rid) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let refs = row.get("refs").and_then(Value::as_array).cloned();
            let stored: Row = row
                .iter()
                .filter(|(k, _)| k.as_str() != "refs")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let text = text_of(&row).to_lowercase();
            self.rows
                .entry(owner.to_string())
                .or_default()
                .insert(rid, stored);
            let order = self.order.entry(owner.to_string()).or_default();
            let at = order.partition_point(|x| *x <= rid);
            order.insert(at, rid);
            if let Some(refs) = refs {
                let members: Vec<i64> = refs.iter().filter_map(as_id).collect();
                for m in &members {
                    self.ref_by_member
                        .entry((owner.to_string(), *m))
                        .or_default()
                        .insert(rid);
                }
                self.fold_refs.insert((owner.to_string(), rid), members);
            }
            self.text.insert((owner.to_string(), rid), text);
        }
    }

    /// DELETE FROM rows WHERE owner=? AND id IN doomed — with every cascade.
    /// Returns (rows removed, grants removed).
    fn delete(&mut self, owner: &str, doomed: &HashSet<i64>) -> (usize, usize) {
        let doomed: HashSet<i64> = doomed
            .iter()
            .copied()
            .filter(|r| self.has(owner, *r))
            .collect();
        let mut grants_gone = 0;
        for rid in &doomed {
            if let Some(t) = self.rows.get_mut(owner) {
                t.remove(rid);
            }
            let key = (owner.to_string(), *rid);
            self.text.remove(&key);
            // FK fold_refs.fold_id → rows: the fold's member list goes with it.
            for m in self.fold_refs.remove(&key).unwrap_or_default() {
                let mk = (owner.to_string(), m);
                if let Some(s) = self.ref_by_member.get_mut(&mk) {
                    s.remove(rid);
                    if s.is_empty() {
                        self.ref_by_member.remove(&mk);
                    }
                }
            }
            // FK fold_refs.member_id → rows: the member leaves every fold naming it.
            for f in self.ref_by_member.remove(&key).unwrap_or_default() {
                if let Some(refs) = self.fold_refs.get_mut(&(owner.to_string(), f)) {
                    refs.retain(|m| m != rid);
                }
            }
            // FK grants.anchor → rows: a grant anchored here is gone.
            for gid in self.grant_by_anchor.remove(&key).unwrap_or_default() {
                if self.drop_grant(gid) {
                    grants_gone += 1;
                }
            }
        }
        if !doomed.is_empty() {
            if let Some(order) = self.order.get_mut(owner) {
                order.retain(|i| !doomed.contains(i));
            }
        }
        (doomed.len(), grants_gone)
    }

    fn add_grant(&mut self, g: Map<String, Value>) -> (i64, bool) {
        let key = grant_key_of(&g);
        if let Some(gid) = self.grant_key.get(&key) {
            return (*gid, false);
        }
        self.grant_seq += 1;
        let gid = self.grant_seq;
        let owner = g.get("owner").map(py_str).unwrap_or_default();
        let anchor = g.get("anchor").and_then(Value::as_i64).unwrap_or(0);
        let mut g = g;
        g.insert("grant".to_string(), Value::from(gid));
        self.grants.insert(gid, g);
        self.grant_key.insert(key, gid);
        self.grant_by_anchor
            .entry((owner, anchor))
            .or_default()
            .insert(gid);
        (gid, true)
    }

    fn drop_grant(&mut self, gid: i64) -> bool {
        let Some(g) = self.grants.remove(&gid) else {
            return false;
        };
        self.grant_key.remove(&grant_key_of(&g));
        let owner = g.get("owner").map(py_str).unwrap_or_default();
        let anchor = g.get("anchor").and_then(Value::as_i64).unwrap_or(0);
        let ak = (owner, anchor);
        if let Some(s) = self.grant_by_anchor.get_mut(&ak) {
            s.remove(&gid);
            if s.is_empty() {
                self.grant_by_anchor.remove(&ak);
            }
        }
        true
    }

    fn dump(&self) -> Value {
        let mut rows = Vec::new();
        for (owner, table) in &self.rows {
            for rid in self.order.get(owner).map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(r) = table.get(rid) {
                    let mut item = Map::new();
                    item.insert("owner".to_string(), Value::from(owner.clone()));
                    item.insert("row".to_string(), Value::Object(r.clone()));
                    rows.push(Value::Object(item));
                }
            }
        }
        let fold_refs: Vec<Value> = self
            .fold_refs
            .iter()
            .map(|((owner, fold), members)| {
                json!({"owner": owner, "fold": fold, "members": members})
            })
            .collect();
        let grants: Vec<Value> = self.grants.values().cloned().map(Value::Object).collect();
        let mut out = Map::new();
        out.insert("rows".to_string(), Value::Array(rows));
        out.insert("fold_refs".to_string(), Value::Array(fold_refs));
        out.insert("grants".to_string(), Value::Array(grants));
        Value::Object(out)
    }
}

fn bad_tables(msg: &str) -> Value {
    let mut r = refusal(BAD_ARG, msg);
    with(&mut r, "arg", json!("tables"));
    r
}

/// Build a `Tables` from a `dump` — every key and FK checked; a refusal on the
/// first violation (nothing half-loaded).
fn load_tables(tables: &Value) -> Result<Tables, Value> {
    let Some(tables) = tables.as_object() else {
        return Err(bad_tables("inject: `tables` must be a dump dict"));
    };
    let mut t = Tables::default();
    let rows = match tables.get("rows") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(v) if !py_truthy(v) => Vec::new(),
        Some(_) => return Err(bad_tables("inject: tables.rows must be a list")),
    };
    let mut by_owner: Vec<(String, Vec<Row>)> = Vec::new();
    for item in &rows {
        let owner = item.get("owner").and_then(Value::as_str);
        let (Some(_), Some(owner)) = (item.as_object(), owner) else {
            return Err(bad_tables("inject: every row needs {owner, row}"));
        };
        let row = item.get("row").and_then(Value::as_object);
        let Some(id) = row.and_then(|r| r.get("id")).and_then(as_id) else {
            return Err(bad_tables("inject: every row needs an int id"));
        };
        let mut r: Row = row
            .expect("checked")
            .iter()
            .filter(|(k, _)| k.as_str() != "refs")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        r.insert("id".to_string(), Value::from(id));
        match by_owner.iter_mut().find(|(o, _)| o == owner) {
            Some((_, v)) => v.push(r),
            None => by_owner.push((owner.to_string(), vec![r])),
        }
    }
    for (owner, rs) in by_owner {
        let mut seen: HashSet<i64> = HashSet::new();
        for r in &rs {
            let id = r.get("id").and_then(Value::as_i64).unwrap_or(0);
            if !seen.insert(id) {
                let mut e = refusal(EXISTS, format!("inject: duplicate row ({owner}, {id})"));
                with(&mut e, "id", json!(id));
                return Err(e);
            }
        }
        t.insert(&owner, rs);
    }
    let empty = Vec::new();
    for fr in tables
        .get("fold_refs")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
    {
        let owner = fr
            .get("owner")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let fold = fr.get("fold").and_then(as_id);
        let members: Vec<Value> = fr
            .get("members")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(fold) = fold.filter(|f| t.has(&owner, *f)) else {
            return Err(bad_tables(&format!(
                "inject: fold_refs names no row {}",
                fold.map_or_else(|| "None".to_string(), |f| f.to_string())
            )));
        };
        if !members
            .iter()
            .all(|m| t.has(&owner, as_id(m).unwrap_or(-1)))
        {
            return Err(bad_tables(&format!(
                "inject: fold {fold} refs a missing row"
            )));
        }
        let ms: Vec<i64> = members.iter().filter_map(as_id).collect();
        for m in &ms {
            t.ref_by_member
                .entry((owner.clone(), *m))
                .or_default()
                .insert(fold);
        }
        t.fold_refs.insert((owner.clone(), fold), ms);
        if let Some(row) = t.row(&owner, fold) {
            let text = text_of(row).to_lowercase();
            t.text.insert((owner.clone(), fold), text);
        }
    }
    for g in tables
        .get("grants")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
    {
        let anchored = g.as_object().is_some_and(|m| {
            let owner = m.get("owner").and_then(Value::as_str).unwrap_or("");
            m.get("anchor")
                .and_then(Value::as_i64)
                .is_some_and(|a| t.has(owner, a))
        });
        if !anchored {
            return Err(bad_tables("inject: a grant's anchor names no row"));
        }
        let Some(gid) = g.get("grant").and_then(as_id) else {
            return Err(bad_tables("inject: a grant needs an int `grant`"));
        };
        let mut body: Map<String, Value> = g
            .as_object()
            .expect("checked")
            .iter()
            .filter(|(k, _)| k.as_str() != "grant")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let key = grant_key_of(&body);
        let owner = body.get("owner").map(py_str).unwrap_or_default();
        let anchor = body.get("anchor").and_then(Value::as_i64).unwrap_or(0);
        body.insert("grant".to_string(), Value::from(gid));
        t.grants.insert(gid, body);
        t.grant_key.insert(key, gid);
        t.grant_by_anchor
            .entry((owner, anchor))
            .or_default()
            .insert(gid);
        t.grant_seq = t.grant_seq.max(gid);
    }
    Ok(t)
}

/// The `ram_store.tools` bundle. The tables live per AGENT (keyed by its id): one
/// kernel's `mem` is never another kernel's `mem` — each kernel holds its own
/// bundle instance.
#[derive(Default)]
pub struct RamStoreBundle {
    tables: Mutex<HashMap<String, Tables>>,
}

// ─── who is calling ────────────────────────────────────────────────

fn guid(kernel: &Arc<Kernel>) -> Option<String> {
    kernel.root().and_then(|r| {
        r.meta
            .read()
            .ok()
            .and_then(|m| m.get("guid").and_then(Value::as_str).map(str::to_string))
            .filter(|g| !g.is_empty())
    })
}

/// An owner/reader NAME as a table key: a qualified `guid|id` verbatim, a bare
/// local id under this kernel's guid.
fn key(kernel: &Arc<Kernel>, name: &Value) -> Option<String> {
    let name = py_strip(name.as_str()?);
    if name.is_empty() {
        return None;
    }
    if name.contains('|') {
        return Some(name.to_string());
    }
    guid(kernel).map(|g| format!("{g}|{name}"))
}

/// A key as a caller reads it: the bare id for this kernel, qualified otherwise.
fn display(kernel: &Arc<Kernel>, key: &str) -> String {
    if let Some(g) = guid(kernel) {
        if let Some(rest) = key.strip_prefix(&format!("{g}|")) {
            return rest.to_string();
        }
    }
    key.to_string()
}

fn record(kernel: &Arc<Kernel>, id: &AgentId) -> Map<String, Value> {
    kernel
        .agents
        .get(id)
        .map(|a| a.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default()
}

/// (key, display) for whoever is calling, or a refusal.
fn caller(kernel: &Arc<Kernel>, id: &AgentId) -> Result<(String, String), Value> {
    if let Some(remote) = fantastic_kernel::current_remote_caller() {
        return Ok((remote.clone(), remote));
    }
    let Some(g) = guid(kernel) else {
        return Err(refusal_hint(
            NOT_FOUND,
            "ram_store: this kernel has no guid yet — the store keys every row by it",
            "boot the kernel root first (it mints the guid)",
        ));
    };
    match fantastic_kernel::current_origin() {
        None => {
            if !record(kernel, id).get("operator").is_some_and(py_truthy) {
                let mut r = refusal_hint(
                    UNAUTHORIZED,
                    format!("{}: the operator is not served here", id.0),
                    format!(
                        "update_agent id={} operator=true to serve the operator (as 'kernel')",
                        id.0
                    ),
                );
                with(&mut r, "side", json!("store"));
                return Err(r);
            }
            Ok((format!("{g}|{OPERATOR}"), OPERATOR.to_string()))
        }
        Some(origin) => Ok((format!("{g}|{}", origin.0), origin.0)),
    }
}

fn is_admin(kernel: &Arc<Kernel>, id: &AgentId, who: &str) -> bool {
    match record(kernel, id).get("admins") {
        Some(Value::String(s)) => s == who,
        Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(who)),
        _ => false,
    }
}

/// (owner key, caller key, caller display) for a verb that acts on ONE owner: the
/// caller's own key, or `owner` when the caller is an admin.
fn owner_for(
    kernel: &Arc<Kernel>,
    id: &AgentId,
    payload: &Value,
    verb: &str,
) -> Result<(String, String, String), Value> {
    let (me, who) = caller(kernel, id)?;
    let name = payload.get("owner").filter(|v| !v.is_null());
    let Some(name) = name else {
        return Ok((me.clone(), me, who));
    };
    let Some(owner) = key(kernel, name) else {
        let mut r = refusal(BAD_ARG, format!("{verb}: `owner` must be an agent id"));
        with(&mut r, "arg", json!("owner"));
        return Err(r);
    };
    if owner != me && !is_admin(kernel, id, &who) {
        let mut r = refusal_hint(
            UNAUTHORIZED,
            format!(
                "{verb}: {} may act only on its own rows, not {}",
                py_repr(&who),
                py_repr_value(name)
            ),
            format!(
                "a caller writes and reads its OWN key; another owner's rows are \
                 reached through a grant it gives (`share`), or by an agent listed in \
                 {}.admins",
                id.0
            ),
        );
        with(&mut r, "side", json!("store"));
        return Err(r);
    }
    Ok((owner, me, who))
}

// ─── scopes ────────────────────────────────────────────────────────

fn fold_closure(t: &Tables, owner: &str, fold: i64) -> HashSet<i64> {
    let mut out: HashSet<i64> = HashSet::new();
    let mut stack = vec![fold];
    while let Some(x) = stack.pop() {
        if !out.insert(x) {
            continue;
        }
        if let Some(refs) = t.fold_refs.get(&(owner.to_string(), x)) {
            stack.extend(refs.iter().copied());
        }
    }
    out
}

/// The rows a grant reaches, as of its bound (frozen) or following the head (live).
fn scope_ids(t: &Tables, g: &Map<String, Value>) -> HashSet<i64> {
    let owner = g.get("owner").map(py_str).unwrap_or_default();
    let scope = g.get("scope").cloned().unwrap_or(Value::Null);
    let live = g.get("mode").and_then(Value::as_str) == Some("live");
    let ids = t.ids(&owner);
    let all: HashSet<i64> = ids.iter().copied().collect();
    let mut base: HashSet<i64> = if let Some(r) = scope.get("row").and_then(Value::as_i64) {
        [r].into_iter().filter(|x| all.contains(x)).collect()
    } else if let Some(f) = scope.get("fold").and_then(Value::as_i64) {
        fold_closure(t, &owner, f)
            .into_iter()
            .filter(|x| all.contains(x))
            .collect()
    } else if let Some(r) = scope.get("range") {
        let lo = r.get("from").and_then(Value::as_i64).unwrap_or(0);
        let hi = r.get("to").and_then(Value::as_i64).unwrap_or(0);
        ids.iter()
            .copied()
            .filter(|i| *i >= lo && (live || *i <= hi))
            .collect()
    } else {
        let n = scope.get("head_until").and_then(Value::as_i64).unwrap_or(0);
        ids.iter().copied().filter(|i| live || *i <= n).collect()
    };
    if !live {
        if let Some(as_of) = g.get("as_of").and_then(Value::as_i64) {
            base.retain(|i| *i <= as_of);
        }
    }
    base
}

fn bad_scope(code: &str, msg: String) -> Value {
    let mut r = refusal(code, msg);
    with(&mut r, "arg", json!("scope"));
    r
}

/// (normalised scope, anchor) — the anchor is the scope's LOWEST existing row, so a
/// prune that touches any part of the scope removes it (FK cascade).
fn parse_scope(t: &Tables, owner: &str, scope: &Value) -> Result<(Value, i64), Value> {
    let one = scope
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.iter().next())
        .filter(|(k, _)| SCOPE_KINDS.contains(&k.as_str()));
    let Some((kind, val)) = one else {
        return Err(bad_scope(
            BAD_ARG,
            "share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, \
             {fold: <id>}, {row: <id>} — a descriptor, never a list of messages"
                .to_string(),
        ));
    };
    let ids = t.ids(owner);
    if kind == "range" {
        let Some(r) = val.as_object() else {
            return Err(bad_scope(
                BAD_ARG,
                "share: range must be {from, to}".to_string(),
            ));
        };
        let lo = r.get("from").and_then(as_id);
        let hi = r.get("to").and_then(as_id);
        let (Some(lo), Some(hi)) = (lo, hi) else {
            return Err(bad_scope(
                BAD_ARG,
                "share: range needs from <= to (row ids)".to_string(),
            ));
        };
        if lo > hi {
            return Err(bad_scope(
                BAD_ARG,
                "share: range needs from <= to (row ids)".to_string(),
            ));
        }
        let Some(first) = ids.iter().copied().find(|i| lo <= *i && *i <= hi) else {
            return Err(bad_scope(
                NOT_FOUND,
                format!("share: none of YOUR rows lie in {lo}..{hi}"),
            ));
        };
        return Ok((json!({"range": {"from": lo, "to": hi}}), first));
    }
    let Some(rid) = as_id(val) else {
        return Err(bad_scope(
            BAD_ARG,
            format!("share: {kind} must be a row id"),
        ));
    };
    if kind == "head_until" {
        match ids.first() {
            Some(first) if *first <= rid => {
                return Ok((json!({"head_until": rid}), *first));
            }
            _ => {
                return Err(bad_scope(
                    NOT_FOUND,
                    format!("share: you have no rows up to {rid}"),
                ))
            }
        }
    }
    if !t.has(owner, rid) {
        // ZERO TOLERANCE: a row that is not under YOUR key is not yours to share —
        // a foreign row, or one shared WITH you, simply is not found here.
        return Err(bad_scope(
            NOT_FOUND,
            format!("share: row {rid} is not one of YOUR rows — only rows you own can be shared"),
        ));
    }
    if kind == "fold" {
        let k = t
            .row(owner, rid)
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str);
        if !k.is_some_and(|k| REF_KINDS.contains(&k)) {
            return Err(bad_scope(
                BAD_ARG,
                format!("share: row {rid} is not a fold"),
            ));
        }
    }
    let mut out = Map::new();
    out.insert(kind.clone(), Value::from(rid));
    Ok((Value::Object(out), rid))
}

fn grant_view(kernel: &Arc<Kernel>, g: &Map<String, Value>) -> Value {
    let get = |k: &str| g.get(k).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("grant".to_string(), get("grant"));
    out.insert(
        "owner".to_string(),
        Value::from(display(
            kernel,
            &g.get("owner").map(py_str).unwrap_or_default(),
        )),
    );
    out.insert(
        "reader".to_string(),
        Value::from(display(
            kernel,
            &g.get("reader").map(py_str).unwrap_or_default(),
        )),
    );
    for k in ["scope", "mode", "as_of", "until", "why", "created_by", "ts"] {
        out.insert(k.to_string(), get(k));
    }
    Value::Object(out)
}

/// (owner key, allowed ids | None = all) for a read-side verb: `grant` reads
/// another owner's scope; else the caller's own key (or `owner`, admin).
fn readable(
    kernel: &Arc<Kernel>,
    id: &AgentId,
    t: &Tables,
    payload: &Value,
    verb: &str,
) -> Result<(String, Option<HashSet<i64>>), Value> {
    let gid = payload.get("grant").filter(|v| !v.is_null());
    let Some(gid) = gid else {
        let (owner, _, _) = owner_for(kernel, id, payload, verb)?;
        return Ok((owner, None));
    };
    let (me, who) = caller(kernel, id)?;
    let g = as_id(gid).and_then(|n| t.grants.get(&n));
    let Some(g) = g else {
        let mut r = refusal(
            NOT_FOUND,
            format!(
                "{verb}: grant {} does not exist — revoked (unshare) or its rows were pruned",
                py_repr_value(gid)
            ),
        );
        with(&mut r, "arg", json!("grant"));
        return Err(r);
    };
    let reader = g.get("reader").map(py_str).unwrap_or_default();
    if reader != me && !is_admin(kernel, id, &who) {
        let mut r = refusal(
            UNAUTHORIZED,
            format!(
                "{verb}: grant {} was given to {}, not {}",
                py_str(gid),
                py_repr(&display(kernel, &reader)),
                py_repr(&who)
            ),
        );
        with(&mut r, "side", json!("store"));
        return Err(r);
    }
    Ok((
        g.get("owner").map(py_str).unwrap_or_default(),
        Some(scope_ids(t, g)),
    ))
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One line per verb — python's first-docstring-line rule, in `VERBS` order.
const VERB_DOCS: [(&str, &str); 11] = [
    ("reflect", "Identity + posture: how many owners/rows/grants, who is admin, whether the operator is served, which optional verbs exist. No args."),
    ("boot", "No-op — the tables start empty (RAM) or are filled by `inject`."),
    ("append", "args: rows:list[dict] (req), owner:str? (admins only). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}."),
    ("read", "args: grant:int? (read another owner's scope), owner:str? (admins), after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}."),
    ("search", "args: query:str (req — case-insensitive substring), grant:int?, owner:str? (admins), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}."),
    ("share", "args: with:str (req — the reader's agent id), scope:dict (req — ONE of {head_until:<row>}, {range:{from,to}}, {fold:<id>}, {row:<id>}), mode:'frozen'|'live'? (default frozen), why:str?. Grants `with` read access to that scope of YOUR rows — only rows you own (never a foreign row, never one shared with you). An identical share returns the existing grant. Returns {grant, anchor, as_of, created}."),
    ("unshare", "args: grant:int (req). Revokes a grant YOU gave (admins: any). The reader's next read under it fails. Returns {unshared:true, grant}."),
    ("grants", "args: side:'given'|'received' (req). The grants YOU gave, or the ones given TO you. Returns {grants:[{grant, owner, reader, scope, mode, as_of, until, why, created_by, ts}]}."),
    ("prune", "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept), owner:str? (admins). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref and every grant anchored on a deleted row goes with it (FK cascade). Returns {pruned, grants_removed}."),
    ("inject", "args: tables:dict (req — a `dump`). ADMINS ONLY. Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows, grants}."),
    ("dump", "No args. ADMINS ONLY. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}], grants:[…]} — exactly what `inject` takes back. Returns {tables}."),
];

impl RamStoreBundle {
    fn with_tables<T>(&self, id: &AgentId, f: impl FnOnce(&mut Tables) -> T) -> T {
        let mut all = self.tables.lock().expect("ram_store poisoned");
        f(all.entry(id.0.clone()).or_default())
    }

    fn reflect(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        let (owners, rows, grants) = self.with_tables(id, |t| {
            (
                t.rows.values().filter(|v| !v.is_empty()).count(),
                t.rows.values().map(HashMap::len).sum::<usize>(),
                t.grants.len(),
            )
        });
        let rec = record(kernel, id);
        let mut verbs = Map::new();
        for (n, d) in VERB_DOCS {
            verbs.insert(n.to_string(), Value::from(d));
        }
        let mut out = Map::new();
        out.insert("id".to_string(), Value::from(id.0.clone()));
        out.insert(
            "sentence".to_string(),
            Value::from(
                "Memory store in RAM — rows per owner (the caller), folds, grants, search; \
                 ephemeral: everything is lost when the process exits.",
            ),
        );
        out.insert("durable".to_string(), Value::Bool(false));
        out.insert("owners".to_string(), Value::from(owners));
        out.insert("rows".to_string(), Value::from(rows));
        out.insert("grants".to_string(), Value::from(grants));
        out.insert(
            "operator".to_string(),
            Value::Bool(rec.get("operator").is_some_and(py_truthy)),
        );
        out.insert(
            "admins".to_string(),
            rec.get("admins")
                .filter(|a| py_truthy(a))
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        out.insert("optional_verbs".to_string(), json!(["search"]));
        out.insert("verbs".to_string(), Value::Object(verbs));
        Value::Object(out)
    }

    fn append(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (owner, _, _) = match owner_for(kernel, id, payload, "append") {
            Ok(o) => o,
            Err(r) => return r,
        };
        let rows = payload.get("rows").and_then(Value::as_array);
        let rows = match rows {
            Some(r) if !r.is_empty() && r.iter().all(Value::is_object) => r,
            _ => {
                let mut r = refusal(BAD_ARG, "append: `rows` must be a non-empty list of rows");
                with(&mut r, "arg", json!("rows"));
                return r;
            }
        };
        self.with_tables(id, |t| {
            let mut staged: Vec<Row> = Vec::new();
            let mut batch: HashSet<i64> = HashSet::new();
            for r in rows {
                let r = r.as_object().expect("checked");
                let rid = match r.get("id") {
                    None | Some(Value::Null) => kernel.mint_id(),
                    Some(v) => match (v, as_id(v)) {
                        (Value::Number(_), Some(n)) => n,
                        _ => {
                            let mut e = refusal(
                                BAD_ARG,
                                format!(
                                    "append: a row id is a kernel int, got {}",
                                    py_repr_value(v)
                                ),
                            );
                            with(&mut e, "arg", json!("rows"));
                            return e;
                        }
                    },
                };
                if t.has(&owner, rid) || batch.contains(&rid) {
                    let mut e = refusal(
                        EXISTS,
                        format!("append: row {rid} already exists for this owner"),
                    );
                    with(&mut e, "id", json!(rid));
                    return e;
                }
                batch.insert(rid);
                let mut row = r.clone();
                row.insert("id".to_string(), Value::from(rid));
                staged.push(row);
            }
            for r in &staged {
                let Some(refs) = r.get("refs").filter(|v| !v.is_null()) else {
                    continue;
                };
                let rid = r.get("id").and_then(Value::as_i64).unwrap_or(0);
                let list = refs
                    .as_array()
                    .filter(|a| a.iter().all(|m| as_id(m).is_some()));
                let Some(list) = list else {
                    let mut e = refusal(BAD_ARG, format!("append: row {rid} refs must be row ids"));
                    with(&mut e, "arg", json!("rows"));
                    return e;
                };
                let missing_refs: Vec<i64> = list
                    .iter()
                    .filter_map(as_id)
                    .filter(|m| !(t.has(&owner, *m) || batch.contains(m)))
                    .collect();
                if !missing_refs.is_empty() {
                    let mut e = refusal(
                        NOT_FOUND,
                        format!(
                            "append: row {rid} refs rows that do not exist for this owner: {:?}",
                            missing_refs
                        ),
                    );
                    with(&mut e, "arg", json!("rows"));
                    return e;
                }
            }
            let ids: Vec<Value> = staged
                .iter()
                .map(|r| r.get("id").cloned().unwrap_or(Value::Null))
                .collect();
            let n = staged.len();
            t.insert(&owner, staged);
            json!({"ids": ids, "appended": n})
        })
    }

    fn read(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        self.with_tables(id, |t| {
            let (owner, allowed) = match readable(kernel, id, t, payload, "read") {
                Ok(r) => r,
                Err(e) => return e,
            };
            let mut ids = t.ids(&owner);
            if let Some(allowed) = &allowed {
                ids.retain(|i| allowed.contains(i));
            }
            let row_of = |i: &i64| t.row(&owner, *i).cloned().unwrap_or_default();
            if let Some(flk) = payload
                .get("from_last_kind")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let last = ids
                    .iter()
                    .rev()
                    .find(|i| row_of(i).get("kind").and_then(Value::as_str) == Some(flk))
                    .copied();
                if let Some(last) = last {
                    ids.retain(|i| *i >= last);
                }
            }
            if let Some(kinds) = payload
                .get("kinds")
                .and_then(Value::as_array)
                .filter(|k| !k.is_empty())
            {
                ids.retain(|i| {
                    let k = row_of(i).get("kind").cloned().unwrap_or(Value::Null);
                    kinds.contains(&k)
                });
            }
            if let Some(m) = payload
                .get("match")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty())
            {
                ids.retain(|i| {
                    let r = row_of(i);
                    m.iter().all(|(k, v)| r.get(k).unwrap_or(&Value::Null) == v)
                });
            }
            if let Some(want) = payload.get("ids").and_then(Value::as_array) {
                let wanted: HashSet<Option<i64>> = want.iter().map(as_id).collect();
                ids.retain(|i| wanted.contains(&Some(*i)));
            }
            let after = payload.get("after").and_then(as_id);
            let before = payload.get("before").and_then(as_id);
            let desc = payload.get("order").and_then(Value::as_str) == Some("desc");
            if desc {
                ids.reverse();
                if let Some(a) = after {
                    ids.retain(|i| *i < a);
                }
            } else if let Some(a) = after {
                ids.retain(|i| *i > a);
            }
            if let Some(b) = before {
                ids.retain(|i| *i < b);
            }
            let total = ids.len();
            let limit = payload.get("limit").and_then(as_id);
            let page: Vec<i64> = match limit {
                Some(l) => ids
                    .iter()
                    .take(usize::try_from(l).unwrap_or(usize::MAX))
                    .copied()
                    .collect(),
                None => ids.clone(),
            };
            let next = match (limit, page.last()) {
                (Some(_), Some(last)) if total > page.len() => Value::from(*last),
                _ => Value::Null,
            };
            let rows: Vec<Value> = page
                .iter()
                .filter_map(|i| t.get(&owner, *i).map(Value::Object))
                .collect();
            obj([
                ("rows", Value::Array(rows)),
                ("next", next),
                ("total", Value::from(total)),
            ])
        })
    }

    fn search(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let q = py_strip(
            &payload
                .get("query")
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default(),
        )
        .to_lowercase();
        if q.is_empty() {
            return missing("search", "query");
        }
        self.with_tables(id, |t| {
            let (owner, allowed) = match readable(kernel, id, t, payload, "search") {
                Ok(r) => r,
                Err(e) => return e,
            };
            let mut ids = t.ids(&owner);
            if let Some(allowed) = &allowed {
                ids.retain(|i| allowed.contains(i));
            }
            if let Some(want) = payload
                .get("ids")
                .and_then(Value::as_array)
                .filter(|w| !w.is_empty())
            {
                let mut scope: HashSet<i64> = HashSet::new();
                for x in want {
                    if let Some(rid) = as_id(x) {
                        scope.extend(fold_closure(t, &owner, rid));
                    }
                }
                ids.retain(|i| scope.contains(i));
            }
            let limit = payload.get("limit").and_then(as_id).unwrap_or(100);
            let width = payload.get("width").and_then(as_id).unwrap_or(200);
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            let width = usize::try_from(width).unwrap_or(usize::MAX);
            let mut matches: Vec<Value> = Vec::new();
            let mut total = 0usize;
            for i in &ids {
                let hit = t
                    .text
                    .get(&(owner.clone(), *i))
                    .is_some_and(|txt| txt.contains(&q));
                if !hit {
                    continue;
                }
                let Some(row) = t.row(&owner, *i) else {
                    continue;
                };
                let text = text_of(row);
                for (n, line) in py_splitlines(&text).into_iter().enumerate() {
                    if line.to_lowercase().contains(&q) {
                        total += 1;
                        if matches.len() < limit {
                            let shown: String = py_strip(line).chars().take(width).collect();
                            matches.push(json!({"id": i, "line": n + 1, "text": shown}));
                        }
                    }
                }
            }
            let truncated = total > matches.len();
            json!({"matches": matches, "total": total, "truncated": truncated})
        })
    }

    fn share(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (me, _who) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let Some(reader) = payload.get("with").and_then(|w| key(kernel, w)) else {
            return missing("share", "with");
        };
        if reader == me {
            let mut r = refusal(BAD_ARG, "share: you already read your own rows");
            with(&mut r, "arg", json!("with"));
            return r;
        }
        let mode = payload
            .get("mode")
            .filter(|m| py_truthy(m))
            .cloned()
            .unwrap_or_else(|| json!("frozen"));
        let Some(mode) = mode
            .as_str()
            .filter(|m| MODES.contains(m))
            .map(str::to_string)
        else {
            let mut r = refusal(BAD_ARG, "share: mode must be one of ('frozen', 'live')");
            with(&mut r, "arg", json!("mode"));
            return r;
        };
        let why_raw = payload
            .get("why")
            .filter(|w| py_truthy(w))
            .map(py_str)
            .unwrap_or_default();
        let why: String = py_split_whitespace(&why_raw)
            .join(" ")
            .chars()
            .take(300)
            .collect();
        self.with_tables(id, |t| {
            let (scope, anchor) =
                match parse_scope(t, &me, payload.get("scope").unwrap_or(&Value::Null)) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
            let ids = t.ids(&me);
            let as_of = match (mode.as_str(), ids.last()) {
                ("frozen", Some(last)) => Value::from(*last),
                _ => Value::Null,
            };
            let mut g = Map::new();
            g.insert("owner".to_string(), Value::from(me.clone()));
            g.insert("reader".to_string(), Value::from(reader.clone()));
            g.insert("scope".to_string(), scope);
            g.insert("mode".to_string(), Value::from(mode.clone()));
            g.insert("anchor".to_string(), Value::from(anchor));
            g.insert("as_of".to_string(), as_of);
            // reserved: a row limit / time bound — unused
            g.insert("until".to_string(), Value::Null);
            g.insert(
                "why".to_string(),
                if why.is_empty() {
                    Value::Null
                } else {
                    Value::from(why.clone())
                },
            );
            g.insert(
                "created_by".to_string(),
                fantastic_kernel::current_task().map_or(Value::Null, |t| Value::from(t.get())),
            );
            g.insert("ts".to_string(), Value::from(now_secs()));
            let (gid, created) = t.add_grant(g);
            let as_of = t
                .grants
                .get(&gid)
                .and_then(|g| g.get("as_of").cloned())
                .unwrap_or(Value::Null);
            obj([
                ("grant", Value::from(gid)),
                ("anchor", Value::from(anchor)),
                ("as_of", as_of),
                ("created", Value::from(created)),
            ])
        })
    }

    fn unshare(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (me, who) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let Some(gid) = payload.get("grant").and_then(as_id) else {
            return missing("unshare", "grant");
        };
        let admin = is_admin(kernel, id, &who);
        self.with_tables(id, |t| {
            let Some(g) = t.grants.get(&gid) else {
                let mut r = refusal(NOT_FOUND, format!("unshare: no grant {gid}"));
                with(&mut r, "arg", json!("grant"));
                return r;
            };
            if g.get("owner").map(py_str).as_deref() != Some(me.as_str()) && !admin {
                let mut r = refusal(
                    UNAUTHORIZED,
                    format!("unshare: grant {gid} is not yours to revoke"),
                );
                with(&mut r, "side", json!("store"));
                return r;
            }
            t.drop_grant(gid);
            json!({"unshared": true, "grant": gid})
        })
    }

    fn grants(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (me, _) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let field = match payload.get("side").and_then(Value::as_str) {
            Some("given") => "owner",
            Some("received") => "reader",
            _ => {
                let mut r = refusal(BAD_ARG, "grants: side must be 'given' or 'received'");
                with(&mut r, "arg", json!("side"));
                return r;
            }
        };
        self.with_tables(id, |t| {
            let out: Vec<Value> = t
                .grants
                .values()
                .filter(|g| g.get(field).map(py_str).as_deref() == Some(me.as_str()))
                .map(|g| grant_view(kernel, g))
                .collect();
            obj([("grants", Value::Array(out))])
        })
    }

    fn prune(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (owner, _, _) = match owner_for(kernel, id, payload, "prune") {
            Ok(o) => o,
            Err(r) => return r,
        };
        let before = payload.get("before_id").and_then(as_id);
        let want = payload.get("ids").and_then(Value::as_array);
        if before.is_none() && want.is_none() {
            return missing("prune", "before_id");
        }
        let wanted: HashSet<Option<i64>> = want
            .map(|w| w.iter().map(as_id).collect())
            .unwrap_or_default();
        let keep: Vec<Value> = payload
            .get("keep")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.with_tables(id, |t| {
            let mut doomed: HashSet<i64> = HashSet::new();
            for i in t.ids(&owner) {
                let kind = t
                    .row(&owner, i)
                    .and_then(|r| r.get("kind"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if keep.contains(&kind) {
                    continue;
                }
                if before.is_some_and(|b| i < b) || (want.is_some() && wanted.contains(&Some(i))) {
                    doomed.insert(i);
                }
            }
            let (n, g) = t.delete(&owner, &doomed);
            json!({"pruned": n, "grants_removed": g})
        })
    }

    fn inject(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (_, who) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        if !is_admin(kernel, id, &who) {
            let mut r = refusal(
                UNAUTHORIZED,
                format!("inject: {} is not an admin of {}", py_repr(&who), id.0),
            );
            with(&mut r, "side", json!("store"));
            return r;
        }
        let t = match load_tables(payload.get("tables").unwrap_or(&Value::Null)) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let rows: usize = t.rows.values().map(HashMap::len).sum();
        let grants = t.grants.len();
        self.tables
            .lock()
            .expect("ram_store poisoned")
            .insert(id.0.clone(), t);
        json!({"rows": rows, "grants": grants})
    }

    fn dump(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        let (_, who) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        if !is_admin(kernel, id, &who) {
            let mut r = refusal(
                UNAUTHORIZED,
                format!("dump: {} is not an admin of {}", py_repr(&who), id.0),
            );
            with(&mut r, "side", json!("store"));
            return r;
        }
        let tables = self.with_tables(id, |t| t.dump());
        let mut out = Map::new();
        out.insert("tables".to_string(), tables);
        Value::Object(out)
    }
}

#[async_trait]
impl Bundle for RamStoreBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// `operator` is a bool; `admins` a list of ids.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        if meta.get("operator").is_some_and(|v| !v.is_boolean()) {
            return Some("operator must be true or false".to_string());
        }
        match meta.get("admins") {
            None | Some(Value::String(_)) => None,
            Some(Value::Array(a))
                if a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())) =>
            {
                None
            }
            Some(_) => {
                Some("admins must be a list of agent ids ('kernel' = the operator)".to_string())
            }
        }
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let out = match verb {
            "reflect" => self.reflect(agent_id, kernel),
            // No-op — the tables start empty (RAM) or are filled by `inject`.
            "boot" => return Ok(None),
            "append" => self.append(agent_id, payload, kernel),
            "read" => self.read(agent_id, payload, kernel),
            "search" => self.search(agent_id, payload, kernel),
            "share" => self.share(agent_id, payload, kernel),
            "unshare" => self.unshare(agent_id, payload, kernel),
            "grants" => self.grants(agent_id, payload, kernel),
            "prune" => self.prune(agent_id, payload, kernel),
            "inject" => self.inject(agent_id, payload, kernel),
            "dump" => self.dump(agent_id, kernel),
            _ => {
                let t = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = refusal(
                    UNHANDLED,
                    format!("ram_store: unknown type {}", py_repr_value(&t)),
                );
                with(&mut r, "verb", t);
                r
            }
        };
        Ok(Some(out))
    }

    /// A deleted store is GONE: a re-created agent under the same id starts empty
    /// (python's tables live on the agent instance, which a delete drops).
    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.tables
            .lock()
            .expect("ram_store poisoned")
            .remove(&agent_id.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
