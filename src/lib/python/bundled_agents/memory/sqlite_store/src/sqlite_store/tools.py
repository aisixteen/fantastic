"""sqlite_store — the DURABLE memory store agent.

ram_store's twin: the SAME verbs, arguments, replies, refusal reasons and reflect,
over the SAME tables with the same keys, uniqueness and foreign-key cascades —
kept in ONE SQLite file per store agent instead of RAM dicts. A caller cannot tell
the two apart, except that this one survives the process.

  rows       PRIMARY KEY (owner, id)             the row dict, minus `refs` (JSON)
  ref_lists  PRIMARY KEY (owner, fold)           "this row carries a refs list"
             FK (owner, fold)   → rows ON DELETE CASCADE
  fold_refs  PRIMARY KEY (owner, fold, pos)      a fold's members, in order
             FK (owner, fold)   → ref_lists ON DELETE CASCADE
             FK (owner, member) → rows      ON DELETE CASCADE
  text_idx   FTS5 (trigram) over the lowercased text — the search index

The schema is the rust `sqlite_store`'s, statement for statement, so one file
opens in either runtime.

OWNER = CALLER: every row is keyed by who wrote it, `<kernel guid>|<agent id>`
(`<kernel guid>|kernel` for the operator and the kernel's own bookkeeping); a
caller reads and writes its own rows. Who may call any verb — `inject` / `dump`
included — is the kernel's call gate alone.

`ref_lists` exists because ram_store keeps a fold's refs list even after every
member was pruned (`refs: []`); a bare member table could not say "empty list"
apart from "no list".

SEARCH keeps ram_store's semantics exactly — a case-insensitive SUBSTRING, then
line by line — with the trigram index doing the candidate filter (`GLOB`).

READ narrows in SQL first (id bounds, a `name`, a `parent`, the `kinds`) and then
applies ram_store's own filters to what came back: the SQL is a superset, the
python filters decide, so the answer is ram_store's whatever the arguments.

Every verb is ONE transaction and has no `await` inside it, so no other dispatch
on the loop sees a half-applied verb.

The file: `path` on the record, REQUIRED (no default), relative to the directory
the kernel runs in, never absolute and never climbing out (`..`); the opened path
is clamped through `file_bridge.fs` like every other path this kernel opens. WAL
journal, `synchronous=NORMAL`, a busy timeout. `delete_agent` deletes the file and
its WAL/SHM; a kernel stop keeps it, checkpointing the WAL on close.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

from file_bridge import fs
from kernel import current_origin
from kernel._refusal import (
    BAD_ARG,
    EXISTS,
    NOT_FOUND,
    UNHANDLED,
    missing,
    refusal,
)
from ram_store.tools import _as_id, _text_of, load_tables

OPERATOR = "kernel"
BUSY_TIMEOUT_MS = 5000  # how long a write waits on another connection's lock

SCHEMA = """
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
CREATE INDEX IF NOT EXISTS rows_by_name ON rows (owner, json_extract(doc, '$."name"'));
CREATE INDEX IF NOT EXISTS rows_by_parent ON rows (owner, json_extract(doc, '$."parent"'));
CREATE VIRTUAL TABLE IF NOT EXISTS text_idx USING fts5 (
  owner UNINDEXED, id UNINDEXED, text, tokenize = 'trigram case_sensitive 1'
);
CREATE TRIGGER IF NOT EXISTS rows_text_gone AFTER DELETE ON rows BEGIN
  DELETE FROM text_idx WHERE owner = old.owner AND id = old.id;
END;
"""

# SQLite's INTEGER is a signed 64-bit value: an int outside it is never pushed
# into a query (the python filter still judges it).
_I64 = (-(2**63), 2**63 - 1)


class Refused(Exception):
    """A verb's refusal raised out of its transaction — rolls the transaction back."""

    def __init__(self, reply: dict) -> None:
        super().__init__(reply.get("error"))
        self.reply = reply


def _db_err(e: Exception) -> dict:
    return refusal(UNHANDLED, f"sqlite_store: {e}")


def _sql_int(v) -> bool:
    return isinstance(v, int) and not isinstance(v, bool) and _I64[0] <= v <= _I64[1]


