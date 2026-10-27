//! sqlite_store — the DURABLE memory store agent (`sqlite_store.tools`).
//!
//! A 1:1 mirror of the lib's `ram_store` (python canonical, rust port in
//! `fantastic-ram-store`): the SAME verbs, arguments, replies, refusal reasons and
//! reflect, over the SAME tables with the same keys, uniqueness and foreign-key
//! cascades — kept in ONE SQLite file per store agent instead of RAM maps. A caller
//! cannot tell the two apart, except that this one survives the process.
//!
//! ```text
//!   rows       PRIMARY KEY (owner, id)             the row dict, minus `refs` (JSON)
//!   ref_lists  PRIMARY KEY (owner, fold)           "this row carries a refs list"
//!              FK (owner, fold)   → rows ON DELETE CASCADE
//!   fold_refs  PRIMARY KEY (owner, fold, pos)      a fold's members, in order
//!              FK (owner, fold)   → ref_lists ON DELETE CASCADE
//!              FK (owner, member) → rows      ON DELETE CASCADE
//!   text_idx   FTS5 (trigram) over the lowercased text — the search index
//! ```
//!
//! OWNER = CALLER: every row is keyed by who wrote it, `<kernel guid>|<agent id>`
//! (`<kernel guid>|kernel` for the operator and the kernel's own bookkeeping); a
//! caller reads and writes its own rows. Who may call any verb — `inject` /
//! `dump` included — is the kernel's call gate alone.
//!
//! `ref_lists` exists because ram_store keeps a fold's refs list even after every
//! member was pruned (`refs: []`); a bare member table could not say "empty list"
//! apart from "no list".
//!
//! SEARCH keeps ram_store's semantics exactly — a case-insensitive SUBSTRING, then
//! line by line — with the trigram index doing the candidate filter (`GLOB`), so a
//! query of any length gives the same answer and one of three chars or more is
//! answered by the index.
//!
//! EVERY TABLE ACCESS GOES THROUGH ONE LOCK per store. Tables raced under
//! concurrent agents + the flusher crash the kernel;
//! here a connection is never shared unlocked, and every verb is one transaction.
//!
//! The file: `path` on the record, REQUIRED (no default), relative to the directory
//! the kernel runs in, never absolute and never climbing out (`..`).

#![deny(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_repr_value, py_type_name, py_splitlines, py_str, py_strip, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    missing, refusal, refusal_hint, with, BAD_ARG, EXISTS, NOT_FOUND, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "sqlite_store.tools";

/// The short orientation readme.
pub const README: &str = include_str!("readme.md");

const OPERATOR: &str = "kernel";

