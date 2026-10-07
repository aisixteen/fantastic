"""ram_store — a MEMORY STORE agent held in RAM.

A memory store keeps conversation rows for MANY owners (multi-tenant). Every row is
keyed by `(owner, id)`; the owner is never a field a caller writes — it is WHO CALLED:
`<kernel guid>|<caller agent id>` for a local caller, the qualified address a bridge
leg relays for a caller on another kernel, and `<guid>|kernel` for the operator
(`sender=None`). So a caller writes ONLY under its own key, by construction.

THE TOPOLOGY IS SQLITE'S. The tables below are the ones a durable `sqlite_store`
keeps, with the same keys, the same uniqueness, the same indexes and the same
foreign-key cascades; only the query language is missing. A caller cannot tell the
two apart, and the durable port is a 1:1 mirror under the same verbs:

  rows       PRIMARY KEY (owner, id)            the row dict, minus `refs`
             INDEX (owner, id) ordered           paging, `from_last_kind`
  fold_refs  (owner, fold_id, member_id)         a fold's members, in order
             FK (owner, fold_id)   → rows ON DELETE CASCADE
             FK (owner, member_id) → rows ON DELETE CASCADE
  text_idx   (owner, id) → lowercased text       the FTS5 counterpart; derived

ACCESS: a caller reads and writes only its OWN key (append / read / search /
prune). `inject` / `dump` act on the whole store. WHO may call any verb is the
kernel's call gate alone.

Ids are the KERNEL's: a row carries the int the kernel minted, or none and this
store takes the next one from the kernel's counter — never an id of its own.

Everything here dies with the process. That is what `ram` says.
"""

from __future__ import annotations

import bisect
import json

from kernel import current_origin
from kernel._refusal import (
    BAD_ARG,
    EXISTS,
    NOT_FOUND,
    UNHANDLED,
    missing,
    refusal,
)

OPERATOR = "kernel"


def _as_id(x) -> int | None:
    if isinstance(x, bool):
        return None
    if isinstance(x, int):
        return x if x > 0 else None
    # ASCII digits only: `'²'.isdigit()` is True and `int('²')` raises — a row id
    # is a counter value, never a superscript (parity 2026-09-30; rust/swift agree).
    if isinstance(x, str) and x.strip().isascii() and x.strip().isdigit():
        return int(x.strip())
    return None


def _text_of(row: dict) -> str:
    c = row.get("content")
    text = c if isinstance(c, str) else json.dumps(c, default=str, ensure_ascii=False)
    title = row.get("title")
    if isinstance(title, str) and title:
        text = f"{title}\n{text}"
    return text


class Tables:
    """The store's tables — RAM dicts in sqlite's shape (see the module docstring)."""

    def __init__(self) -> None:
        self.rows: dict[str, dict[int, dict]] = {}
        self.order: dict[str, list[int]] = {}
        self.fold_refs: dict[tuple[str, int], list[int]] = {}
        self.ref_by_member: dict[tuple[str, int], set[int]] = {}
        self.text: dict[tuple[str, int], str] = {}

    # ── rows ─────────────────────────────────────────────────────────
    def has(self, owner: str, rid: int) -> bool:
        return rid in self.rows.get(owner, {})

    def ids(self, owner: str) -> list[int]:
        return self.order.get(owner, [])

    def get(self, owner: str, rid: int) -> dict | None:
        row = self.rows.get(owner, {}).get(rid)
        if row is None:
            return None
        out = dict(row)
        refs = self.fold_refs.get((owner, rid))
        if refs is not None:
            out["refs"] = list(refs)
        return out

    def insert(self, owner: str, rows: list[dict]) -> None:
        """All-or-nothing: the caller has validated every row and every ref."""
        table = self.rows.setdefault(owner, {})
        order = self.order.setdefault(owner, [])
        for row in rows:
            rid = row["id"]
            refs = row.get("refs")
            stored = {k: v for k, v in row.items() if k != "refs"}
            table[rid] = stored
            bisect.insort(order, rid)
            if isinstance(refs, list):
                members = [int(m) for m in refs]
                self.fold_refs[(owner, rid)] = members
                for m in members:
                    self.ref_by_member.setdefault((owner, m), set()).add(rid)
            self.text[(owner, rid)] = _text_of(row).lower()

    def delete(self, owner: str, doomed: set[int]) -> int:
        """DELETE FROM rows WHERE owner=? AND id IN doomed — with every cascade.
        Returns the rows removed."""
        table = self.rows.get(owner, {})
        doomed = {r for r in doomed if r in table}
        for rid in doomed:
            del table[rid]
            self.text.pop((owner, rid), None)
            # FK fold_refs.fold_id → rows: the fold's member list goes with it.
            for m in self.fold_refs.pop((owner, rid), []):
                s = self.ref_by_member.get((owner, m))
                if s is not None:
                    s.discard(rid)
                    if not s:
                        del self.ref_by_member[(owner, m)]
            # FK fold_refs.member_id → rows: the member leaves every fold naming it.
            for f in self.ref_by_member.pop((owner, rid), set()):
                refs = self.fold_refs.get((owner, f))
                if refs is not None:
                    self.fold_refs[(owner, f)] = [m for m in refs if m != rid]
        if doomed:
            self.order[owner] = [
                i for i in self.order.get(owner, []) if i not in doomed
            ]
        return len(doomed)

    # ── dump / inject ────────────────────────────────────────────────
    def dump(self) -> dict:
        return {
            "rows": [
                {"owner": owner, "row": dict(self.rows[owner][rid])}
                for owner in sorted(self.rows)
                for rid in self.order.get(owner, [])
            ],
            "fold_refs": [
                {"owner": owner, "fold": fold, "members": list(members)}
                for (owner, fold), members in sorted(self.fold_refs.items())
            ],
        }