def _glob_substring(q: str) -> str:
    """A GLOB pattern matching `q` as a substring (metacharacters bracketed)."""
    return "*" + "".join(f"[{c}]" if c in "*?[]" else c for c in q) + "*"


def _encode(row: dict) -> str:
    """A row as the JSON `doc` column. NaN / infinity and non-JSON values are not
    rows the file can hold — refused, never rewritten."""
    try:
        return json.dumps(row, ensure_ascii=False, allow_nan=False)
    except (TypeError, ValueError) as e:
        raise Refused(
            refusal(
                BAD_ARG, f"append: row {row.get('id')} is not JSON: {e}", arg="rows"
            )
        ) from None


# ─── the tables, over one connection ─────────────────────────────────────


class Db:
    """ram_store's `Tables`, as SQL over one open connection."""

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.c = conn

    def has(self, owner: str, rid) -> bool:
        if not _sql_int(rid):
            return False
        return (
            self.c.execute(
                "SELECT 1 FROM rows WHERE owner = ? AND id = ?", (owner, rid)
            ).fetchone()
            is not None
        )

    def ids(self, owner: str) -> list[int]:
        return [
            r[0]
            for r in self.c.execute(
                "SELECT id FROM rows WHERE owner = ? ORDER BY id", (owner,)
            )
        ]

    def row(self, owner: str, rid: int) -> dict | None:
        r = self.c.execute(
            "SELECT doc FROM rows WHERE owner = ? AND id = ?", (owner, rid)
        ).fetchone()
        return json.loads(r[0]) if r is not None else None

    def refs(self, owner: str, fold: int) -> list[int] | None:
        listed = self.c.execute(
            "SELECT 1 FROM ref_lists WHERE owner = ? AND fold = ?", (owner, fold)
        ).fetchone()
        if listed is None:
            return None
        return [
            r[0]
            for r in self.c.execute(
                "SELECT member FROM fold_refs WHERE owner = ? AND fold = ? ORDER BY pos",
                (owner, fold),
            )
        ]

    def get(self, owner: str, rid: int) -> dict | None:
        """ram_store `get`: the row, `refs` appended when it carries a list."""
        out = self.row(owner, rid)
        if out is None:
            return None
        refs = self.refs(owner, rid)
        if refs is not None:
            out["refs"] = refs
        return out

    def last_of_kind(self, owner: str, kind: str) -> int | None:
        """The newest id whose `kind` is the string `kind`."""
        r = self.c.execute(
            "SELECT MAX(id) FROM rows WHERE owner = ? AND json_type(doc, '$.kind') = 'text'"
            " AND json_extract(doc, '$.kind') = ?",
            (owner, kind),
        ).fetchone()
        return r[0] if r is not None else None

    def rows_between(
        self,
        owner: str,
        lo: int | None,
        hi: int | None,
        *,
        name: str | None = None,
        parent: int | None = None,
        kinds: list[str] | None = None,
        ids_only: bool = False,
    ) -> dict[int, dict | None]:
        """The rows of one owner strictly between `lo` and `hi` (either open),
        narrowed to a `name` (string), a `parent` (int) and `kind`s (strings) —
        a SUPERSET of ram_store's exact filters, which the caller still applies.
        The json paths are literals, so `name` and `parent` hit their indexes.
        `ids_only` leaves each row undecoded (`None`)."""
        if name is not None:
            index = " INDEXED BY rows_by_name"
        elif parent is not None:
            index = " INDEXED BY rows_by_parent"
        else:
            index = ""
        sql = f"SELECT id{'' if ids_only else ', doc'} FROM rows{index} WHERE owner = ?"
        args: list = [owner]
        if lo is not None:
            sql += " AND id > ?"
            args.append(lo)
        if hi is not None:
            sql += " AND id < ?"
            args.append(hi)
        if name is not None:
            sql += " AND json_extract(doc, '$.\"name\"') = ?"
            args.append(name)
        if parent is not None:
            sql += " AND json_extract(doc, '$.\"parent\"') = ?"
            args.append(parent)
        if kinds:
            sql += f" AND json_extract(doc, '$.kind') IN ({','.join('?' * len(kinds))})"
            args.extend(kinds)
        found = self.c.execute(sql + " ORDER BY id", args)
        if ids_only:
            return {r[0]: None for r in found}
        return {i: json.loads(d) for i, d in found}

    def set_refs(self, owner: str, fold: int, members: list[int]) -> None:
        self.c.execute(
            "INSERT OR REPLACE INTO ref_lists (owner, fold) VALUES (?, ?)",
            (owner, fold),
        )
        self.c.execute(
            "DELETE FROM fold_refs WHERE owner = ? AND fold = ?", (owner, fold)
        )
        self.c.executemany(
            "INSERT INTO fold_refs (owner, fold, pos, member) VALUES (?, ?, ?, ?)",
            [(owner, fold, pos, m) for pos, m in enumerate(members)],
        )

    def insert(self, owner: str, rows: list[dict]) -> None:
        """ram_store `insert`: the caller validated every row and every ref."""
        for row in rows:
            rid = row["id"]
            stored = {k: v for k, v in row.items() if k != "refs"}
            self.c.execute(
                "INSERT INTO rows (owner, id, doc) VALUES (?, ?, ?)",
                (owner, rid, _encode(stored)),
            )
            self.c.execute(
                "INSERT INTO text_idx (owner, id, text) VALUES (?, ?, ?)",
                (owner, rid, _text_of(row).lower()),
            )
        # Refs after every row of the batch is in: a fold may name a member that
        # arrives later in the same batch.
        for row in rows:
            refs = row.get("refs")
            if isinstance(refs, list):
                self.set_refs(owner, row["id"], [int(m) for m in refs])

    def delete(self, owner: str, doomed: set[int]) -> int:
        """DELETE the rows; the foreign keys cascade to fold refs and the trigger
        to the text index. Returns the rows removed."""
        n = 0
        for rid in doomed:
            n += self.c.execute(
                "DELETE FROM rows WHERE owner = ? AND id = ?", (owner, rid)
            ).rowcount
        return n

    def dump(self) -> dict:
        rows = [
            {"owner": owner, "row": json.loads(d)}
            for owner, d in self.c.execute(
                "SELECT owner, doc FROM rows ORDER BY owner, id"
            )
        ]
        lists = self.c.execute(
            "SELECT owner, fold FROM ref_lists ORDER BY owner, fold"
        ).fetchall()
        fold_refs = [
            {"owner": owner, "fold": fold, "members": self.refs(owner, fold) or []}
            for owner, fold in lists
        ]
        return {"rows": rows, "fold_refs": fold_refs}


