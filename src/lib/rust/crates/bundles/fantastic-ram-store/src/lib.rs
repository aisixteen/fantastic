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
use fantastic_bundle::{py_json_dumps, py_repr_value, py_splitlines, py_strip};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::outcome::{body_at, Blob};
use fantastic_kernel::refusal::{missing, refusal, refusal_hint, with, BAD_ARG, EXISTS, NOT_FOUND};
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

pub mod record;
pub mod verbs;

const OPERATOR: &str = "kernel";

/// One stored row (without `refs` — those live in `fold_refs`).
type Row = Map<String, Value>;

/// A row id inside a stored row or a dump: a positive JSON integer.
pub fn as_id(x: &Value) -> Option<i64> {
    match x {
        Value::Number(n) => n.as_i64().filter(|i| *i > 0),
        _ => None,
    }
}

/// The `refs` list of a staged row, when it has one ([`stage_rows`] checked it).
pub fn row_refs(row: &Map<String, Value>) -> Option<&Vec<Value>> {
    row.get("refs").filter(|v| !v.is_null()).and_then(Value::as_array)
}

/// The rows of an `append` as maps — shared by every store that answers the verb:
/// each row gets the id it carries or one minted, no id twice in the batch. The
/// engine already checked the types of `id` and `refs`; a store still checks
/// whether the rows and the refs exist for its owner.
pub fn stage_rows(
    rows: &[crate::verbs::AppendRow],
    mut mint: impl FnMut() -> i64,
) -> Result<Vec<Map<String, Value>>, Value> {
    let mut staged: Vec<Map<String, Value>> = Vec::new();
    let mut batch: HashSet<i64> = HashSet::new();
    for r in rows {
        let rid = r.id.unwrap_or_else(&mut mint);
        if !batch.insert(rid) {
            let mut e = refusal(EXISTS, format!("append: row {rid} already exists for this owner"));
            with(&mut e, "id", json!(rid));
            return Err(e);
        }
        let mut row = r.rest.clone();
        row.insert("id".to_string(), Value::from(rid));
        if let Some(refs) = &r.refs {
            row.insert("refs".to_string(), json!(refs));
        }
        staged.push(row);
    }
    Ok(staged)
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
    /// (owner, ref) → (media type, raw bytes): `put_bytes` / `get_bytes`. Not rows:
    /// `read`, `search` and `dump` never see them.
    blobs: HashMap<(String, String), (String, Vec<u8>)>,
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
fn load_tables(tables: &verbs::StoreTables) -> Result<Tables, Value> {
    let mut t = Tables::default();
    let mut by_owner: Vec<(String, Vec<Row>)> = Vec::new();
    for item in tables.rows.iter().flatten() {
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
    for fr in tables.fold_refs.iter().flatten() {
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

/// What the bundle is for — its one short man text.
const MAN: &str = "A memory store held in RAM: conversation rows per owner (the caller), folds, search, and raw bytes by ref. Everything is lost when the process exits.";

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

    fn append(&self, id: &AgentId, a: &verbs::Append, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let rows = &a.rows;
        self.with_tables(id, |t| {
            let staged = match stage_rows(rows, || kernel.mint_id()) {
                Ok(s) => s,
                Err(e) => return e,
            };
            let batch: HashSet<i64> = staged.iter().filter_map(|r| r.get("id").and_then(as_id)).collect();
            for r in &staged {
                let rid = r.get("id").and_then(Value::as_i64).unwrap_or(0);
                if t.has(&owner, rid) {
                    let mut e = refusal(
                        EXISTS,
                        format!("append: row {rid} already exists for this owner"),
                    );
                    with(&mut e, "id", json!(rid));
                    return e;
                }
            }
            for r in &staged {
                let Some(list) = row_refs(r) else {
                    continue;
                };
                let rid = r.get("id").and_then(Value::as_i64).unwrap_or(0);
                let missing_refs: Vec<Value> = list
                    .iter()
                    .filter(|m| !as_id(m).is_some_and(|i| t.has(&owner, i) || batch.contains(&i)))
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

    fn read(&self, id: &AgentId, a: &verbs::Read, kernel: &Arc<Kernel>) -> Value {
        self.with_tables(id, |t| {
            let (owner, _) = match caller(kernel) {
                Ok(r) => r,
                Err(e) => return e,
            };
            // A `name` to match starts from the rows carrying it — the same ids, in
            // the same order, as the scan below would keep.
            let mut ids = match a
                .r#match
                .as_ref()
                .and_then(|m| m.get("name"))
                .and_then(Value::as_str)
            {
                Some(name) => t.named(&owner, name),
                None => t.ids(&owner),
            };
            // Borrowed, never cloned: a filter reads one field of each row.
            let field = |i: &i64, k: &str| t.row(&owner, *i).and_then(|r| r.get(k));
            if let Some(flk) = a.from_last_kind.as_deref().filter(|s| !s.is_empty()) {
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
            if let Some(kinds) = a.kinds.as_ref().filter(|k| !k.is_empty()) {
                ids.retain(|i| kinds.contains(field(i, "kind").unwrap_or(&Value::Null)));
            }
            if let Some(m) = a.r#match.as_ref().filter(|m| !m.is_empty()) {
                ids.retain(|i| {
                    m.iter()
                        .all(|(k, v)| field(i, k).unwrap_or(&Value::Null) == v)
                });
            }
            if let Some(want) = &a.ids {
                let wanted: HashSet<i64> = want.iter().copied().collect();
                ids.retain(|i| wanted.contains(i));
            }
            if a.order == Some(verbs::Order::Desc) {
                ids.reverse();
                if let Some(after) = a.after {
                    ids.retain(|i| *i < after);
                }
            } else if let Some(after) = a.after {
                ids.retain(|i| *i > after);
            }
            if let Some(b) = a.before {
                ids.retain(|i| *i < b);
            }
            let total = ids.len();
            // `limit` 0 is no limit.
            let limit = a.limit.filter(|l| *l != 0);
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

    fn search(&self, id: &AgentId, a: &verbs::Search, kernel: &Arc<Kernel>) -> Value {
        let q = py_strip(&a.query).to_lowercase();
        self.with_tables(id, |t| {
            let (owner, _) = match caller(kernel) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let mut ids = t.ids(&owner);
            if let Some(want) = a.ids.as_ref().filter(|w| !w.is_empty()) {
                let mut scope: HashSet<i64> = HashSet::new();
                for rid in want {
                    scope.extend(fold_closure(t, &owner, *rid));
                }
                ids.retain(|i| scope.contains(i));
            }
            // a `0` falls back to the default too
            let limit = a.limit.filter(|l| *l != 0).unwrap_or(100);
            let width = a.width.filter(|w| *w != 0).unwrap_or(200);
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

    fn prune(&self, id: &AgentId, a: &verbs::Prune, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let wanted: HashSet<i64> = a.ids.iter().flatten().copied().collect();
        let keep: &[Value] = a.keep.as_deref().unwrap_or(&[]);
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
                if a.before_id.is_some_and(|b| i < b) || (a.ids.is_some() && wanted.contains(&i)) {
                    doomed.insert(i);
                }
            }
            let n = t.delete(&owner, &doomed);
            json!({"pruned": n})
        })
    }

    fn put_bytes(
        &self,
        id: &AgentId,
        a: &verbs::PutBytes,
        body: Option<&Blob>,
        kernel: &Arc<Kernel>,
    ) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let Some(bytes) = body_at(body, "data") else {
            return missing("put_bytes", "data (the raw bytes, as the request body)");
        };
        let n = bytes.len();
        let key = a.ref_name.clone();
        let media = a.media_type.clone();
        self.with_tables(id, |t| {
            t.blobs
                .insert((owner, key.clone()), (media.clone(), bytes.to_vec()));
        });
        json!({"ref": key, "media_type": media, "bytes": n})
    }

    fn get_bytes(
        &self,
        id: &AgentId,
        a: &verbs::GetBytes,
        kernel: &Arc<Kernel>,
    ) -> (Value, Option<Blob>) {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return (r, None),
        };
        let key = a.ref_name.clone();
        let found = self.with_tables(id, |t| t.blobs.get(&(owner, key.clone())).cloned());
        match found {
            Some((media, bytes)) => (
                json!({"ref": key, "media_type": media, "bytes": bytes.len()}),
                Some(Blob::key("data", bytes)),
            ),
            None => {
                let mut r = refusal(NOT_FOUND, format!("get_bytes: no bytes under ref `{key}`"));
                with(&mut r, "ref", Value::from(key));
                (r, None)
            }
        }
    }

    fn inject(&self, id: &AgentId, a: &verbs::Inject) -> Value {
        let t = match load_tables(&a.tables) {
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
impl Handler for RamStoreBundle {
    type Call = verbs::Call;

    /// The CATALOG name — python's entry-point name, not the handler module
    /// (`ram_store.tools` leaked into every `bundles=` line of a system prompt).
    fn name(&self) -> &str {
        "ram_store"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        use verbs::Call;
        let out = match call {
            Call::Reflect(_) => self.reflect(agent_id),
            Call::Append(a) => {
                let out = self.append(agent_id, &a, kernel);
                // one `appended` event per new row; a refusal has no `ids`
                for id in out
                    .get("ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    kernel
                        .emit(
                            agent_id,
                            json!({"type": "appended", "table": "rows", "id": id}),
                        )
                        .await;
                }
                out
            }
            Call::Read(a) => self.read(agent_id, &a, kernel),
            Call::Search(a) => self.search(agent_id, &a, kernel),
            Call::Prune(a) => self.prune(agent_id, &a, kernel),
            Call::Inject(a) => self.inject(agent_id, &a),
            Call::Dump(_) => self.dump(agent_id),
            // the bytes verbs ride the binary path only (`handle_binary`)
            Call::PutBytes(_) => refusal(
                BAD_ARG,
                "ram_store: put_bytes carries its bytes as a binary body, at `data`",
            ),
            Call::GetBytes(_) => refusal(
                BAD_ARG,
                "ram_store: get_bytes carries its bytes as a binary body, at `data`",
            ),
        };
        Ok(Some(out))
    }

    /// `put_bytes` takes the raw bytes as the body at `data`, `get_bytes` answers
    /// them as the reply body there; every other verb is `handle`'s.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        match call {
            verbs::Call::PutBytes(a) => {
                let out = self.put_bytes(agent_id, &a, body.as_ref(), kernel);
                Ok((Some(out), None))
            }
            verbs::Call::GetBytes(a) => {
                let (out, blob) = self.get_bytes(agent_id, &a, kernel);
                Ok((Some(out), blob))
            }
            other => Ok((self.handle(agent_id, other, kernel).await?, None)),
        }
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
