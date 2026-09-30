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
//!   grants     PRIMARY KEY gid                     the grant dict (JSON)
//!              UNIQUE (owner, reader, scope, mode) an identical share is one grant
//!              FK (owner, anchor) → rows ON DELETE CASCADE
//!              INDEX owner, INDEX reader
//!   seq        the grant counter (never reuses a revoked id; `inject` resets it)
//!   text_idx   FTS5 (trigram) over the lowercased text — the search index
//! ```
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
//! EVERY TABLE ACCESS GOES THROUGH ONE LOCK per store. The swift ram_store port
//! raced its tables under concurrent agents + the flusher and crashed the kernel;
//! here a connection is never shared unlocked, and every verb is one transaction.
//!
//! The file: `path` on the record, REQUIRED (no default), relative to the directory
//! the kernel runs in, never absolute and never climbing out (`..`).

#![deny(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_repr, py_repr_value, py_split_whitespace, py_splitlines, py_str, py_strip, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    missing, refusal, refusal_hint, with, BAD_ARG, EXISTS, NOT_FOUND, UNAUTHORIZED, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "sqlite_store.tools";

/// The short orientation readme.
pub const README: &str = include_str!("readme.md");

const OPERATOR: &str = "kernel";
const MODES: [&str; 2] = ["frozen", "live"];
const SCOPE_KINDS: [&str; 4] = ["head_until", "range", "fold", "row"];
const REF_KINDS: [&str; 2] = ["fold", "clear"];

type Row = Map<String, Value>;