# ─── the file ────────────────────────────────────────────────────────────


def _path_problem(rec: dict) -> str | None:
    """Why a record's `path` cannot name the file: it is required, relative, and
    never climbs out of the kernel's running dir."""
    p = rec.get("path")
    if not isinstance(p, str) or not p.strip():
        return (
            "sqlite_store needs `path`: the database file, relative to the "
            "project dir (no default)"
        )
    absolute = fs.is_absolute(p) or Path(p).is_absolute()
    climbs = ".." in Path(p).parts
    if absolute or climbs:
        return f"sqlite_store: `path` must stay inside the project dir, got {p!r}"
    return None


def validate_meta(meta: dict, record: dict) -> str | None:
    """`path` — required, relative, inside — refused at the write."""
    return _path_problem({**(record or {}), **meta})


def _resolve(agent):
    """The database file: `path` under the kernel's running dir, clamped by `fs`
    (a symlink out of the dir is refused too). Returns the Path or a refusal."""
    why = _path_problem(agent.record)
    if why is not None:
        return refusal(BAD_ARG, why, arg="path")
    try:
        return fs.resolve(fs.Root(agent.ctx.workdir), agent.record["path"])
    except (ValueError, RuntimeError) as e:
        return refusal(BAD_ARG, f"sqlite_store: {e}", arg="path")


def _conn(agent) -> sqlite3.Connection | dict:
    """The store's connection, opened (and the schema ensured) on first use. It
    lives ON the agent instance — one kernel's store is never another's."""
    c = getattr(agent, "_sqlite_conn", None)
    if c is not None:
        return c
    path = _resolve(agent)
    if isinstance(path, dict):
        return path
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        c = sqlite3.connect(path, isolation_level=None)
        c.execute("PRAGMA journal_mode = WAL")
        c.execute("PRAGMA synchronous = NORMAL")
        # A freed page is zeroed, not left holding the deleted text.
        c.execute("PRAGMA secure_delete = ON")
        c.execute(f"PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}")
        c.executescript(SCHEMA)
    except (OSError, sqlite3.Error) as e:
        return refusal(UNHANDLED, f"sqlite_store: {path}: {e}")
    agent._sqlite_conn = c
    return c


