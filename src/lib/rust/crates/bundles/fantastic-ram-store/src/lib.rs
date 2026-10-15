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
//!   text_idx   (owner, id) → lowercased text       the FTS5 counterpart; derived
//! ```
//!
//! ACCESS: a caller reads and writes ONLY its own key (append / read / search /
//! prune). `inject` / `dump` act on the whole store; they are plain verbs, and the
//! kernel's call gate decides who may reach this agent — and each verb — at all.
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
    py_json_dumps, py_repr_value, py_splitlines, py_str, py_strip, py_truthy, py_type_name,
};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    missing, refusal, refusal_hint, with, BAD_ARG, EXISTS, NOT_FOUND, UNHANDLED,
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
/// under the store's lock, so every later call would answer "ram_store
/// poisoned". So a reply that carries a row is
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

/// python `x in container` for the JSON value a payload carried. `Err` is the
/// TypeError python raises (`'in <string>' requires string as left operand`,
/// `argument of type 'int' is not iterable`, `unhashable type: 'list'`).
fn py_in(x: &Value, container: &Value) -> Result<bool, BundleError> {
    let type_error = |m: String| -> BundleError { format!("TypeError: {m}").into() };
    match container {
        Value::Array(items) => Ok(items.contains(x)),
        Value::String(s) => match x {
            Value::String(needle) => Ok(s.contains(needle.as_str())),
            other => Err(type_error(format!(
                "'in <string>' requires string as left operand, not {}",
                py_type_name(other)
            ))),
        },
        Value::Object(m) => match x {
            Value::String(k) => Ok(m.contains_key(k)),
            Value::Array(_) | Value::Object(_) => Err(type_error(format!(
                "unhashable type: '{}'",
                py_type_name(x)
            ))),
            _ => Ok(false),
        },
        other => Err(type_error(format!(
            "argument of type '{}' is not iterable",
            py_type_name(other)
        ))),
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

/// The store's tables — RAM maps in sqlite's shape (see the module docs).
#[derive(Default)]
struct Tables {
    rows: BTreeMap<String, HashMap<i64, Row>>,
    order: HashMap<String, Vec<i64>>,
    fold_refs: BTreeMap<(String, i64), Vec<i64>>,
    ref_by_member: HashMap<(String, i64), HashSet<i64>>,
    text: HashMap<(String, i64), String>,
    /// (owner, name) → the ids of the rows carrying that `name`, ascending: the
    /// kernel looks a caller's execution name up on every named call, and a scan
    /// of every row there grows with the whole history.
    by_name: HashMap<(String, String), Vec<i64>>,
}

impl Tables {
    fn has(&self, owner: &str, rid: i64) -> bool {
        self.rows.get(owner).is_some_and(|t| t.contains_key(&rid))
    }

    fn ids(&self, owner: &str) -> Vec<i64> {
        self.order.get(owner).cloned().unwrap_or_default()
    }

    fn named(&self, owner: &str, name: &str) -> Vec<i64> {
        self.by_name
            .get(&(owner.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_default()
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
            if let Some(name) = stored.get("name").and_then(Value::as_str) {
                let named = self
                    .by_name
                    .entry((owner.to_string(), name.to_string()))
                    .or_default();
                let at = named.partition_point(|x| *x <= rid);
                named.insert(at, rid);
            }
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
    /// Returns the rows removed.
    fn delete(&mut self, owner: &str, doomed: &HashSet<i64>) -> usize {
        let doomed: HashSet<i64> = doomed
            .iter()
            .copied()
            .filter(|r| self.has(owner, *r))
            .collect();
        for rid in &doomed {
            if let Some(row) = self.rows.get_mut(owner).and_then(|t| t.remove(rid)) {
                if let Some(name) = row.get("name").and_then(Value::as_str) {
                    let nk = (owner.to_string(), name.to_string());
                    if let Some(named) = self.by_name.get_mut(&nk) {
                        named.retain(|i| i != rid);
                        if named.is_empty() {
                            self.by_name.remove(&nk);
                        }
                    }
                }
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
        }
        if !doomed.is_empty() {
            if let Some(order) = self.order.get_mut(owner) {
                order.retain(|i| !doomed.contains(i));
            }
        }
        doomed.len()
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
        let mut out = Map::new();
        out.insert("rows".to_string(), Value::Array(rows));
        out.insert("fold_refs".to_string(), Value::Array(fold_refs));
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

/// (key, display) for whoever is calling — the operator (and the kernel's own
/// bookkeeping) as `kernel` — or a refusal. A caller is always local: a call a
/// bridge leg serves is the leg's.
fn caller(kernel: &Arc<Kernel>) -> Result<(String, String), Value> {
    let Some(g) = guid(kernel) else {
        return Err(refusal_hint(
            NOT_FOUND,
            "ram_store: this kernel has no guid yet — the store keys every row by it",
            "boot the kernel root first (it mints the guid)",
        ));
    };
    match fantastic_kernel::current_origin() {
        None => Ok((format!("{g}|{OPERATOR}"), OPERATOR.to_string())),
        Some(origin) => Ok((format!("{g}|{}", origin.0), origin.0)),
    }
}

// ─── folds ─────────────────────────────────────────────────────────

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

/// One line per verb — python's first-docstring-line rule, in `VERBS` order.
const VERB_DOCS: [(&str, &str); 8] = [
    ("reflect", "Identity + posture: how many owners/rows, which optional verbs exist. No args."),
    ("boot", "No-op — the tables start empty (RAM) or are filled by `inject`."),
    ("append", "args: rows:list[dict] (req). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}."),
    ("read", "args: after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}."),
    ("search", "args: query:str (req — case-insensitive substring), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}."),
    ("prune", "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). Returns {pruned}."),
    ("inject", "args: tables:dict (req — a `dump`). Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}."),
    ("dump", "No args. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}."),
];

impl RamStoreBundle {
    fn with_tables<T>(&self, id: &AgentId, f: impl FnOnce(&mut Tables) -> T) -> T {
        let mut all = self.tables.lock().expect("ram_store poisoned");
        f(all.entry(id.0.clone()).or_default())
    }

    fn reflect(&self, id: &AgentId) -> Value {
        let (owners, rows) = self.with_tables(id, |t| {
            (
                t.rows.values().filter(|v| !v.is_empty()).count(),
                t.rows.values().map(HashMap::len).sum::<usize>(),
            )
        });
        let mut out = Map::new();
        out.insert("id".to_string(), Value::from(id.0.clone()));
        out.insert(
            "sentence".to_string(),
            Value::from(
                "Memory store in RAM — rows per owner (the caller), folds, search; \
                 ephemeral: everything is lost when the process exits.",
            ),
        );
        out.insert("durable".to_string(), Value::Bool(false));
        out.insert("owners".to_string(), Value::from(owners));
        out.insert("rows".to_string(), Value::from(rows));
        out.insert("optional_verbs".to_string(), json!(["search"]));
        Value::Object(out)
    }

    fn append(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
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
                // python `t.has(owner, m) or m in batch` on the RAW ref: the
                // tables are keyed by int, so a digit STRING passes `_as_id`
                // and is still never found.
                let missing_refs: Vec<Value> = list
                    .iter()
                    .filter(|m| {
                        let held = m.is_number()
                            && as_id(m).is_some_and(|i| t.has(&owner, i) || batch.contains(&i));
                        !held
                    })
                    .cloned()
                    .collect();
                if !missing_refs.is_empty() {
                    let mut e = refusal(
                        NOT_FOUND,
                        format!(
                            "append: row {rid} refs rows that do not exist for this owner: {}",
                            py_repr_value(&Value::Array(missing_refs))
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
            let (owner, _) = match caller(kernel) {
                Ok(r) => r,
                Err(e) => return e,
            };
            // A `name` to match starts from the rows carrying it — the same ids, in
            // the same order, as the scan below would keep.
            let mut ids = match payload
                .get("match")
                .and_then(|m| m.get("name"))
                .and_then(Value::as_str)
            {
                Some(name) => t.named(&owner, name),
                None => t.ids(&owner),
            };
            // Borrowed, never cloned: a filter reads one field of each row.
            let field = |i: &i64, k: &str| t.row(&owner, *i).and_then(|r| r.get(k));
            if let Some(flk) = payload
                .get("from_last_kind")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let last = t
                    .ids(&owner)
                    .iter()
                    .rev()
                    .find(|i| field(i, "kind").and_then(Value::as_str) == Some(flk))
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
                ids.retain(|i| kinds.contains(field(i, "kind").unwrap_or(&Value::Null)));
            }
            if let Some(m) = payload
                .get("match")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty())
            {
                ids.retain(|i| {
                    m.iter()
                        .all(|(k, v)| field(i, k).unwrap_or(&Value::Null) == v)
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
            // `ids[:limit] if limit else ids` — a `0` (the string "0" included,
            // which `_as_id` passes) is falsy: no limit.
            let limit = payload.get("limit").and_then(as_id).filter(|l| *l != 0);
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
            let (owner, _) = match caller(kernel) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let mut ids = t.ids(&owner);
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
            // `_as_id(limit) or 100` / `or 200`: a `0` falls back too.
            let limit = payload
                .get("limit")
                .and_then(as_id)
                .filter(|l| *l != 0)
                .unwrap_or(100);
            let width = payload
                .get("width")
                .and_then(as_id)
                .filter(|w| *w != 0)
                .unwrap_or(200);
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

    /// `Err` is python's TypeError out of `kind in keep` — a raise.
    fn prune(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Value, BundleError> {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return Ok(r),
        };
        let before = payload.get("before_id").and_then(as_id);
        let want = payload.get("ids").and_then(Value::as_array);
        if before.is_none() && want.is_none() {
            return Ok(missing("prune", "before_id"));
        }
        let wanted: HashSet<Option<i64>> = want
            .map(|w| w.iter().map(as_id).collect())
            .unwrap_or_default();
        // `keep = payload.get("keep") or []`, then `row.get("kind") in keep` —
        // python's `in` on WHATEVER arrived: a list is membership, a string a
        // SUBSTRING test, a dict its keys, and anything else raises.
        let keep: Value = payload
            .get("keep")
            .filter(|v| py_truthy(v))
            .cloned()
            .unwrap_or_else(|| json!([]));
        self.with_tables(id, |t| {
            let mut doomed: HashSet<i64> = HashSet::new();
            for i in t.ids(&owner) {
                let kind = t
                    .row(&owner, i)
                    .and_then(|r| r.get("kind"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if py_in(&kind, &keep)? {
                    continue;
                }
                if before.is_some_and(|b| i < b) || (want.is_some() && wanted.contains(&Some(i))) {
                    doomed.insert(i);
                }
            }
            let n = t.delete(&owner, &doomed);
            Ok(json!({"pruned": n}))
        })
    }

    fn inject(&self, id: &AgentId, payload: &Value) -> Value {
        let t = match load_tables(payload.get("tables").unwrap_or(&Value::Null)) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let rows: usize = t.rows.values().map(HashMap::len).sum();
        self.tables
            .lock()
            .expect("ram_store poisoned")
            .insert(id.0.clone(), t);
        json!({"rows": rows})
    }

    fn dump(&self, id: &AgentId) -> Value {
        let tables = self.with_tables(id, |t| t.dump());
        let mut out = Map::new();
        out.insert("tables".to_string(), tables);
        Value::Object(out)
    }
}

#[async_trait]
impl Bundle for RamStoreBundle {
    /// The CATALOG name — python's entry-point name, not the handler module
    /// (`ram_store.tools` leaked into every `bundles=` line of a system prompt).
    fn name(&self) -> &str {
        "ram_store"
    }

    fn verb_docs(&self) -> Value {
        let mut verbs = Map::new();
        for (n, d) in VERB_DOCS {
            verbs.insert(n.to_string(), Value::from(d));
        }
        Value::Object(verbs)
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let out = match verb {
            "reflect" => self.reflect(agent_id),
            // No-op — the tables start empty (RAM) or are filled by `inject`.
            "boot" => return Ok(None),
            "append" => self.append(agent_id, payload, kernel),
            "read" => self.read(agent_id, payload, kernel),
            "search" => self.search(agent_id, payload, kernel),
            "prune" => self.prune(agent_id, payload, kernel)?,
            "inject" => self.inject(agent_id, payload),
            "dump" => self.dump(agent_id),
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