const SCHEMA: &str = "
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
CREATE TABLE IF NOT EXISTS grants (
  gid INTEGER PRIMARY KEY, owner TEXT NOT NULL, reader TEXT NOT NULL, scope_key TEXT NOT NULL,
  mode TEXT NOT NULL, anchor INTEGER NOT NULL, doc TEXT NOT NULL,
  UNIQUE (owner, reader, scope_key, mode),
  FOREIGN KEY (owner, anchor) REFERENCES rows (owner, id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS grants_owner ON grants (owner);
CREATE INDEX IF NOT EXISTS grants_reader ON grants (reader);
CREATE TABLE IF NOT EXISTS seq (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS text_idx USING fts5 (
  owner UNINDEXED, id UNINDEXED, text, tokenize = 'trigram case_sensitive 1'
);
CREATE TRIGGER IF NOT EXISTS rows_text_gone AFTER DELETE ON rows BEGIN
  DELETE FROM text_idx WHERE owner = old.owner AND id = old.id;
END;
";

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

/// ram_store `scope_key`: `json.dumps(scope, sort_keys=True)`.
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

    /// The rows of one owner whose ids are in `ids` — one range scan between the
    /// smallest and largest id asked for — narrowed in SQL to those whose
    /// string field `k` equals `v` for every `(k, v)` in `eq`, and whose `kind`
    /// is one of `kinds`. A SUPERSET of an exact `match`/`kinds` filter (the
    /// caller still applies that): it only skips decoding rows that cannot pass.
    fn rows_where(
        &self,
        owner: &str,
        ids: &[i64],
        eq: &[(String, String)],
        kinds: Option<&[String]>,
    ) -> rusqlite::Result<HashMap<i64, Row>> {
        let (Some(lo), Some(hi)) = (ids.iter().min(), ids.iter().max()) else {
            return Ok(HashMap::new());
        };
        let want: HashSet<i64> = ids.iter().copied().collect();
        let mut sql = String::from("SELECT id, doc FROM rows WHERE owner = ? AND id BETWEEN ? AND ?");
        let mut args: Vec<rusqlite::types::Value> =
            vec![owner.to_string().into(), (*lo).into(), (*hi).into()];
        for (k, v) in eq {
            // a JSON path naming the key as one quoted label
            sql.push_str(" AND json_extract(doc, ?) = ?");
            args.push(format!("$.\"{k}\"").into());
            args.push(v.clone().into());
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
            .filter(|x| x.as_ref().map_or(true, |(i, _)| want.contains(i)))
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

    fn grant(&self, gid: i64) -> rusqlite::Result<Option<Row>> {
        self.0
            .query_row("SELECT doc FROM grants WHERE gid = ?1", params![gid], |r| r.get::<_, String>(0))
            .optional()
            .map(|o| o.map(|t| doc(&t)))
    }

    fn grants_where(&self, field: &str, who: &str) -> rusqlite::Result<Vec<Row>> {
        let sql = format!("SELECT doc FROM grants WHERE {field} = ?1 ORDER BY gid");
        let mut st = self.0.prepare(&sql)?;
        let out = st.query_map(params![who], |r| r.get::<_, String>(0))?.map(|x| x.map(|t| doc(&t))).collect();
        out
    }

    fn seq(&self) -> rusqlite::Result<i64> {
        Ok(self
            .0
            .query_row("SELECT value FROM seq WHERE name = 'grant'", [], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0))
    }

    fn set_seq(&self, v: i64) -> rusqlite::Result<()> {
        self.0
            .execute("INSERT OR REPLACE INTO seq (name, value) VALUES ('grant', ?1)", params![v])?;
        Ok(())
    }

    /// ram_store `add_grant`: an identical (owner, reader, scope, mode) is the SAME
    /// grant; otherwise the next id of a counter that never goes back.
    fn add_grant(&self, mut g: Row) -> rusqlite::Result<(i64, bool)> {
        let owner = g.get("owner").map(py_str).unwrap_or_default();
        let reader = g.get("reader").map(py_str).unwrap_or_default();
        let sk = scope_key(g.get("scope").unwrap_or(&Value::Null));
        let mode = g.get("mode").map(py_str).unwrap_or_default();
        let existing = self
            .0
            .query_row(
                "SELECT gid FROM grants WHERE owner = ?1 AND reader = ?2 AND scope_key = ?3 AND mode = ?4",
                params![owner, reader, sk, mode],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(gid) = existing {
            return Ok((gid, false));
        }
        let gid = self.seq()? + 1;
        self.set_seq(gid)?;
        let anchor = g.get("anchor").and_then(Value::as_i64).unwrap_or(0);
        g.insert("grant".to_string(), Value::from(gid));
        self.0.execute(
            "INSERT INTO grants (gid, owner, reader, scope_key, mode, anchor, doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![gid, owner, reader, sk, mode, anchor, to_text(&g)],
        )?;
        Ok((gid, true))
    }

    fn drop_grant(&self, gid: i64) -> rusqlite::Result<bool> {
        Ok(self.0.execute("DELETE FROM grants WHERE gid = ?1", params![gid])? > 0)
    }

    /// ram_store `delete`: DELETE the rows; the FKs cascade to fold refs and
    /// grants. Returns (rows removed, grants removed).
    fn delete(&self, owner: &str, doomed: &HashSet<i64>) -> rusqlite::Result<(usize, usize)> {
        let mut rows = 0usize;
        let mut grants = 0usize;
        for rid in doomed {
            let g: i64 = self.0.query_row(
                "SELECT COUNT(*) FROM grants WHERE owner = ?1 AND anchor = ?2",
                params![owner, rid],
                |r| r.get(0),
            )?;
            let n = self
                .0
                .execute("DELETE FROM rows WHERE owner = ?1 AND id = ?2", params![owner, rid])?;
            if n > 0 {
                rows += n;
                grants += g as usize;
            }
        }
        Ok((rows, grants))
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
        let mut st = self.0.prepare("SELECT doc FROM grants ORDER BY gid")?;
        let grants: Vec<Value> = st
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|x| x.map(|t| Value::Object(doc(&t))))
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Map::new();
        out.insert("rows".to_string(), Value::Array(rows));
        out.insert("fold_refs".to_string(), Value::Array(fold_refs));
        out.insert("grants".to_string(), Value::Array(grants));
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
    grants: Vec<(i64, Row)>,
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
    let mut grants = Vec::new();
    for g in tables.get("grants").and_then(Value::as_array).unwrap_or(&empty) {
        let anchored = g.as_object().is_some_and(|m| {
            let owner = m.get("owner").and_then(Value::as_str).unwrap_or("");
            m.get("anchor").and_then(Value::as_i64).is_some_and(|a| has(owner, a))
        });
        if !anchored {
            return Err(bad_tables("inject: a grant's anchor names no row"));
        }
        let Some(gid) = g.get("grant").and_then(as_id) else {
            return Err(bad_tables("inject: a grant needs an int `grant`"));
        };
        let mut body: Row = g
            .as_object()
            .expect("checked")
            .iter()
            .filter(|(k, _)| k.as_str() != "grant")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        body.insert("grant".to_string(), Value::from(gid));
        grants.push((gid, body));
    }
    Ok(Staged { rows: by_owner, fold_refs, grants })
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

fn caller(kernel: &Arc<Kernel>, id: &AgentId) -> Result<(String, String), Value> {
    if let Some(remote) = fantastic_kernel::current_remote_caller() {
        return Ok((remote.clone(), remote));
    }
    let Some(g) = guid(kernel) else {
        return Err(refusal_hint(
            NOT_FOUND,
            "sqlite_store: this kernel has no guid yet — the store keys every row by it",
            "boot the kernel root first (it mints the guid)",
        ));
    };
    match fantastic_kernel::current_origin() {
        None => {
            if !record(kernel, id).get("operator").is_some_and(py_truthy) {
                let mut r = refusal_hint(
                    UNAUTHORIZED,
                    format!("{}: the operator is not served here", id.0),
                    format!("update_agent id={} operator=true to serve the operator (as 'kernel')", id.0),
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

fn owner_for(kernel: &Arc<Kernel>, id: &AgentId, payload: &Value, verb: &str) -> Result<(String, String, String), Value> {
    let (me, who) = caller(kernel, id)?;
    let Some(name) = payload.get("owner").filter(|v| !v.is_null()) else {
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
            format!("{verb}: {} may act only on its own rows, not {}", py_repr(&who), py_repr_value(name)),
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

// ─── scopes ──────────────────────────────────────────────────────────────────

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

fn scope_ids(db: &Db<'_>, g: &Row) -> rusqlite::Result<HashSet<i64>> {
    let owner = g.get("owner").map(py_str).unwrap_or_default();
    let scope = g.get("scope").cloned().unwrap_or(Value::Null);
    let live = g.get("mode").and_then(Value::as_str) == Some("live");
    let ids = db.ids(&owner)?;
    let all: HashSet<i64> = ids.iter().copied().collect();
    let mut base: HashSet<i64> = if let Some(r) = scope.get("row").and_then(Value::as_i64) {
        [r].into_iter().filter(|x| all.contains(x)).collect()
    } else if let Some(f) = scope.get("fold").and_then(Value::as_i64) {
        fold_closure(db, &owner, f)?.into_iter().filter(|x| all.contains(x)).collect()
    } else if let Some(r) = scope.get("range") {
        let lo = r.get("from").and_then(Value::as_i64).unwrap_or(0);
        let hi = r.get("to").and_then(Value::as_i64).unwrap_or(0);
        ids.iter().copied().filter(|i| *i >= lo && (live || *i <= hi)).collect()
    } else {
        let n = scope.get("head_until").and_then(Value::as_i64).unwrap_or(0);
        ids.iter().copied().filter(|i| live || *i <= n).collect()
    };
    if !live {
        if let Some(as_of) = g.get("as_of").and_then(Value::as_i64) {
            base.retain(|i| *i <= as_of);
        }
    }
    Ok(base)
}

fn bad_scope(code: &str, msg: String) -> Value {
    let mut r = refusal(code, msg);
    with(&mut r, "arg", json!("scope"));
    r
}

fn parse_scope(db: &Db<'_>, owner: &str, scope: &Value) -> Result<(Value, i64), Value> {
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
    let ids = db.ids(owner).map_err(db_err)?;
    if kind == "range" {
        let Some(r) = val.as_object() else {
            return Err(bad_scope(BAD_ARG, "share: range must be {from, to}".to_string()));
        };
        let (Some(lo), Some(hi)) = (r.get("from").and_then(as_id), r.get("to").and_then(as_id)) else {
            return Err(bad_scope(BAD_ARG, "share: range needs from <= to (row ids)".to_string()));
        };
        if lo > hi {
            return Err(bad_scope(BAD_ARG, "share: range needs from <= to (row ids)".to_string()));
        }
        let Some(first) = ids.iter().copied().find(|i| lo <= *i && *i <= hi) else {
            return Err(bad_scope(NOT_FOUND, format!("share: none of YOUR rows lie in {lo}..{hi}")));
        };
        return Ok((json!({"range": {"from": lo, "to": hi}}), first));
    }
    let Some(rid) = as_id(val) else {
        return Err(bad_scope(BAD_ARG, format!("share: {kind} must be a row id")));
    };
    if kind == "head_until" {
        return match ids.first() {
            Some(first) if *first <= rid => Ok((json!({"head_until": rid}), *first)),
            _ => Err(bad_scope(NOT_FOUND, format!("share: you have no rows up to {rid}"))),
        };
    }
    if !db.has(owner, rid).map_err(db_err)? {
        return Err(bad_scope(
            NOT_FOUND,
            format!("share: row {rid} is not one of YOUR rows — only rows you own can be shared"),
        ));
    }
    if kind == "fold" {
        let row = db.row(owner, rid).map_err(db_err)?;
        let k = row.as_ref().and_then(|r| r.get("kind")).and_then(Value::as_str);
        if !k.is_some_and(|k| REF_KINDS.contains(&k)) {
            return Err(bad_scope(BAD_ARG, format!("share: row {rid} is not a fold")));
        }
    }
    let mut out = Map::new();
    out.insert(kind.clone(), Value::from(rid));
    Ok((Value::Object(out), rid))
}

fn grant_view(kernel: &Arc<Kernel>, g: &Row) -> Value {
    let get = |k: &str| g.get(k).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("grant".to_string(), get("grant"));
    out.insert("owner".to_string(), Value::from(display(kernel, &g.get("owner").map(py_str).unwrap_or_default())));
    out.insert("reader".to_string(), Value::from(display(kernel, &g.get("reader").map(py_str).unwrap_or_default())));
    for k in ["scope", "mode", "as_of", "until", "why", "created_by", "ts"] {
        out.insert(k.to_string(), get(k));
    }
    Value::Object(out)
}

fn readable(
    kernel: &Arc<Kernel>,
    id: &AgentId,
    db: &Db<'_>,
    payload: &Value,
    verb: &str,
) -> Result<(String, Option<HashSet<i64>>), Value> {
    let Some(gid) = payload.get("grant").filter(|v| !v.is_null()) else {
        let (owner, _, _) = owner_for(kernel, id, payload, verb)?;
        return Ok((owner, None));
    };
    let (me, who) = caller(kernel, id)?;
    let g = match as_id(gid) {
        Some(n) => db.grant(n).map_err(db_err)?,
        None => None,
    };
    let Some(g) = g else {
        let mut r = refusal(
            NOT_FOUND,
            format!("{verb}: grant {} does not exist — revoked (unshare) or its rows were pruned", py_repr_value(gid)),
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
    let allowed = scope_ids(db, &g).map_err(db_err)?;
    Ok((g.get("owner").map(py_str).unwrap_or_default(), Some(allowed)))
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// ram_store's verb docs, word for word — the one on `boot` names the file.
pub const VERB_DOCS: [(&str, &str); 11] = [
    ("reflect", "Identity + posture: how many owners/rows/grants, who is admin, whether the operator is served, which optional verbs exist. No args."),
    ("boot", "Opens (creating if absent) the SQLite file named by `path`; the tables survive the process."),
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
    Ok(work.join(path))
}

/// The `sqlite_store.tools` bundle: one connection per store agent, each behind
/// its own lock.
#[derive(Default)]
pub struct SqliteStoreBundle {
    conns: Mutex<HashMap<String, Arc<Mutex<Connection>>>>,
}

impl SqliteStoreBundle {
    /// The store's connection, opened (and the schema ensured) on first use.
    fn conn(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Result<Arc<Mutex<Connection>>, Value> {
        if let Some(c) = self.conns.lock().expect("sqlite_store poisoned").get(&id.0) {
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
        c.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
        c.execute_batch(SCHEMA).map_err(db_err)?;
        let c = Arc::new(Mutex::new(c));
        self.conns
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
                q("SELECT COUNT(*) FROM grants")?,
            ]))
        });
        let n = |i: usize| counts.get(i).cloned().unwrap_or(Value::Null);
        let mut verbs = Map::new();
        for (name, d) in VERB_DOCS {
            verbs.insert(name.to_string(), Value::from(d));
        }
        let mut out = Map::new();
        out.insert("id".to_string(), Value::from(id.0.clone()));
        out.insert(
            "sentence".to_string(),
            Value::from(
                "Memory store in SQLite — rows per owner (the caller), folds, grants, search; \
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
        out.insert("grants".to_string(), n(2));
        if counts.get("error").is_some() {
            out.insert("error".to_string(), counts.get("error").cloned().unwrap_or(Value::Null));
        }
        out.insert("operator".to_string(), Value::Bool(rec.get("operator").is_some_and(py_truthy)));
        out.insert(
            "admins".to_string(),
            rec.get("admins").filter(|a| py_truthy(a)).cloned().unwrap_or_else(|| json!([])),
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
                let mut missing_refs = Vec::new();
                for m in list.iter().filter_map(as_id) {
                    if !(db.has(&owner, m).map_err(db_err)? || batch.contains(&m)) {
                        missing_refs.push(m);
                    }
                }
                if !missing_refs.is_empty() {
                    let mut e = refusal(
                        NOT_FOUND,
                        format!("append: row {rid} refs rows that do not exist for this owner: {missing_refs:?}"),
                    );
                    with(&mut e, "arg", json!("rows"));
                    return Err(e);
                }
            }
            let ids: Vec<Value> = staged.iter().map(|r| r.get("id").cloned().unwrap_or(Value::Null)).collect();
            let n = staged.len();
            db.insert(&owner, &staged).map_err(db_err)?;
            Ok(json!({"ids": ids, "appended": n}))
        })
    }

    fn read(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        self.tx(id, kernel, |db| {
            let (owner, allowed) = readable(kernel, id, db, payload, "read")?;
            let mut ids = db.ids(&owner).map_err(db_err)?;
            if let Some(allowed) = &allowed {
                ids.retain(|i| allowed.contains(i));
            }
            let flk = payload.get("from_last_kind").and_then(Value::as_str).filter(|s| !s.is_empty());
            let after = payload.get("after").and_then(as_id);
            let before = payload.get("before").and_then(as_id);
            let desc = payload.get("order").and_then(Value::as_str) == Some("desc");
            // THE ID BOUNDS FIRST, before any row is decoded: `after`/`before` only
            // look at ids, so a cursor read ("what came after my newest row") costs
            // the rows after the cursor, not the owner's whole history. Not with
            // `from_last_kind`, which must see every row to find the newest of a
            // kind. The same bounds are applied again below — idempotent.
            if flk.is_none() {
                match (desc, after) {
                    (true, Some(a)) => ids.retain(|i| *i < a),
                    (false, Some(a)) => ids.retain(|i| *i > a),
                    _ => {}
                }
                if let Some(b) = before {
                    ids.retain(|i| *i < b);
                }
            }
            // Rows are decoded only when a filter reads them, and only the ids
            // still in play.
            let needs_rows = flk.is_some()
                || payload.get("kinds").and_then(Value::as_array).is_some_and(|k| !k.is_empty())
                || payload.get("match").and_then(Value::as_object).is_some_and(|m| !m.is_empty());
            let rows = if !needs_rows {
                HashMap::new()
            } else if flk.is_some() {
                db.all_rows(&owner).map_err(db_err)?
            } else {
                // Let SQLite narrow the candidates by the STRING-valued `match`
                // fields and the `kinds` first (json_extract equality — a
                // superset of the exact filter below, which still decides), so
                // a lookup like the kernel's named-call dedup
                // (`{kinds:[call], match:{name}}`) decodes a handful of rows,
                // not the owner's whole history.
                let eq: Vec<(String, String)> = payload
                    .get("match")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            // a key holding `"` cannot be one quoted path label: left to the exact filter
                            .filter(|(k, _)| !k.contains('"'))
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let kinds: Option<Vec<String>> = payload.get("kinds").and_then(Value::as_array).and_then(|k| {
                    let s: Vec<String> = k.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                    (s.len() == k.len() && !s.is_empty()).then_some(s)
                });
                let got = db.rows_where(&owner, &ids, &eq, kinds.as_deref()).map_err(db_err)?;
                ids.retain(|i| got.contains_key(i));
                got
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
            let limit = payload.get("limit").and_then(as_id);
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
            let (owner, allowed) = readable(kernel, id, db, payload, "search")?;
            let mut ids = db.ids(&owner).map_err(db_err)?;
            if let Some(allowed) = &allowed {
                ids.retain(|i| allowed.contains(i));
            }
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
            let limit = usize::try_from(payload.get("limit").and_then(as_id).unwrap_or(100)).unwrap_or(usize::MAX);
            let width = usize::try_from(payload.get("width").and_then(as_id).unwrap_or(200)).unwrap_or(usize::MAX);
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
        let mode = payload.get("mode").filter(|m| py_truthy(m)).cloned().unwrap_or_else(|| json!("frozen"));
        let Some(mode) = mode.as_str().filter(|m| MODES.contains(m)).map(str::to_string) else {
            let mut r = refusal(BAD_ARG, "share: mode must be one of ('frozen', 'live')");
            with(&mut r, "arg", json!("mode"));
            return r;
        };
        let why_raw = payload.get("why").filter(|w| py_truthy(w)).map(py_str).unwrap_or_default();
        let why: String = py_split_whitespace(&why_raw).join(" ").chars().take(300).collect();
        self.tx(id, kernel, |db| {
            let (scope, anchor) = parse_scope(db, &me, payload.get("scope").unwrap_or(&Value::Null))?;
            let ids = db.ids(&me).map_err(db_err)?;
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
            g.insert("until".to_string(), Value::Null);
            g.insert("why".to_string(), if why.is_empty() { Value::Null } else { Value::from(why.clone()) });
            g.insert(
                "created_by".to_string(),
                fantastic_kernel::current_task().map_or(Value::Null, |t| Value::from(t.get())),
            );
            g.insert("ts".to_string(), Value::from(now_secs()));
            let (gid, created) = db.add_grant(g).map_err(db_err)?;
            let as_of = db
                .grant(gid)
                .map_err(db_err)?
                .and_then(|g| g.get("as_of").cloned())
                .unwrap_or(Value::Null);
            Ok(json!({"grant": gid, "anchor": anchor, "as_of": as_of, "created": created}))
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
        self.tx(id, kernel, |db| {
            let Some(g) = db.grant(gid).map_err(db_err)? else {
                let mut r = refusal(NOT_FOUND, format!("unshare: no grant {gid}"));
                with(&mut r, "arg", json!("grant"));
                return Err(r);
            };
            if g.get("owner").map(py_str).as_deref() != Some(me.as_str()) && !admin {
                let mut r = refusal(UNAUTHORIZED, format!("unshare: grant {gid} is not yours to revoke"));
                with(&mut r, "side", json!("store"));
                return Err(r);
            }
            db.drop_grant(gid).map_err(db_err)?;
            Ok(json!({"unshared": true, "grant": gid}))
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
        self.tx(id, kernel, |db| {
            let out: Vec<Value> = db.grants_where(field, &me).map_err(db_err)?.iter().map(|g| grant_view(kernel, g)).collect();
            Ok(json!({"grants": out}))
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
        let wanted: HashSet<Option<i64>> = want.map(|w| w.iter().map(as_id).collect()).unwrap_or_default();
        let keep: Vec<Value> = payload.get("keep").and_then(Value::as_array).cloned().unwrap_or_default();
        let has_want = want.is_some();
        self.tx(id, kernel, |db| {
            let rows = db.all_rows(&owner).map_err(db_err)?;
            let mut doomed: HashSet<i64> = HashSet::new();
            for i in db.ids(&owner).map_err(db_err)? {
                let kind = rows.get(&i).and_then(|r| r.get("kind")).cloned().unwrap_or(Value::Null);
                if keep.contains(&kind) {
                    continue;
                }
                if before.is_some_and(|b| i < b) || (has_want && wanted.contains(&Some(i))) {
                    doomed.insert(i);
                }
            }
            let (n, g) = db.delete(&owner, &doomed).map_err(db_err)?;
            Ok(json!({"pruned": n, "grants_removed": g}))
        })
    }

    fn inject(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
        let (_, who) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        if !is_admin(kernel, id, &who) {
            let mut r = refusal(UNAUTHORIZED, format!("inject: {} is not an admin of {}", py_repr(&who), id.0));
            with(&mut r, "side", json!("store"));
            return r;
        }
        let staged = match stage_tables(payload.get("tables").unwrap_or(&Value::Null)) {
            Ok(s) => s,
            Err(e) => return e,
        };
        self.tx(id, kernel, |db| {
            // The WHOLE store is replaced, in this one transaction.
            db.0
                .execute_batch(
                    "DELETE FROM grants; DELETE FROM fold_refs; DELETE FROM ref_lists; \
                     DELETE FROM rows; DELETE FROM text_idx; DELETE FROM seq;",
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
            let mut seq = 0i64;
            for (gid, g) in &staged.grants {
                let owner = g.get("owner").map(py_str).unwrap_or_default();
                let reader = g.get("reader").map(py_str).unwrap_or_default();
                let sk = scope_key(g.get("scope").unwrap_or(&Value::Null));
                let mode = g.get("mode").map(py_str).unwrap_or_default();
                let anchor = g.get("anchor").and_then(Value::as_i64).unwrap_or(0);
                db.0
                    .execute(
                        "INSERT OR REPLACE INTO grants (gid, owner, reader, scope_key, mode, anchor, doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![gid, owner, reader, sk, mode, anchor, to_text(g)],
                    )
                    .map_err(db_err)?;
                seq = seq.max(*gid);
            }
            db.set_seq(seq).map_err(db_err)?;
            let grants: i64 = db.0.query_row("SELECT COUNT(*) FROM grants", [], |r| r.get(0)).map_err(db_err)?;
            Ok(json!({"rows": rows, "grants": grants}))
        })
    }

    fn dump(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        let (_, who) = match caller(kernel, id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        if !is_admin(kernel, id, &who) {
            let mut r = refusal(UNAUTHORIZED, format!("dump: {} is not an admin of {}", py_repr(&who), id.0));
            with(&mut r, "side", json!("store"));
            return r;
        }
        self.tx(id, kernel, |db| Ok(json!({"tables": db.dump().map_err(db_err)?})))
    }
}

#[async_trait]
impl Bundle for SqliteStoreBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// ram_store's posture fields, plus `path` — required, relative, inside.
    fn validate_meta(&self, meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
        if meta.get("operator").is_some_and(|v| !v.is_boolean()) {
            return Some("operator must be true or false".to_string());
        }
        match meta.get("admins") {
            None | Some(Value::String(_)) => {}
            Some(Value::Array(a)) if a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())) => {}
            Some(_) => return Some("admins must be a list of agent ids ('kernel' = the operator)".to_string()),
        }
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
            "share" => self.share(agent_id, payload, kernel),
            "unshare" => self.unshare(agent_id, payload, kernel),
            "grants" => self.grants(agent_id, payload, kernel),
            "prune" => self.prune(agent_id, payload, kernel),
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
        self.conns.lock().expect("sqlite_store poisoned").remove(&agent_id.0);
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
    /// must not lose the kernel's last rows. The file closes with the process.
    async fn on_shutdown(&self, _agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        Ok(())
    }
}