def _close(agent) -> str | None:
    """Checkpoint the WAL into the file and close; returns why it could not."""
    c = getattr(agent, "_sqlite_conn", None)
    if c is None:
        return None
    agent._sqlite_conn = None
    try:
        c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        return None
    except sqlite3.Error as e:
        return f"sqlite_store {agent.id}: checkpoint: {e}"
    finally:
        c.close()


def _tx(agent, fn) -> dict:
    """Run `fn(db)` in ONE transaction; a `Refused` rolls it back."""
    c = _conn(agent)
    if isinstance(c, dict):
        return c
    try:
        c.execute("BEGIN IMMEDIATE")
    except sqlite3.Error as e:
        return _db_err(e)
    try:
        out = fn(Db(c))
    except Refused as r:
        c.execute("ROLLBACK")
        return r.reply
    except (sqlite3.Error, OverflowError) as e:
        c.execute("ROLLBACK")
        return _db_err(e)
    except BaseException:
        c.execute("ROLLBACK")
        raise
    try:
        c.execute("COMMIT")
    except sqlite3.Error as e:
        c.execute("ROLLBACK")
        return _db_err(e)
    return out


# ─── who is calling ──────────────────────────────────────────────────────


def _guid(agent) -> str | None:
    root = getattr(agent.ctx, "root", None)
    return (root.record.get("guid") if root is not None else None) or None


def _owner(agent) -> str:
    """The caller's key — `<guid>|kernel` for the operator — or `Refused`."""
    g = _guid(agent)
    if not g:
        raise Refused(
            refusal(
                NOT_FOUND,
                "sqlite_store: this kernel has no guid yet — the store keys every row by it",
                hint="boot the kernel root first (it mints the guid)",
            )
        )
    origin = current_origin()
    return f"{g}|{OPERATOR if origin is None else origin}"


def _fold_closure(db: Db, owner: str, fold: int) -> set[int]:
    out: set[int] = set()
    stack = [fold]
    while stack:
        x = stack.pop()
        if x in out:
            continue
        out.add(x)
        stack.extend((db.refs(owner, x) or []) if _sql_int(x) else [])
    return out


# ─── verbs ───────────────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + posture: how many owners/rows, which optional verbs exist. No args."""

    def counts(db: Db) -> dict:
        owners, rows = db.c.execute(
            "SELECT COUNT(DISTINCT owner), COUNT(*) FROM rows"
        ).fetchone()
        return {"owners": owners, "rows": rows}

    n = _tx(agent, counts)
    out = {
        "id": id,
        "sentence": "Memory store in SQLite — rows per owner (the caller), folds, "
        "search; durable: one database file, the tables survive the process.",
        "durable": True,
        "path": agent.record.get("path"),
        "owners": n.get("owners"),
        "rows": n.get("rows"),
    }
    if "error" in n:
        out["error"] = n["error"]
    out["optional_verbs"] = ["search"]
    return out


async def _boot(id, payload, agent):
    """Opens (creating if absent) the SQLite file named by `path`; the tables survive the process."""
    c = _conn(agent)
    return c if isinstance(c, dict) else None