type Row = Map<String, Value>;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS rows (
  owner TEXT NOT NULL, id INTEGER NOT NULL, doc TEXT NOT NULL,
  PRIMARY KEY (owner, id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ref_lists (
  owner TEXT NOT NULL, fold INTEGER NOT NULL,
  PRIMARY KEY (owner, fold),
  FOREIGN KEY (owner, fold) REFERENCES rows (owner, id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS fold_refs (
  owner TEXT NOT NULL, fold INTEGER NOT NULL, pos INTEGER NOT NULL, member INTEGER NOT NULL,
  PRIMARY KEY (owner, fold, pos),
  FOREIGN KEY (owner, fold) REFERENCES ref_lists (owner, fold) ON DELETE CASCADE,
  FOREIGN KEY (owner, member) REFERENCES rows (owner, id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS fold_refs_member ON fold_refs (owner, member);
-- THE KERNEL'S LOOKUPS, indexed (2026-10-09, the input-lag bug): a named call
-- is looked up by `name`, its settle by `parent`, on EVERY named send — a scan
-- of the whole history (json parsed per row) was ~160 ms at 66k rows, under the
-- store's one lock, and every page call queued behind it. `read` writes these
-- paths as literals and names the index (`INDEXED BY`): without statistics the
-- planner preferred the primary key's owner range — the whole history.
CREATE INDEX IF NOT EXISTS rows_by_name ON rows (owner, json_extract(doc, '$."name"'));
CREATE INDEX IF NOT EXISTS rows_by_parent ON rows (owner, json_extract(doc, '$."parent"'));
CREATE VIRTUAL TABLE IF NOT EXISTS text_idx USING fts5 (
  owner UNINDEXED, id UNINDEXED, text, tokenize = 'trigram case_sensitive 1'
);
CREATE TRIGGER IF NOT EXISTS rows_text_gone AFTER DELETE ON rows BEGIN
  DELETE FROM text_idx WHERE owner = old.owner AND id = old.id;
END;
"#;

/// ram_store `_as_id`: a positive int, or the decimal digits a caller wrote.
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

/// ram_store `_text_of`: the content as text, a title prefixed on its own line.
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

/// A GLOB pattern matching `q` as a substring (metacharacters bracketed).
fn glob_substring(q: &str) -> String {
    let mut out = String::from("*");
    for c in q.chars() {
        match c {
            '*' | '?' | '[' | ']' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out.push('*');
    out
}

fn db_err(e: rusqlite::Error) -> Value {
    refusal(UNHANDLED, format!("sqlite_store: {e}"))
}

fn doc(text: &str) -> Row {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn to_text(v: &Row) -> String {
    serde_json::to_string(&Value::Object(v.clone())).expect("a row serialises")
}

// ─── the tables, over one connection (always under the store's lock) ─────────

struct Db<'a>(&'a Connection);

impl Db<'_> {
    fn has(&self, owner: &str, rid: i64) -> rusqlite::Result<bool> {
        self.0
            .query_row("SELECT 1 FROM rows WHERE owner = ?1 AND id = ?2", params![owner, rid], |_| Ok(()))
            .optional()
            .map(|o| o.is_some())
    }

    /// The owner's ids, ascending (ram_store keeps `order` sorted by id).
    fn ids(&self, owner: &str) -> rusqlite::Result<Vec<i64>> {
        let mut st = self.0.prepare_cached("SELECT id FROM rows WHERE owner = ?1 ORDER BY id")?;
        let out = st.query_map(params![owner], |r| r.get::<_, i64>(0))?.collect();
        out
    }

    fn row(&self, owner: &str, rid: i64) -> rusqlite::Result<Option<Row>> {
        self.0
            .query_row("SELECT doc FROM rows WHERE owner = ?1 AND id = ?2", params![owner, rid], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map(|o| o.map(|t| doc(&t)))
    }

    /// Every row of one owner, by id.
    fn all_rows(&self, owner: &str) -> rusqlite::Result<HashMap<i64, Row>> {
        let mut st = self.0.prepare_cached("SELECT id, doc FROM rows WHERE owner = ?1")?;
        let out = st
            .query_map(params![owner], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .map(|x| x.map(|(i, t)| (i, doc(&t))))
            .collect();
        out
    }

    /// The ids of one owner strictly between `lo` and `hi` (either open), ascending.
    fn ids_between(&self, owner: &str, lo: Option<i64>, hi: Option<i64>) -> rusqlite::Result<Vec<i64>> {
        let mut st = self.0.prepare_cached("SELECT id FROM rows WHERE owner = ?1 AND id > ?2 AND id < ?3 ORDER BY id")?;
        let out = st.query_map(params![owner, lo.unwrap_or(i64::MIN), hi.unwrap_or(i64::MAX)], |r| r.get::<_, i64>(0))?.collect();
        out
    }

    /// The rows of one owner strictly between `lo` and `hi` (either open),
    /// narrowed in SQL to those whose field `k` equals `v` for every `(k, v)` in
    /// `eq` (a string or an integer) and whose `kind` is one of `kinds`. A
    /// SUPERSET of an exact `match`/`kinds` filter (the caller still applies
    /// that). The json paths are LITERALS — `name` and `parent` then hit their
    /// expression indexes — so a key is pushed down only when it is a plain
    /// label (`[A-Za-z0-9_]`); any other is left to the exact filter.
    fn rows_between(
        &self,
        owner: &str,
        lo: Option<i64>,
        hi: Option<i64>,
        eq: &[(String, Value)],
        kinds: Option<&[String]>,
    ) -> rusqlite::Result<HashMap<i64, Row>> {
        let pushed = |k: &str| eq.iter().any(|(x, v)| x == k && (v.is_string() || v.as_i64().is_some()));
        let index = if pushed("name") {
            " INDEXED BY rows_by_name"
        } else if pushed("parent") {
            " INDEXED BY rows_by_parent"
        } else {
            ""
        };
        let mut sql = format!("SELECT id, doc FROM rows{index} WHERE owner = ?");
        let mut args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if let Some(lo) = lo {
            sql.push_str(" AND id > ?");
            args.push(lo.into());
        }
        if let Some(hi) = hi {
            sql.push_str(" AND id < ?");
            args.push(hi.into());
        }
        for (k, v) in eq {
            if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let bound: rusqlite::types::Value = match v {
                Value::String(t) => t.clone().into(),
                Value::Number(n) if n.as_i64().is_some() => n.as_i64().expect("checked").into(),
                _ => continue,
            };
            sql.push_str(&format!(" AND json_extract(doc, '$.\"{k}\"') = ?"));
            args.push(bound);
        }
        if let Some(ks) = kinds {
            sql.push_str(" AND json_extract(doc, '$.kind') IN (");
            sql.push_str(&vec!["?"; ks.len()].join(","));
            sql.push(')');
            args.extend(ks.iter().map(|k| rusqlite::types::Value::from(k.clone())));
        }
        let mut st = self.0.prepare_cached(&sql)?;
        let out = st
            .query_map(rusqlite::params_from_iter(args), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .map(|x| x.map(|(i, t)| (i, doc(&t))))
            .collect();
        out
    }

    /// A row's refs list, if it carries one.
    fn refs(&self, owner: &str, fold: i64) -> rusqlite::Result<Option<Vec<i64>>> {
        let listed = self
            .0
            .query_row("SELECT 1 FROM ref_lists WHERE owner = ?1 AND fold = ?2", params![owner, fold], |_| Ok(()))
            .optional()?
            .is_some();
        if !listed {
            return Ok(None);
        }
        let mut st = self
            .0
            .prepare_cached("SELECT member FROM fold_refs WHERE owner = ?1 AND fold = ?2 ORDER BY pos")?;
        let ms = st.query_map(params![owner, fold], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(ms))
    }

    /// ram_store `get`: the row, `refs` appended when it carries a list.
    fn get(&self, owner: &str, rid: i64) -> rusqlite::Result<Option<Row>> {
        let Some(mut out) = self.row(owner, rid)? else {
            return Ok(None);
        };
        if let Some(refs) = self.refs(owner, rid)? {
            out.insert("refs".to_string(), Value::Array(refs.into_iter().map(Value::from).collect()));
        }
        Ok(Some(out))
    }

    fn set_refs(&self, owner: &str, fold: i64, members: &[i64]) -> rusqlite::Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO ref_lists (owner, fold) VALUES (?1, ?2)",
            params![owner, fold],
        )?;
        self.0
            .execute("DELETE FROM fold_refs WHERE owner = ?1 AND fold = ?2", params![owner, fold])?;
        for (pos, m) in members.iter().enumerate() {
            self.0.execute(
                "INSERT INTO fold_refs (owner, fold, pos, member) VALUES (?1, ?2, ?3, ?4)",
                params![owner, fold, pos as i64, m],
            )?;
        }
        Ok(())
    }

    /// ram_store `insert`: the caller validated every row and every ref.
    fn insert(&self, owner: &str, rows: &[Row]) -> rusqlite::Result<()> {
        for row in rows {
            let Some(rid) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let stored: Row = row
                .iter()
                .filter(|(k, _)| k.as_str() != "refs")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.0.execute(
                "INSERT INTO rows (owner, id, doc) VALUES (?1, ?2, ?3)",
                params![owner, rid, to_text(&stored)],
            )?;
            self.0.execute(
                "INSERT INTO text_idx (owner, id, text) VALUES (?1, ?2, ?3)",
                params![owner, rid, text_of(row).to_lowercase()],
            )?;
        }
        // Refs after every row of the batch is in: a fold may name a member that
        // arrives later in the same batch.
        for row in rows {
            let Some(rid) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            if let Some(refs) = row.get("refs").and_then(Value::as_array) {
                let members: Vec<i64> = refs.iter().filter_map(as_id).collect();
                self.set_refs(owner, rid, &members)?;
            }
        }
        Ok(())
    }

    /// ram_store `delete`: DELETE the rows; the FKs cascade to fold refs.
    /// Returns the rows removed.
    fn delete(&self, owner: &str, doomed: &HashSet<i64>) -> rusqlite::Result<usize> {
        let mut rows = 0usize;
        for rid in doomed {
            rows += self
                .0
                .execute("DELETE FROM rows WHERE owner = ?1 AND id = ?2", params![owner, rid])?;
        }
        Ok(rows)
    }

    fn dump(&self) -> rusqlite::Result<Value> {
        let mut st = self.0.prepare("SELECT owner, doc FROM rows ORDER BY owner, id")?;
        let rows: Vec<Value> = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|x| {
                x.map(|(owner, t)| {
                    let mut item = Map::new();
                    item.insert("owner".to_string(), Value::from(owner));
                    item.insert("row".to_string(), Value::Object(doc(&t)));
                    Value::Object(item)
                })
            })
            .collect::<rusqlite::Result<_>>()?;
        let mut st = self.0.prepare("SELECT owner, fold FROM ref_lists ORDER BY owner, fold")?;
        let lists: Vec<(String, i64)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut fold_refs = Vec::new();
        for (owner, fold) in lists {
            let members = self.refs(&owner, fold)?.unwrap_or_default();
            fold_refs.push(json!({"owner": owner, "fold": fold, "members": members}));
        }
        let mut out = Map::new();
        out.insert("rows".to_string(), Value::Array(rows));
        out.insert("fold_refs".to_string(), Value::Array(fold_refs));
        Ok(Value::Object(out))
    }
}

fn bad_tables(msg: &str) -> Value {
    let mut r = refusal(BAD_ARG, msg);
    with(&mut r, "arg", json!("tables"));
    r
}

/// A validated `dump`, ready to write — ram_store `load_tables`, every key and FK
/// checked in the same order with the same refusals.
struct Staged {
    rows: Vec<(String, Vec<Row>)>,
    fold_refs: Vec<(String, i64, Vec<i64>)>,
}

fn stage_tables(tables: &Value) -> Result<Staged, Value> {
    let Some(tables) = tables.as_object() else {
        return Err(bad_tables("inject: `tables` must be a dump dict"));
    };
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
    let mut have: HashSet<(String, i64)> = HashSet::new();
    for (owner, rs) in &by_owner {
        let mut seen: HashSet<i64> = HashSet::new();
        for r in rs {
            let id = r.get("id").and_then(Value::as_i64).unwrap_or(0);
            if !seen.insert(id) {
                let mut e = refusal(EXISTS, format!("inject: duplicate row ({owner}, {id})"));
                with(&mut e, "id", json!(id));
                return Err(e);
            }
            have.insert((owner.clone(), id));
        }
    }
    let has = |owner: &str, id: i64| have.contains(&(owner.to_string(), id));
    let empty = Vec::new();
    let mut fold_refs = Vec::new();
    for fr in tables.get("fold_refs").and_then(Value::as_array).unwrap_or(&empty) {
        let owner = fr.get("owner").and_then(Value::as_str).unwrap_or("").to_string();
        let fold = fr.get("fold").and_then(as_id);
        let members: Vec<Value> = fr.get("members").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(fold) = fold.filter(|f| has(&owner, *f)) else {
            return Err(bad_tables(&format!(
                "inject: fold_refs names no row {}",
                fold.map_or_else(|| "None".to_string(), |f| f.to_string())
            )));
        };
        if !members.iter().all(|m| has(&owner, as_id(m).unwrap_or(-1))) {
            return Err(bad_tables(&format!("inject: fold {fold} refs a missing row")));
        }
        fold_refs.push((owner, fold, members.iter().filter_map(as_id).collect()));
    }
    Ok(Staged { rows: by_owner, fold_refs })
}

// ─── who is calling (ram_store, rule for rule) ───────────────────────────────

fn guid(kernel: &Arc<Kernel>) -> Option<String> {
    kernel.root().and_then(|r| {
        r.meta
            .read()
            .ok()
            .and_then(|m| m.get("guid").and_then(Value::as_str).map(str::to_string))
            .filter(|g| !g.is_empty())
    })
}

fn record(kernel: &Arc<Kernel>, id: &AgentId) -> Map<String, Value> {
    kernel
        .agents
        .get(id)
        .map(|a| a.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default()
}

/// (key, display) for whoever is calling — the operator (and the kernel's own
/// bookkeeping) as `kernel` — or a refusal. A caller is always local: a call a
/// bridge leg serves is the leg's.
fn caller(kernel: &Arc<Kernel>) -> Result<(String, String), Value> {
    let Some(g) = guid(kernel) else {
        return Err(refusal_hint(
            NOT_FOUND,
            "sqlite_store: this kernel has no guid yet — the store keys every row by it",
            "boot the kernel root first (it mints the guid)",
        ));
    };
    match fantastic_kernel::current_origin() {
        None => Ok((format!("{g}|{OPERATOR}"), OPERATOR.to_string())),
        Some(origin) => Ok((format!("{g}|{}", origin.0), origin.0)),
    }
}

// ─── folds ───────────────────────────────────────────────────────────────────

fn fold_closure(db: &Db<'_>, owner: &str, fold: i64) -> rusqlite::Result<HashSet<i64>> {
    let mut out: HashSet<i64> = HashSet::new();
    let mut stack = vec![fold];
    while let Some(x) = stack.pop() {
        if !out.insert(x) {
            continue;
        }
        if let Some(refs) = db.refs(owner, x)? {
            stack.extend(refs);
        }
    }
    Ok(out)
}

/// ram_store's verb docs, word for word — the one on `boot` names the file.
pub const VERB_DOCS: [(&str, &str); 8] = [
    ("reflect", "Identity + posture: how many owners/rows, which optional verbs exist. No args."),
    ("boot", "Opens (creating if absent) the SQLite file named by `path`; the tables survive the process."),
    ("append", "args: rows:list[dict] (req). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}."),
    ("read", "args: after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}."),
    ("search", "args: query:str (req — case-insensitive substring), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}."),
    ("prune", "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). Returns {pruned}."),
    ("inject", "args: tables:dict (req — a `dump`). Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}."),
    ("dump", "No args. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}."),
];

/// python `x in container` for the JSON value a payload carried. `Err` is the
/// TypeError python raises (`'in <string>' requires string as left operand`,
/// `argument of type 'int' is not iterable`, `unhashable type: 'list'`) — the
/// same text as ram_store's.
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
            Value::Array(_) | Value::Object(_) => Err(type_error(format!("unhashable type: '{}'", py_type_name(x)))),
            _ => Ok(false),
        },
        other => Err(type_error(format!("argument of type '{}' is not iterable", py_type_name(other)))),
    }
}

/// Why a record's `path` cannot name the file: it is required, relative, and
/// never climbs out of the kernel's workdir.
pub fn check_path(rec: &Map<String, Value>) -> Result<&Path, String> {
    let p = rec
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("sqlite_store needs `path`: the database file, relative to the project dir (no default)")?;
    let path = Path::new(p);
    if path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("sqlite_store: `path` must stay inside the project dir, got {p:?}"));
    }
    Ok(path)
}

/// Where the file is: `path` under the kernel's WORKDIR (the project dir). A
/// kernel without one has nowhere durable to put it — refused.
pub fn resolve_path(rec: &Map<String, Value>, kernel: &Kernel) -> Result<PathBuf, String> {
    let path = check_path(rec)?;
    let work = kernel
        .storage
        .workdir()
        .ok_or("sqlite_store needs a disk-backed kernel: an in-memory kernel has no project dir to hold the file")?;
    // Clamp like python `fs.resolve`: the deepest part of the path that exists is
    // followed through its symlinks and must still lie under the real workdir —
    // a `stores -> /elsewhere` link must not carry the file out of the project.
    let escapes = || format!("sqlite_store: `path` {:?} escapes the project dir", path.display().to_string());
    let base = work.canonicalize().map_err(|e| format!("sqlite_store: {}: {e}", work.display()))?;
    let joined = base.join(path);
    let mut probe = joined.as_path();
    while probe.symlink_metadata().is_err() {
        probe = probe.parent().ok_or_else(escapes)?;
    }
    let real = probe.canonicalize().map_err(|_| escapes())?;
    if !real.starts_with(&base) {
        return Err(escapes());
    }
    let rest = joined.strip_prefix(probe).map_err(|_| escapes())?;
    Ok(if rest.as_os_str().is_empty() { real } else { real.join(rest) })
}

/// The `sqlite_store.tools` bundle. Its connections are the KERNEL'S state
/// ([`Conns`]), not the bundle's: the host closes them after the kernel's
/// last flush ([`close_all`]).
#[derive(Default)]
pub struct SqliteStoreBundle;

/// One connection per store agent, each behind its own lock, kept on the kernel.
#[derive(Default)]
struct Conns(Mutex<HashMap<String, Arc<Mutex<Connection>>>>);

const CONNS: &str = "sqlite_store.conns";

fn conns(kernel: &Kernel) -> Arc<Conns> {
    kernel.extension::<Conns>(CONNS)
}

/// Close every store `kernel` opened: checkpoint its WAL into the file
/// (`TRUNCATE`) and drop the connection, so a stopped kernel leaves only the
/// `.db`. For AFTER `Kernel::shutdown` — its teardown walk ([`Bundle::on_shutdown`])
/// runs before the final flush, which still writes rows. Returns one line per
/// store that could not checkpoint.
pub fn close_all(kernel: &Kernel) -> Vec<String> {
    let all: Vec<(String, Arc<Mutex<Connection>>)> = conns(kernel).0.lock().expect("sqlite_store poisoned").drain().collect();
    let mut errs = Vec::new();
    for (id, c) in all {
        let c = c.lock().expect("sqlite_store connection poisoned");
        if let Err(e) = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
            errs.push(format!("sqlite_store {id}: checkpoint: {e}"));
        }
    }
    errs
}

impl SqliteStoreBundle {
    /// The store's connection, opened (and the schema ensured) on first use.
    fn conn(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Result<Arc<Mutex<Connection>>, Value> {
        if let Some(c) = conns(kernel).0.lock().expect("sqlite_store poisoned").get(&id.0) {
            return Ok(Arc::clone(c));
        }
        let path = resolve_path(&record(kernel, id), kernel).map_err(|e| {
            let mut r = refusal(BAD_ARG, e);
            with(&mut r, "arg", json!("path"));
            r
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| refusal(UNHANDLED, format!("sqlite_store: {}: {e}", dir.display())))?;
        }
        let c = Connection::open(&path).map_err(db_err)?;
        c.pragma_update(None, "journal_mode", "WAL").map_err(db_err)?;
        c.pragma_update(None, "synchronous", "NORMAL").map_err(db_err)?;
        // A deleted row's bytes are zeroed in place, not left in a free page.
        c.pragma_update(None, "secure_delete", "ON").map_err(db_err)?;
        c.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
        c.execute_batch(SCHEMA).map_err(db_err)?;
        let c = Arc::new(Mutex::new(c));
        conns(kernel)
            .0
            .lock()
            .expect("sqlite_store poisoned")
            .entry(id.0.clone())
            .or_insert_with(|| Arc::clone(&c));
        Ok(c)
    }

    /// Run `f` in ONE transaction under the store's lock; a refusal rolls back.
    fn tx(
        &self,
        id: &AgentId,
        kernel: &Arc<Kernel>,
        f: impl FnOnce(&Db<'_>) -> Result<Value, Value>,
    ) -> Value {
        let c = match self.conn(id, kernel) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let mut c = c.lock().expect("sqlite_store connection poisoned");
        let tx = match c.transaction() {
            Ok(t) => t,
            Err(e) => return db_err(e),
        };
        let out = f(&Db(&tx));
        match out {
            Ok(v) => match tx.commit() {
                Ok(()) => v,
                Err(e) => db_err(e),
            },
            Err(refused) => {
                let _ = tx.rollback();
                refused
            }
        }
    }

    fn reflect(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        let rec = record(kernel, id);
        let counts = self.tx(id, kernel, |db| {
            let q = |sql: &str| db.0.query_row(sql, [], |r| r.get::<_, i64>(0)).map_err(db_err);
            Ok(json!([
                q("SELECT COUNT(DISTINCT owner) FROM rows")?,
                q("SELECT COUNT(*) FROM rows")?,
            ]))
        });
        let n = |i: usize| counts.get(i).cloned().unwrap_or(Value::Null);
        let mut out = Map::new();
        out.insert("id".to_string(), Value::from(id.0.clone()));
        out.insert(
            "sentence".to_string(),
            Value::from(
                "Memory store in SQLite — rows per owner (the caller), folds, search; \
                 durable: one database file, the tables survive the process.",
            ),
        );
        out.insert("durable".to_string(), Value::Bool(true));
        out.insert(
            "path".to_string(),
            rec.get("path").cloned().unwrap_or(Value::Null),
        );
        out.insert("owners".to_string(), n(0));
        out.insert("rows".to_string(), n(1));
        if counts.get("error").is_some() {
            out.insert("error".to_string(), counts.get("error").cloned().unwrap_or(Value::Null));
        }
        out.insert("optional_verbs".to_string(), json!(["search"]));
        Value::Object(out)
    }

    fn append(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let rows = match payload.get("rows").and_then(Value::as_array) {
            Some(r) if !r.is_empty() && r.iter().all(Value::is_object) => r.clone(),
            _ => {
                let mut r = refusal(BAD_ARG, "append: `rows` must be a non-empty list of rows");
                with(&mut r, "arg", json!("rows"));
                return r;
            }
        };
        self.tx(id, kernel, |db| {
            let mut staged: Vec<Row> = Vec::new();
            let mut batch: HashSet<i64> = HashSet::new();
            for r in &rows {
                let r = r.as_object().expect("checked");
                let rid = match r.get("id") {
                    None | Some(Value::Null) => kernel.mint_id(),
                    Some(v) => match (v, as_id(v)) {
                        (Value::Number(_), Some(n)) => n,
                        _ => {
                            let mut e = refusal(BAD_ARG, format!("append: a row id is a kernel int, got {}", py_repr_value(v)));
                            with(&mut e, "arg", json!("rows"));
                            return Err(e);
                        }
                    },
                };
                if db.has(&owner, rid).map_err(db_err)? || batch.contains(&rid) {
                    let mut e = refusal(EXISTS, format!("append: row {rid} already exists for this owner"));
                    with(&mut e, "id", json!(rid));
                    return Err(e);
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
                let Some(list) = refs.as_array().filter(|a| a.iter().all(|m| as_id(m).is_some())) else {
                    let mut e = refusal(BAD_ARG, format!("append: row {rid} refs must be row ids"));
                    with(&mut e, "arg", json!("rows"));
                    return Err(e);
                };
                // python `t.has(owner, m) or m in batch` on the RAW ref: the tables
                // are keyed by int, so a digit STRING passes `as_id` and is still
                // never found.
                let mut missing_refs: Vec<Value> = Vec::new();
                for m in list {
                    let held = match (m.is_number(), as_id(m)) {
                        (true, Some(i)) => db.has(&owner, i).map_err(db_err)? || batch.contains(&i),
                        _ => false,
                    };
                    if !held {
                        missing_refs.push(m.clone());
                    }
                }
                if !missing_refs.is_empty() {
                    let mut e = refusal(
                        NOT_FOUND,
                        format!("append: row {rid} refs rows that do not exist for this owner: {}", py_repr_value(&Value::Array(missing_refs))),
                    );
                    with(&mut e, "arg", json!("rows"));
                    return Err(e);
                }
            }
            let ids: Vec<Value> = staged.iter().map(|r| r.get("id").cloned().unwrap_or(Value::Null)).collect();
            // The file holds JSON: a row whose text does not parse back (the one
            // non-JSON number the kernel keeps, `Infinity`) is refused, never
            // rewritten — all or none, like python's `_encode`.
            for r in &staged {
                if let Err(e) = serde_json::from_str::<Value>(&to_text(r)) {
                    let mut e = refusal(
                        BAD_ARG,
                        format!("append: row {} is not JSON: {e}", r.get("id").map_or("None".to_string(), Value::to_string)),
                    );
                    with(&mut e, "arg", json!("rows"));
                    return Err(e);
                }
            }
            let n = staged.len();
            db.insert(&owner, &staged).map_err(db_err)?;
            Ok(json!({"ids": ids, "appended": n}))
        })
    }

    fn read(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        self.tx(id, kernel, |db| {
            let (owner, _) = caller(kernel)?;
            let flk = payload.get("from_last_kind").and_then(Value::as_str).filter(|s| !s.is_empty());
            let after = payload.get("after").and_then(as_id);
            let before = payload.get("before").and_then(as_id);
            let desc = payload.get("order").and_then(Value::as_str) == Some("desc");
            // THE ID BOUNDS IN THE SQL, before any row is read: `after`/`before`
            // only look at ids, so a cursor read ("what came after my newest row")
            // costs the rows after the cursor, not the owner's whole history. Not
            // with `from_last_kind`, which must see every row to find the newest
            // of a kind. The same bounds are applied again below — idempotent.
            let (lo, hi) = if flk.is_some() {
                (None, None)
            } else {
                match (desc, after) {
                    (true, Some(a)) => (None, Some(before.map_or(a, |b| b.min(a)))),
                    (false, Some(a)) => (Some(a), before),
                    _ => (None, before),
                }
            };
            // Rows are decoded only when a filter reads them, and only the ids
            // still in play.
            let needs_rows = flk.is_some()
                || payload.get("kinds").and_then(Value::as_array).is_some_and(|k| !k.is_empty())
                || payload.get("match").and_then(Value::as_object).is_some_and(|m| !m.is_empty());
            let (mut ids, rows) = if !needs_rows {
                (db.ids_between(&owner, lo, hi).map_err(db_err)?, HashMap::new())
            } else if flk.is_some() {
                let rows = db.all_rows(&owner).map_err(db_err)?;
                let mut ids: Vec<i64> = rows.keys().copied().collect();
                ids.sort_unstable();
                (ids, rows)
            } else {
                // SQLite narrows by the `match` fields (strings, integers) and the
                // `kinds` first — a superset of the exact filter below, which still
                // decides — so the kernel's per-send lookups (`{kinds:[call],
                // match:{name}}`, `{kinds:[settle], match:{parent}}`) are index hits.
                let eq: Vec<(String, Value)> = payload
                    .get("match")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let kinds: Option<Vec<String>> = payload.get("kinds").and_then(Value::as_array).and_then(|k| {
                    let s: Vec<String> = k.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                    (s.len() == k.len() && !s.is_empty()).then_some(s)
                });
                let rows = db.rows_between(&owner, lo, hi, &eq, kinds.as_deref()).map_err(db_err)?;
                let mut ids: Vec<i64> = rows.keys().copied().collect();
                ids.sort_unstable();
                (ids, rows)
            };
            let row_of = |i: &i64| rows.get(i).cloned().unwrap_or_default();
            if let Some(flk) = flk {
                let last = ids
                    .iter()
                    .rev()
                    .find(|i| row_of(i).get("kind").and_then(Value::as_str) == Some(flk))
                    .copied();
                if let Some(last) = last {
                    ids.retain(|i| *i >= last);
                }
            }
            if let Some(kinds) = payload.get("kinds").and_then(Value::as_array).filter(|k| !k.is_empty()) {
                ids.retain(|i| kinds.contains(&row_of(i).get("kind").cloned().unwrap_or(Value::Null)));
            }
            if let Some(m) = payload.get("match").and_then(Value::as_object).filter(|m| !m.is_empty()) {
                ids.retain(|i| {
                    let r = row_of(i);
                    m.iter().all(|(k, v)| r.get(k).unwrap_or(&Value::Null) == v)
                });
            }
            if let Some(want) = payload.get("ids").and_then(Value::as_array) {
                let wanted: HashSet<Option<i64>> = want.iter().map(as_id).collect();
                ids.retain(|i| wanted.contains(&Some(*i)));
            }
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
            // `ids[:limit] if limit else ids` — a `0` is falsy: no limit.
            let limit = payload.get("limit").and_then(as_id).filter(|l| *l != 0);
            let page: Vec<i64> = match limit {
                Some(l) => ids.iter().take(usize::try_from(l).unwrap_or(usize::MAX)).copied().collect(),
                None => ids.clone(),
            };
            let next = match (limit, page.last()) {
                (Some(_), Some(last)) if total > page.len() => Value::from(*last),
                _ => Value::Null,
            };
            let mut out = Vec::with_capacity(page.len());
            for i in &page {
                if let Some(r) = db.get(&owner, *i).map_err(db_err)? {
                    out.push(Value::Object(r));
                }
            }
            Ok(json!({"rows": out, "next": next, "total": total}))
        })
    }

    fn search(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let q = py_strip(&payload.get("query").filter(|v| py_truthy(v)).map(py_str).unwrap_or_default()).to_lowercase();
        if q.is_empty() {
            return missing("search", "query");
        }
        self.tx(id, kernel, |db| {
            let (owner, _) = caller(kernel)?;
            let mut ids = db.ids(&owner).map_err(db_err)?;
            if let Some(want) = payload.get("ids").and_then(Value::as_array).filter(|w| !w.is_empty()) {
                let mut scope: HashSet<i64> = HashSet::new();
                for x in want {
                    if let Some(rid) = as_id(x) {
                        scope.extend(fold_closure(db, &owner, rid).map_err(db_err)?);
                    }
                }
                ids.retain(|i| scope.contains(i));
            }
            // THE INDEX picks the candidate rows (trigram GLOB = substring).
            let hits: HashSet<i64> = {
                let mut st = db
                    .0
                    .prepare_cached("SELECT id FROM text_idx WHERE owner = ?1 AND text GLOB ?2")
                    .map_err(db_err)?;
                let found = st
                    .query_map(params![owner, glob_substring(&q)], |r| r.get::<_, i64>(0))
                    .map_err(db_err)?
                    .collect::<rusqlite::Result<HashSet<i64>>>()
                    .map_err(db_err)?;
                found
            };
            // `_as_id(limit) or 100` / `or 200`: a `0` falls back too.
            let limit = usize::try_from(payload.get("limit").and_then(as_id).filter(|l| *l != 0).unwrap_or(100)).unwrap_or(usize::MAX);
            let width = usize::try_from(payload.get("width").and_then(as_id).filter(|w| *w != 0).unwrap_or(200)).unwrap_or(usize::MAX);
            let mut matches: Vec<Value> = Vec::new();
            let mut total = 0usize;
            for i in &ids {
                if !hits.contains(i) {
                    continue;
                }
                let Some(row) = db.row(&owner, *i).map_err(db_err)? else { continue };
                let text = text_of(&row);
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
            Ok(json!({"matches": matches, "total": total, "truncated": truncated}))
        })
    }

    /// `Err` is python's TypeError out of `kind in keep` — a raise.
    fn prune(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Value, BundleError> {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return Ok(r),
        };
        let before = payload.get("before_id").and_then(as_id);
        let want = payload.get("ids").and_then(Value::as_array);
        if before.is_none() && want.is_none() {
            return Ok(missing("prune", "before_id"));
        }
        let wanted: HashSet<Option<i64>> = want.map(|w| w.iter().map(as_id).collect()).unwrap_or_default();
        // `keep = payload.get("keep") or []`, then `row.get("kind") in keep` —
        // python's `in` on WHATEVER arrived: a list is membership, a string a
        // SUBSTRING test, a dict its keys, and anything else raises.
        let keep: Value = payload.get("keep").filter(|v| py_truthy(v)).cloned().unwrap_or_else(|| json!([]));
        let has_want = want.is_some();
        let mut raised: Option<BundleError> = None;
        let out = self.tx(id, kernel, |db| {
            let rows = db.all_rows(&owner).map_err(db_err)?;
            let mut doomed: HashSet<i64> = HashSet::new();
            for i in db.ids(&owner).map_err(db_err)? {
                let kind = rows.get(&i).and_then(|r| r.get("kind")).cloned().unwrap_or(Value::Null);
                match py_in(&kind, &keep) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        raised = Some(e);
                        return Err(refusal(UNHANDLED, "prune raised"));
                    }
                }
                if before.is_some_and(|b| i < b) || (has_want && wanted.contains(&Some(i))) {
                    doomed.insert(i);
                }
            }
            let n = db.delete(&owner, &doomed).map_err(db_err)?;
            Ok(json!({"pruned": n}))
        });
        // The WAL still holds the old frames: fold them into the file and
        // truncate it, so the pruned text is gone from disk.
        if out.get("pruned").and_then(Value::as_u64).is_some_and(|n| n > 0) {
            if let Ok(c) = self.conn(id, kernel) {
                let c = c.lock().expect("sqlite_store connection poisoned");
                let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
            }
        }
        match raised {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    fn inject(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let staged = match stage_tables(payload.get("tables").unwrap_or(&Value::Null)) {
            Ok(s) => s,
            Err(e) => return e,
        };
        self.tx(id, kernel, |db| {
            // The WHOLE store is replaced, in this one transaction.
            db.0
                .execute_batch(
                    "DELETE FROM fold_refs; DELETE FROM ref_lists; DELETE FROM rows; DELETE FROM text_idx;",
                )
                .map_err(db_err)?;
            let mut rows = 0usize;
            for (owner, rs) in &staged.rows {
                db.insert(owner, rs).map_err(db_err)?;
                rows += rs.len();
            }
            for (owner, fold, members) in &staged.fold_refs {
                db.set_refs(owner, *fold, members).map_err(db_err)?;
            }
            Ok(json!({"rows": rows}))
        })
    }

    fn dump(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        self.tx(id, kernel, |db| Ok(json!({"tables": db.dump().map_err(db_err)?})))
    }
}

#[async_trait]
impl Bundle for SqliteStoreBundle {
    /// HOW TO CALL it — served by the kernel's `docs` verb, never on a reflect.
    fn verb_docs(&self) -> Value {
        let mut verbs = Map::new();
        for (name, d) in VERB_DOCS {
            verbs.insert(name.to_string(), Value::from(d));
        }
        Value::Object(verbs)
    }

    /// The CATALOG name — python's entry-point name, not the handler module
    /// (the kind `reflect bundles=all` lists and a system prompt names).
    fn name(&self) -> &str {
        "sqlite_store"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// `path` — required, relative, inside.
    fn validate_meta(&self, meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
        let mut merged = record.clone();
        for (k, v) in meta {
            merged.insert(k.clone(), v.clone());
        }
        check_path(&merged).err()
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let out = match verb {
            "reflect" => self.reflect(agent_id, kernel),
            // Opens the file (the schema ensured): a bad `path` fails LOUD here,
            // at boot, not on the first turn.
            "boot" => match self.conn(agent_id, kernel) {
                Ok(_) => return Ok(None),
                Err(e) => e,
            },
            "append" => self.append(agent_id, payload, kernel),
            "read" => self.read(agent_id, payload, kernel),
            "search" => self.search(agent_id, payload, kernel),
            "prune" => self.prune(agent_id, payload, kernel)?,
            "inject" => self.inject(agent_id, payload, kernel),
            "dump" => self.dump(agent_id, kernel),
            _ => {
                let t = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = refusal(UNHANDLED, format!("sqlite_store: unknown type {}", py_repr_value(&t)));
                with(&mut r, "verb", t);
                r
            }
        };
        Ok(Some(out))
    }

    /// A deleted store agent DELETES ITS DATA — the file and its WAL/SHM
    /// siblings — as ram_store's tables die with the agent (owner decision,
    /// 2026-09-26). Only an explicit `delete_agent` reaches here: the stop's
    /// teardown walk calls [`Self::on_shutdown`], which keeps the file.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        conns(kernel).0.lock().expect("sqlite_store poisoned").remove(&agent_id.0);
        let path = resolve_path(&record(kernel, agent_id), kernel)?;
        for suffix in ["", "-wal", "-shm"] {
            let f = PathBuf::from(format!("{}{suffix}", path.display()));
            match std::fs::remove_file(&f) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("sqlite_store: {}: {e}", f.display()).into()),
            }
        }
        Ok(())
    }

    /// NOT a close before the final flush, and never a delete: a teardown walk
    /// must not lose the kernel's last rows. The host closes the file after the
    /// flush ([`close_all`]).
    async fn on_shutdown(&self, _agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