def load_tables(tables: dict) -> Tables | dict:
    """Build a `Tables` from a `dump` — every key and FK checked; a refusal on the
    first violation (nothing half-loaded)."""
    if not isinstance(tables, dict):
        return refusal(BAD_ARG, "inject: `tables` must be a dump dict", arg="tables")
    t = Tables()
    rows = tables.get("rows") or []
    if not isinstance(rows, list):
        return refusal(BAD_ARG, "inject: tables.rows must be a list", arg="tables")
    by_owner: dict[str, list[dict]] = {}
    for item in rows:
        if not isinstance(item, dict) or not isinstance(item.get("owner"), str):
            return refusal(
                BAD_ARG, "inject: every row needs {owner, row}", arg="tables"
            )
        row = item.get("row")
        if not isinstance(row, dict) or _as_id(row.get("id")) is None:
            return refusal(BAD_ARG, "inject: every row needs an int id", arg="tables")
        by_owner.setdefault(item["owner"], []).append(
            {k: v for k, v in row.items() if k != "refs"}
        )
    for owner, rs in by_owner.items():
        seen: set[int] = set()
        for r in rs:
            if r["id"] in seen:
                return refusal(
                    EXISTS, f"inject: duplicate row ({owner}, {r['id']})", id=r["id"]
                )
            seen.add(r["id"])
        t.insert(owner, rs)
    for fr in tables.get("fold_refs") or []:
        owner, fold = fr.get("owner"), _as_id(fr.get("fold"))
        members = fr.get("members") or []
        if fold is None or not t.has(owner, fold):
            return refusal(
                BAD_ARG, f"inject: fold_refs names no row {fold!r}", arg="tables"
            )
        if not all(t.has(owner, _as_id(m) or -1) for m in members):
            return refusal(
                BAD_ARG, f"inject: fold {fold} refs a missing row", arg="tables"
            )
        t.fold_refs[(owner, fold)] = [int(m) for m in members]
        for m in members:
            t.ref_by_member.setdefault((owner, int(m)), set()).add(fold)
        row = t.rows[owner][fold]
        t.text[(owner, fold)] = _text_of(row).lower()
    return t


def _tables(agent) -> Tables:
    """The store's tables live ON the agent instance: one kernel's `mem` is never
    another kernel's `mem`, even in one process. RAM by design."""
    t = getattr(agent, "_ram_tables", None)
    if t is None:
        t = agent._ram_tables = Tables()
    return t


# ─── who is calling ────────────────────────────────────────────────


def _guid(agent) -> str | None:
    root = getattr(agent.ctx, "root", None)
    return (root.record.get("guid") if root is not None else None) or None


def _caller(agent) -> tuple[str, str] | dict:
    """(key, display) for whoever is calling — `<guid>|kernel` for the operator —
    or a refusal."""
    g = _guid(agent)
    if not g:
        return refusal(
            NOT_FOUND,
            "ram_store: this kernel has no guid yet — the store keys every row by it",
            hint="boot the kernel root first (it mints the guid)",
        )
    origin = current_origin()
    if origin is None:
        return f"{g}|{OPERATOR}", OPERATOR
    return f"{g}|{origin}", origin


# ─── scopes ────────────────────────────────────────────────────────


def _fold_closure(t: Tables, owner: str, fold: int) -> set[int]:
    out: set[int] = set()
    stack = [fold]
    while stack:
        x = stack.pop()
        if x in out:
            continue
        out.add(x)
        stack.extend(t.fold_refs.get((owner, x), []))
    return out


# ─── verbs ─────────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + posture: how many owners/rows, which optional verbs exist. No args."""
    t = _tables(agent)
    return {
        "id": id,
        "sentence": "Memory store in RAM — rows per owner (the caller), folds, "
        "search; ephemeral: everything is lost when the process exits.",
        "durable": False,
        "owners": len([o for o in t.rows if t.rows[o]]),
        "rows": sum(len(v) for v in t.rows.values()),
        "optional_verbs": ["search"],
    }


async def _boot(id, payload, agent):
    """No-op — the tables start empty (RAM) or are filled by `inject`."""
    return None