async def _append(id, payload, agent):
    """args: rows:list[dict] (req). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}."""
    rows = payload.get("rows")

    def run(db: Db) -> dict:
        owner = _owner(agent)
        if (
            not isinstance(rows, list)
            or not rows
            or not all(isinstance(r, dict) for r in rows)
        ):
            raise Refused(
                refusal(
                    BAD_ARG,
                    "append: `rows` must be a non-empty list of rows",
                    arg="rows",
                )
            )
        staged: list[dict] = []
        batch: set[int] = set()
        for r in rows:
            rid = r.get("id")
            if rid is None:
                rid = agent.ctx.mint_id()
            elif _as_id(rid) is None or isinstance(rid, str):
                raise Refused(
                    refusal(
                        BAD_ARG,
                        f"append: a row id is a kernel int, got {rid!r}",
                        arg="rows",
                    )
                )
            if db.has(owner, rid) or rid in batch:
                raise Refused(
                    refusal(
                        EXISTS,
                        f"append: row {rid} already exists for this owner",
                        id=rid,
                    )
                )
            batch.add(rid)
            staged.append({**r, "id": rid})
        for r in staged:
            refs = r.get("refs")
            if refs is None:
                continue
            if not isinstance(refs, list) or any(_as_id(m) is None for m in refs):
                raise Refused(
                    refusal(
                        BAD_ARG,
                        f"append: row {r['id']} refs must be row ids",
                        arg="rows",
                    )
                )
            missing_refs = [m for m in refs if not (db.has(owner, m) or m in batch)]
            if missing_refs:
                raise Refused(
                    refusal(
                        NOT_FOUND,
                        f"append: row {r['id']} refs rows that do not exist for this owner: {missing_refs}",
                        arg="rows",
                    )
                )
        db.insert(owner, staged)
        return {"ids": [r["id"] for r in staged], "appended": len(staged)}

    return _tx(agent, run)


async def _read(id, payload, agent):
    """args: after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}."""

    def run(db: Db) -> dict:
        owner = _owner(agent)
        match = payload.get("match")
        named = match.get("name") if isinstance(match, dict) else None
        parent = match.get("parent") if isinstance(match, dict) else None
        kinds = payload.get("kinds")
        flk = payload.get("from_last_kind")
        after, before = _as_id(payload.get("after")), _as_id(payload.get("before"))
        desc = payload.get("order") == "desc"
        # THE ID BOUNDS IN THE SQL: each filter below that only looks at ids is
        # pushed down as a bound, so a cursor read costs the rows past the cursor,
        # not the owner's whole history. The same filters run again on the result.
        lo = hi = None
        last = None
        if isinstance(flk, str) and flk:
            last = db.last_of_kind(owner, flk)
            if last is not None:
                lo = last - 1
        if after is not None and _sql_int(after):
            if desc:
                hi = after
            else:
                lo = after if lo is None else max(lo, after)
        if before is not None and _sql_int(before):
            hi = before if hi is None else min(hi, before)
        # Rows are decoded only when a filter reads them; a plain page reads ids.
        needs_rows = (
            last is not None
            or (isinstance(kinds, list) and bool(kinds))
            or (isinstance(match, dict) and bool(match))
        )
        rows = db.rows_between(
            owner,
            lo,
            hi,
            name=named if isinstance(named, str) else None,
            parent=parent if _sql_int(parent) else None,
            kinds=kinds
            if isinstance(kinds, list)
            and kinds
            and all(isinstance(k, str) for k in kinds)
            else None,
            ids_only=not needs_rows,
        )
        # ram_store's filters, in ram_store's order, over the narrowed rows.
        ids = list(rows)
        if isinstance(named, str):
            ids = [i for i in ids if rows[i].get("name") == named]
        if last is not None:
            ids = [i for i in ids if i >= last]
        if isinstance(kinds, list) and kinds:
            ids = [i for i in ids if rows[i].get("kind") in kinds]
        if isinstance(match, dict) and match:
            ids = [i for i in ids if all(rows[i].get(k) == v for k, v in match.items())]
        want = payload.get("ids")
        if isinstance(want, list):
            wanted = {_as_id(x) for x in want}
            ids = [i for i in ids if i in wanted]
        if desc:
            ids = list(reversed(ids))
            if after is not None:
                ids = [i for i in ids if i < after]
        elif after is not None:
            ids = [i for i in ids if i > after]
        if before is not None:
            ids = [i for i in ids if i < before]
        total = len(ids)
        limit = _as_id(payload.get("limit"))
        page = ids[:limit] if limit else ids
        nxt = page[-1] if limit and total > len(page) else None
        return {"rows": [db.get(owner, i) for i in page], "next": nxt, "total": total}

    return _tx(agent, run)


