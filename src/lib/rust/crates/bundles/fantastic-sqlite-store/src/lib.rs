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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_repr_value, py_splitlines, py_strip};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::outcome::{body_at, Blob};
use fantastic_kernel::refusal::{missing, refusal, refusal_hint, with, BAD_ARG, EXISTS, NOT_FOUND, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use fantastic_ram_store::{as_id, row_refs, stage_rows, stage_tables};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "sqlite_store.tools";

pub mod record;
pub mod verbs;

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
-- `put_bytes` / `get_bytes`: raw bytes per (owner, ref). Not rows: no text index, never in `dump`.
CREATE TABLE IF NOT EXISTS blobs (
  owner TEXT NOT NULL, ref TEXT NOT NULL, media_type TEXT NOT NULL, data BLOB NOT NULL,
  PRIMARY KEY (owner, ref)
) WITHOUT ROWID;
CREATE VIRTUAL TABLE IF NOT EXISTS text_idx USING fts5 (
  owner UNINDEXED, id UNINDEXED, text, tokenize = 'trigram case_sensitive 1'
);
CREATE TRIGGER IF NOT EXISTS rows_text_gone AFTER DELETE ON rows BEGIN
  DELETE FROM text_idx WHERE owner = old.owner AND id = old.id;
END;
"#;

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

/// The agent's own folder (empty when it is gone).
fn own_root(kernel: &Arc<Kernel>, id: &AgentId) -> PathBuf {
    kernel
        .agents
        .get(id)
        .map(|a| a.root_path.clone())
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

/// Where the file is: `path` by the kernel's one rule ([`Kernel::record_root`]:
/// `@state/…` under the state folder, `@self/…` under the agent's own folder
/// `own`, else under the work folder). A kernel without folders has nowhere
/// durable to put it — refused.
pub fn resolve_path(
    rec: &Map<String, Value>,
    kernel: &Kernel,
    own: &Path,
) -> Result<PathBuf, String> {
    // the record was checked against its declaration (`record.rs`) when it was written
    let raw = serde_json::from_value::<record::Record>(Value::Object(rec.clone()))
        .map_err(|e| format!("sqlite_store: the record's `path`: {e}"))?
        .path;
    let no_disk = "sqlite_store needs a disk-backed kernel: an in-memory kernel has no folder to hold the file";
    if kernel.storage.is_in_memory() {
        return Err(no_disk.to_string());
    }
    let (work, rel) = kernel
        .record_root(&raw, own)
        .map_err(|e| format!("sqlite_store: {e}"))?;
    let path = Path::new(&rel);
    // Clamp like python `fs.resolve`: the deepest part of the path that exists is
    // followed through its symlinks and must still lie under the real base —
    // a `stores -> /elsewhere` link must not carry the file out of its folder.
    let escapes = || {
        format!(
            "sqlite_store: `path` {:?} escapes its folder",
            path.display().to_string()
        )
    };
    let base = work
        .canonicalize()
        .map_err(|e| format!("sqlite_store: {}: {e}", work.display()))?;
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
        let path =
            resolve_path(&record(kernel, id), kernel, &own_root(kernel, id)).map_err(|e| {
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
            Value::from(MAN),
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

    fn append(&self, id: &AgentId, a: &verbs::Append, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        self.tx(id, kernel, |db| {
            let staged = stage_rows(&a.rows, || kernel.mint_id())?;
            let batch: HashSet<i64> = staged.iter().filter_map(|r| r.get("id").and_then(as_id)).collect();
            for r in &staged {
                let rid = r.get("id").and_then(Value::as_i64).unwrap_or(0);
                if db.has(&owner, rid).map_err(db_err)? {
                    let mut e = refusal(EXISTS, format!("append: row {rid} already exists for this owner"));
                    with(&mut e, "id", json!(rid));
                    return Err(e);
                }
            }
            for r in &staged {
                let Some(list) = row_refs(r) else {
                    continue;
                };
                let rid = r.get("id").and_then(Value::as_i64).unwrap_or(0);
                let mut missing_refs: Vec<Value> = Vec::new();
                for m in list {
                    let held = match as_id(m) {
                        Some(i) => db.has(&owner, i).map_err(db_err)? || batch.contains(&i),
                        None => false,
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
            let n = staged.len();
            db.insert(&owner, &staged).map_err(db_err)?;
            Ok(json!({"ids": ids, "appended": n}))
        })
    }

    fn read(&self, id: &AgentId, a: &verbs::Read, kernel: &Arc<Kernel>) -> Value {
        self.tx(id, kernel, |db| {
            let (owner, _) = caller(kernel)?;
            let flk = a.from_last_kind.as_deref();
            let after = a.after;
            let before = a.before;
            let desc = a.order == Some(verbs::Order::Desc);
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
                || a.kinds.is_some()
                || a.r#match.as_ref().is_some_and(|m| !m.is_empty());
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
                let eq: Vec<(String, Value)> = a
                    .r#match
                    .as_ref()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let kinds: Option<Vec<String>> = a.kinds.as_ref().and_then(|k| {
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
            if let Some(kinds) = a.kinds.as_ref() {
                ids.retain(|i| kinds.contains(&row_of(i).get("kind").cloned().unwrap_or(Value::Null)));
            }
            if let Some(m) = a.r#match.as_ref() {
                ids.retain(|i| {
                    let r = row_of(i);
                    m.iter().all(|(k, v)| r.get(k).unwrap_or(&Value::Null) == v)
                });
            }
            if let Some(want) = &a.ids {
                let wanted: HashSet<i64> = want.iter().copied().collect();
                ids.retain(|i| wanted.contains(i));
            }
            if desc {
                ids.reverse();
                if let Some(after) = after {
                    ids.retain(|i| *i < after);
                }
            } else if let Some(after) = after {
                ids.retain(|i| *i > after);
            }
            if let Some(b) = before {
                ids.retain(|i| *i < b);
            }
            let total = ids.len();
            let limit = a.limit;
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

    fn search(&self, id: &AgentId, a: &verbs::Search, kernel: &Arc<Kernel>) -> Value {
        let q = py_strip(&a.query).to_lowercase();
        self.tx(id, kernel, |db| {
            let (owner, _) = caller(kernel)?;
            let mut ids = db.ids(&owner).map_err(db_err)?;
            if let Some(want) = a.ids.as_ref() {
                let mut scope: HashSet<i64> = HashSet::new();
                for rid in want {
                    scope.extend(fold_closure(db, &owner, *rid).map_err(db_err)?);
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
            let limit = usize::try_from(a.limit).unwrap_or(usize::MAX);
            let width = usize::try_from(a.width).unwrap_or(usize::MAX);
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

    fn prune(&self, id: &AgentId, a: &verbs::Prune, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let wanted: HashSet<i64> = a.ids.iter().flatten().copied().collect();
        let keep: &[Value] = a.keep.as_deref().unwrap_or(&[]);
        let out = self.tx(id, kernel, |db| {
            let rows = db.all_rows(&owner).map_err(db_err)?;
            let mut doomed: HashSet<i64> = HashSet::new();
            for i in db.ids(&owner).map_err(db_err)? {
                let kind = rows.get(&i).and_then(|r| r.get("kind")).cloned().unwrap_or(Value::Null);
                if keep.contains(&kind) {
                    continue;
                }
                if a.before_id.is_some_and(|b| i < b) || (a.ids.is_some() && wanted.contains(&i)) {
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
        out
    }

    fn inject(&self, id: &AgentId, a: &verbs::Inject, kernel: &Arc<Kernel>) -> Value {
        let staged = match stage_tables(&a.tables) {
            Ok(s) => s,
            Err(e) => return e,
        };
        self.tx(id, kernel, |db| {
            // The WHOLE store is replaced, in this one transaction.
            db.0
                .execute_batch(
                    "DELETE FROM fold_refs; DELETE FROM ref_lists; DELETE FROM rows; DELETE FROM text_idx; DELETE FROM blobs;",
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

    fn put_bytes(&self, id: &AgentId, a: &verbs::PutBytes, body: Option<&Blob>, kernel: &Arc<Kernel>) -> Value {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let key = a.ref_name.clone();
        let media = a.media_type.clone();
        let Some(bytes) = body_at(body, "data") else {
            return missing("put_bytes", "data (the raw bytes, as the request body)");
        };
        self.tx(id, kernel, |db| {
            db.0.execute(
                "INSERT OR REPLACE INTO blobs (owner, ref, media_type, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![owner, key, media, bytes],
            )
            .map_err(db_err)?;
            Ok(json!({"ref": key, "media_type": media, "bytes": bytes.len()}))
        })
    }

    fn get_bytes(&self, id: &AgentId, a: &verbs::GetBytes, kernel: &Arc<Kernel>) -> (Value, Option<Blob>) {
        let (owner, _) = match caller(kernel) {
            Ok(o) => o,
            Err(r) => return (r, None),
        };
        let key = a.ref_name.clone();
        let mut found: Option<Vec<u8>> = None;
        let out = self.tx(id, kernel, |db| {
            let row = db
                .0
                .query_row(
                    "SELECT media_type, data FROM blobs WHERE owner = ?1 AND ref = ?2",
                    rusqlite::params![owner, key],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
                )
                .optional()
                .map_err(db_err)?;
            match row {
                Some((media, bytes)) => {
                    let head = json!({"ref": key, "media_type": media, "bytes": bytes.len()});
                    found = Some(bytes);
                    Ok(head)
                }
                None => {
                    let mut r = refusal(NOT_FOUND, format!("get_bytes: no bytes under ref `{key}`"));
                    with(&mut r, "ref", Value::from(key.clone()));
                    Err(r)
                }
            }
        });
        (out, found.map(|b| Blob::key("data", b)))
    }

    fn dump(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        self.tx(id, kernel, |db| Ok(json!({"tables": db.dump().map_err(db_err)?})))
    }
}

/// What the bundle is for — its one short man text.
const MAN: &str = "A durable memory store: conversation rows per owner (the caller), folds, search and raw bytes by ref, kept in one SQLite file (`path` on the record) that survives the process.";

#[async_trait]
impl Handler for SqliteStoreBundle {
    type Call = verbs::Call;

    /// The CATALOG name — python's entry-point name, not the handler module
    /// (the kind `reflect bundles=all` lists and a system prompt names).
    fn name(&self) -> &str {
        "sqlite_store"
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

    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        use verbs::Call;
        let out = match call {
            Call::Reflect(_) => self.reflect(agent_id, kernel),
            // Opens the file (the schema ensured): a bad `path` fails LOUD here,
            // at boot, not on the first turn.
            Call::Boot(_) => match self.conn(agent_id, kernel) {
                Ok(_) => return Ok(None),
                Err(e) => e,
            },
            Call::Append(a) => {
                let out = self.append(agent_id, &a, kernel);
                // one `appended` event per new row; a refusal has no `ids`
                for id in out.get("ids").and_then(Value::as_array).into_iter().flatten() {
                    kernel.emit(agent_id, json!({"type": "appended", "table": "rows", "id": id})).await;
                }
                out
            }
            Call::Read(a) => self.read(agent_id, &a, kernel),
            Call::Search(a) => self.search(agent_id, &a, kernel),
            Call::Prune(a) => self.prune(agent_id, &a, kernel),
            Call::Inject(a) => self.inject(agent_id, &a, kernel),
            Call::Dump(_) => self.dump(agent_id, kernel),
            // the bytes verbs ride the binary path only (`handle_binary`)
            Call::PutBytes(_) => refusal(
                BAD_ARG,
                "sqlite_store: put_bytes carries its bytes as a binary body, at `data`",
            ),
            Call::GetBytes(_) => refusal(
                BAD_ARG,
                "sqlite_store: get_bytes carries its bytes as a binary body, at `data`",
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

    /// A deleted store agent DELETES ITS DATA — the file and its WAL/SHM
    /// siblings — as ram_store's tables die with the agent (owner decision,
    /// 2026-09-26). Only an explicit `delete_agent` reaches here: the stop's
    /// teardown walk calls [`Self::on_shutdown`], which keeps the file.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        conns(kernel)
            .0
            .lock()
            .expect("sqlite_store poisoned")
            .remove(&agent_id.0);
        let path = resolve_path(
            &record(kernel, agent_id),
            kernel,
            &own_root(kernel, agent_id),
        )?;
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