async def _append(id, payload, agent):
    """args: rows:list[dict] (req). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    owner = who[0]
    rows = payload.get("rows")
    if (
        not isinstance(rows, list)
        or not rows
        or not all(isinstance(r, dict) for r in rows)
    ):
        return refusal(
            BAD_ARG, "append: `rows` must be a non-empty list of rows", arg="rows"
        )
    t = _tables(agent)
    staged: list[dict] = []
    batch: set[int] = set()
    for r in rows:
        rid = r.get("id")
        if rid is None:
            rid = agent.ctx.mint_id()
        elif _as_id(rid) is None or isinstance(rid, str):
            return refusal(
                BAD_ARG, f"append: a row id is a kernel int, got {rid!r}", arg="rows"
            )
        if t.has(owner, rid) or rid in batch:
            return refusal(
                EXISTS, f"append: row {rid} already exists for this owner", id=rid
            )
        batch.add(rid)
        staged.append({**r, "id": rid})
    for r in staged:
        refs = r.get("refs")
        if refs is None:
            continue
        if not isinstance(refs, list) or any(_as_id(m) is None for m in refs):
            return refusal(
                BAD_ARG, f"append: row {r['id']} refs must be row ids", arg="rows"
            )
        missing_refs = [m for m in refs if not (t.has(owner, m) or m in batch)]
        if missing_refs:
            return refusal(
                NOT_FOUND,
                f"append: row {r['id']} refs rows that do not exist for this owner: {missing_refs}",
                arg="rows",
            )
    t.insert(owner, staged)
    return {"ids": [r["id"] for r in staged], "appended": len(staged)}


async def _read(id, payload, agent):
    """args: after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}."""
    t = _tables(agent)
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    owner = who[0]
    ids = list(t.ids(owner))
    flk = payload.get("from_last_kind")
    if isinstance(flk, str) and flk:
        last = next(
            (i for i in reversed(ids) if t.rows[owner][i].get("kind") == flk), None
        )
        if last is not None:
            ids = [i for i in ids if i >= last]
    kinds = payload.get("kinds")
    if isinstance(kinds, list) and kinds:
        ids = [i for i in ids if t.rows[owner][i].get("kind") in kinds]
    match = payload.get("match")
    if isinstance(match, dict) and match:
        ids = [
            i
            for i in ids
            if all(t.rows[owner][i].get(k) == v for k, v in match.items())
        ]
    want = payload.get("ids")
    if isinstance(want, list):
        wanted = {_as_id(x) for x in want}
        ids = [i for i in ids if i in wanted]
    after, before = _as_id(payload.get("after")), _as_id(payload.get("before"))
    desc = payload.get("order") == "desc"
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
    return {"rows": [t.get(owner, i) for i in page], "next": nxt, "total": total}


async def _search(id, payload, agent):
    """args: query:str (req — case-insensitive substring), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}."""
    q = str(payload.get("query") or "").strip().lower()
    if not q:
        return missing("search", "query")
    t = _tables(agent)
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    owner = who[0]
    ids = t.ids(owner)
    want = payload.get("ids")
    if isinstance(want, list) and want:
        scope: set[int] = set()
        for x in want:
            rid = _as_id(x)
            if rid is not None:
                scope |= _fold_closure(t, owner, rid)
        ids = [i for i in ids if i in scope]
    limit = _as_id(payload.get("limit")) or 100
    width = _as_id(payload.get("width")) or 200
    matches, total = [], 0
    for i in ids:
        if q not in t.text.get((owner, i), ""):
            continue
        for n, line in enumerate(_text_of(t.rows[owner][i]).splitlines(), 1):
            if q in line.lower():
                total += 1
                if len(matches) < limit:
                    matches.append({"id": i, "line": n, "text": line.strip()[:width]})
    return {"matches": matches, "total": total, "truncated": total > len(matches)}


async def _prune(id, payload, agent):
    """args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). Returns {pruned}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    owner = who[0]
    t = _tables(agent)
    before = _as_id(payload.get("before_id"))
    want = payload.get("ids")
    if before is None and not isinstance(want, list):
        return missing("prune", "before_id")
    keep = payload.get("keep") or []
    doomed: set[int] = set()
    for i in t.ids(owner):
        row = t.rows[owner][i]
        if row.get("kind") in keep:
            continue
        if before is not None and i < before:
            doomed.add(i)
        elif isinstance(want, list) and i in {_as_id(x) for x in want}:
            doomed.add(i)
    return {"pruned": t.delete(owner, doomed)}


async def _inject(id, payload, agent):
    """args: tables:dict (req — a `dump`). Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}."""
    t = load_tables(payload.get("tables"))
    if isinstance(t, dict):
        return t
    agent._ram_tables = t
    return {"rows": sum(len(v) for v in t.rows.values())}


async def _dump(id, payload, agent):
    """No args. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}."""
    return {"tables": _tables(agent).dump()}


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
        return refusal(UNHANDLED, f"ram_store: unknown type {t!r}", verb=t)
    return await fn(id, payload, agent)