async def _search(id, payload, agent):
    """args: query:str (req — case-insensitive substring), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}."""
    q = str(payload.get("query") or "").strip().lower()
    if not q:
        return missing("search", "query")

    def run(db: Db) -> dict:
        owner = _owner(agent)
        ids = db.ids(owner)
        want = payload.get("ids")
        if isinstance(want, list) and want:
            scope: set[int] = set()
            for x in want:
                rid = _as_id(x)
                if rid is not None:
                    scope |= _fold_closure(db, owner, rid)
            ids = [i for i in ids if i in scope]
        # THE INDEX picks the candidate rows (trigram GLOB = substring).
        hits = {
            r[0]
            for r in db.c.execute(
                "SELECT id FROM text_idx WHERE owner = ? AND text GLOB ?",
                (owner, _glob_substring(q)),
            )
        }
        limit = _as_id(payload.get("limit")) or 100
        width = _as_id(payload.get("width")) or 200
        matches, total = [], 0
        for i in ids:
            if i not in hits:
                continue
            for n, line in enumerate(_text_of(db.row(owner, i)).splitlines(), 1):
                if q in line.lower():
                    total += 1
                    if len(matches) < limit:
                        matches.append(
                            {"id": i, "line": n, "text": line.strip()[:width]}
                        )
        return {"matches": matches, "total": total, "truncated": total > len(matches)}

    return _tx(agent, run)


async def _prune(id, payload, agent):
    """args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). Returns {pruned}."""
    before = _as_id(payload.get("before_id"))
    want = payload.get("ids")

    def run(db: Db) -> dict:
        owner = _owner(agent)
        if before is None and not isinstance(want, list):
            raise Refused(missing("prune", "before_id"))
        keep = payload.get("keep") or []
        wanted = {_as_id(x) for x in want} if isinstance(want, list) else set()
        doomed: set[int] = set()
        for i, d in db.c.execute(
            "SELECT id, doc FROM rows WHERE owner = ? ORDER BY id", (owner,)
        ).fetchall():
            if json.loads(d).get("kind") in keep:
                continue
            if before is not None and i < before:
                doomed.add(i)
            elif isinstance(want, list) and i in wanted:
                doomed.add(i)
        return {"pruned": db.delete(owner, doomed)}

    out = _tx(agent, run)
    if out.get("pruned", 0) > 0:
        # The deleted rows are still in the -wal file until a checkpoint moves
        # the pages into memory.db and truncates the log: do it now, so a prune
        # leaves no bytes behind. Best effort, like the close's checkpoint.
        c = getattr(agent, "_sqlite_conn", None)
        if c is not None:
            try:
                c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            except sqlite3.Error:
                pass
    return out


async def _inject(id, payload, agent):
    """args: tables:dict (req — a `dump`). Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}."""
    # ram_store's own validator: the same checks, in the same order, with the
    # same refusals — then its tables are written here in one transaction.
    t = load_tables(payload.get("tables"))
    if isinstance(t, dict):
        return t

    def run(db: Db) -> dict:
        for table in ("fold_refs", "ref_lists", "rows", "text_idx"):
            db.c.execute(f"DELETE FROM {table}")
        n = 0
        for owner in t.rows:
            rows = [t.rows[owner][rid] for rid in t.order.get(owner, [])]
            db.insert(owner, [{**r, "id": _as_id(r["id"])} for r in rows])
            n += len(rows)
        for (owner, fold), members in t.fold_refs.items():
            db.set_refs(owner, _as_id(fold), members)
        return {"rows": n}

    return _tx(agent, run)


async def _dump(id, payload, agent):
    """No args. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}."""
    return _tx(agent, lambda db: {"tables": db.dump()})


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "append": _append,
    "read": _read,
    "search": _search,
    "prune": _prune,
    "inject": _inject,
    "dump": _dump,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"sqlite_store: unknown type {t!r}", verb=t)
    return await fn(id, payload, agent)


async def on_delete(agent) -> None:
    """A deleted store DELETES ITS DATA — the file and its WAL/SHM siblings — as
    ram_store's tables die with the agent. Only an explicit `delete_agent` reaches
    here; a kernel stop runs `on_shutdown`, which keeps the file."""
    _close(agent)
    path = _resolve(agent)
    if isinstance(path, dict):
        raise RuntimeError(path["error"])
    for suffix in ("", "-wal", "-shm"):
        try:
            path.with_name(path.name + suffix).unlink()
        except FileNotFoundError:
            pass


async def on_shutdown(agent) -> None:
    """A kernel stop: checkpoint the WAL into the file and close it, keeping the
    file. The kernel runs this walk AFTER its final ledger flush, so the last rows
    are already in."""
    why = _close(agent)
    if why is not None:
        raise RuntimeError(why)
