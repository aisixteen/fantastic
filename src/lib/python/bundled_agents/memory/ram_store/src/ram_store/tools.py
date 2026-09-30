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
  grants     PRIMARY KEY gid                     {owner, reader, scope, mode,
                                                  anchor, as_of, until, why, …}
             FK (owner, anchor) → rows ON DELETE CASCADE
             UNIQUE (owner, reader, scope, mode) an identical share is one grant
             INDEX owner, INDEX reader
  text_idx   (owner, id) → lowercased text       the FTS5 counterpart; derived

ACCESS, nothing implicit:
  - any caller: its OWN key (append / read / search / prune / share / unshare /
    grants), plus rows another owner GRANTED it (`read`/`search` with `grant`).
  - `operator: true` on the record serves the operator at all (as `kernel`).
  - `admins: [ids]` (`"kernel"` = the operator) may name another `owner` and may
    `inject` / `dump` the whole store.
The kernel's call gate still decides who may reach this agent at all.

Ids are the KERNEL's: a row carries the int the kernel minted, or none and this
store takes the next one from the kernel's counter — never an id of its own.

Everything here dies with the process. That is what `ram` says.
"""

from __future__ import annotations

import bisect
import json
import time

from kernel import current_origin, current_remote_caller, current_task
from kernel._refusal import (
    BAD_ARG,
    EXISTS,
    NOT_FOUND,
    UNAUTHORIZED,
    UNHANDLED,
    missing,
    refusal,
)

OPERATOR = "kernel"
MODES = ("frozen", "live")
SCOPE_KINDS = ("head_until", "range", "fold", "row")
# kinds whose `refs` name members (the fold_refs table): a fold, a clear-fold.
REF_KINDS = ("fold", "clear")


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
        self.grants: dict[int, dict] = {}
        self.grant_seq = 0
        self.grant_by_anchor: dict[tuple[str, int], set[int]] = {}
        self.grant_key: dict[tuple, int] = {}
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

    def delete(self, owner: str, doomed: set[int]) -> tuple[int, int]:
        """DELETE FROM rows WHERE owner=? AND id IN doomed — with every cascade.
        Returns (rows removed, grants removed)."""
        table = self.rows.get(owner, {})
        doomed = {r for r in doomed if r in table}
        grants_gone = 0
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
            # FK grants.anchor → rows: a grant anchored here is gone.
            for gid in list(self.grant_by_anchor.pop((owner, rid), set())):
                if self._drop_grant(gid):
                    grants_gone += 1
        if doomed:
            self.order[owner] = [
                i for i in self.order.get(owner, []) if i not in doomed
            ]
        return len(doomed), grants_gone

    # ── grants ───────────────────────────────────────────────────────
    def add_grant(self, g: dict) -> tuple[int, bool]:
        key = (
            g["owner"],
            g["reader"],
            json.dumps(g["scope"], sort_keys=True),
            g["mode"],
        )
        if key in self.grant_key:
            return self.grant_key[key], False
        self.grant_seq += 1
        gid = self.grant_seq
        g = {**g, "grant": gid}
        self.grants[gid] = g
        self.grant_key[key] = gid
        self.grant_by_anchor.setdefault((g["owner"], g["anchor"]), set()).add(gid)
        return gid, True

    def _drop_grant(self, gid: int) -> bool:
        g = self.grants.pop(gid, None)
        if g is None:
            return False
        key = (
            g["owner"],
            g["reader"],
            json.dumps(g["scope"], sort_keys=True),
            g["mode"],
        )
        self.grant_key.pop(key, None)
        s = self.grant_by_anchor.get((g["owner"], g["anchor"]))
        if s is not None:
            s.discard(gid)
            if not s:
                del self.grant_by_anchor[(g["owner"], g["anchor"])]
        return True

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
            "grants": [dict(g) for _, g in sorted(self.grants.items())],
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
    for g in tables.get("grants") or []:
        if not isinstance(g, dict) or not t.has(g.get("owner"), g.get("anchor")):
            return refusal(
                BAD_ARG, "inject: a grant's anchor names no row", arg="tables"
            )
        gid = _as_id(g.get("grant"))
        if gid is None:
            return refusal(
                BAD_ARG, "inject: a grant needs an int `grant`", arg="tables"
            )
        body = {k: v for k, v in g.items() if k != "grant"}
        key = (
            body["owner"],
            body["reader"],
            json.dumps(body["scope"], sort_keys=True),
            body["mode"],
        )
        t.grants[gid] = {**body, "grant": gid}
        t.grant_key[key] = gid
        t.grant_by_anchor.setdefault((body["owner"], body["anchor"]), set()).add(gid)
        t.grant_seq = max(t.grant_seq, gid)
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


def _key(agent, name: str) -> str | None:
    """An owner/reader NAME as a table key: a qualified `guid|id` verbatim, a bare
    local id under this kernel's guid."""
    if not isinstance(name, str) or not name.strip():
        return None
    name = name.strip()
    if "|" in name:
        return name
    g = _guid(agent)
    return f"{g}|{name}" if g else None


def _display(agent, key: str) -> str:
    """A key as a caller reads it: the bare id for this kernel, qualified otherwise."""
    g = _guid(agent)
    if g and key.startswith(g + "|"):
        return key[len(g) + 1 :]
    return key


def _caller(agent) -> tuple[str, str] | dict:
    """(key, display) for whoever is calling, or a refusal."""
    remote = current_remote_caller()
    if remote:
        return remote, remote
    g = _guid(agent)
    if not g:
        return refusal(
            NOT_FOUND,
            "ram_store: this kernel has no guid yet — the store keys every row by it",
            hint="boot the kernel root first (it mints the guid)",
        )
    origin = current_origin()
    if origin is None:
        if not agent.record.get("operator"):
            return refusal(
                UNAUTHORIZED,
                f"{agent.id}: the operator is not served here",
                hint=f"update_agent id={agent.id} operator=true to serve the operator (as 'kernel')",
                side="store",
            )
        return f"{g}|{OPERATOR}", OPERATOR
    return f"{g}|{origin}", origin


def _is_admin(agent, display: str) -> bool:
    admins = agent.record.get("admins") or []
    if isinstance(admins, str):
        admins = [admins]
    return display in admins


def _owner_for(agent, payload: dict, verb: str) -> tuple[str, str, str] | dict:
    """(owner key, caller key, caller display) for a verb that acts on ONE owner:
    the caller's own key, or `owner` when the caller is an admin."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    me, display = who
    name = payload.get("owner")
    if name is None:
        return me, me, display
    owner = _key(agent, name)
    if owner is None:
        return refusal(BAD_ARG, f"{verb}: `owner` must be an agent id", arg="owner")
    if owner != me and not _is_admin(agent, display):
        return refusal(
            UNAUTHORIZED,
            f"{verb}: {display!r} may act only on its own rows, not {name!r}",
            hint=f"a caller writes and reads its OWN key; another owner's rows are "
            f"reached through a grant it gives (`share`), or by an agent listed in "
            f"{agent.id}.admins",
            side="store",
        )
    return owner, me, display


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


def _scope_ids(t: Tables, g: dict) -> set[int]:
    """The rows a grant reaches, as of its bound (frozen) or following the head (live)."""
    owner, scope, live = g["owner"], g["scope"], g["mode"] == "live"
    ids = t.ids(owner)
    if "row" in scope:
        base = {scope["row"]} & set(ids)
    elif "fold" in scope:
        base = _fold_closure(t, owner, scope["fold"]) & set(ids)
    elif "range" in scope:
        lo, hi = scope["range"]["from"], scope["range"]["to"]
        base = {i for i in ids if i >= lo and (live or i <= hi)}
    else:
        n = scope["head_until"]
        base = {i for i in ids if live or i <= n}
    if not live and g.get("as_of") is not None:
        base = {i for i in base if i <= g["as_of"]}
    return base


def _parse_scope(t: Tables, owner: str, scope) -> tuple[dict, int] | dict:
    """(normalised scope, anchor) — the anchor is the scope's LOWEST existing row, so a
    prune that touches any part of the scope removes it (FK cascade)."""
    if (
        not isinstance(scope, dict)
        or len(scope) != 1
        or next(iter(scope)) not in SCOPE_KINDS
    ):
        return refusal(
            BAD_ARG,
            "share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, "
            "{fold: <id>}, {row: <id>} — a descriptor, never a list of messages",
            arg="scope",
        )
    kind, val = next(iter(scope.items()))
    ids = t.ids(owner)
    if kind == "range":
        if not isinstance(val, dict):
            return refusal(BAD_ARG, "share: range must be {from, to}", arg="scope")
        lo, hi = _as_id(val.get("from")), _as_id(val.get("to"))
        if lo is None or hi is None or lo > hi:
            return refusal(
                BAD_ARG, "share: range needs from <= to (row ids)", arg="scope"
            )
        inside = [i for i in ids if lo <= i <= hi]
        if not inside:
            return refusal(
                NOT_FOUND, f"share: none of YOUR rows lie in {lo}..{hi}", arg="scope"
            )
        return {"range": {"from": lo, "to": hi}}, inside[0]
    rid = _as_id(val)
    if rid is None:
        return refusal(BAD_ARG, f"share: {kind} must be a row id", arg="scope")
    if kind == "head_until":
        if not ids or ids[0] > rid:
            return refusal(
                NOT_FOUND, f"share: you have no rows up to {rid}", arg="scope"
            )
        return {"head_until": rid}, ids[0]
    if not t.has(owner, rid):
        # ZERO TOLERANCE: a row that is not under YOUR key is not yours to share —
        # a foreign row, or one shared WITH you, simply is not found here.
        return refusal(
            NOT_FOUND,
            f"share: row {rid} is not one of YOUR rows — only rows you own can be shared",
            arg="scope",
        )
    if kind == "fold" and (t.rows[owner][rid].get("kind") not in REF_KINDS):
        return refusal(BAD_ARG, f"share: row {rid} is not a fold", arg="scope")
    return {kind: rid}, rid


def _grant_view(agent, g: dict) -> dict:
    return {
        "grant": g["grant"],
        "owner": _display(agent, g["owner"]),
        "reader": _display(agent, g["reader"]),
        "scope": g["scope"],
        "mode": g["mode"],
        "as_of": g.get("as_of"),
        "until": g.get("until"),
        "why": g.get("why"),
        "created_by": g.get("created_by"),
        "ts": g.get("ts"),
    }


def _readable(
    agent, t: Tables, payload: dict, verb: str
) -> tuple[str, set[int] | None, dict | None] | dict:
    """(owner key, allowed ids | None = all, grant | None) for a read-side verb:
    `grant` reads another owner's scope; else the caller's own key (or `owner`, admin)."""
    gid = payload.get("grant")
    if gid is None:
        o = _owner_for(agent, payload, verb)
        if isinstance(o, dict):
            return o
        return o[0], None, None
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    me, display = who
    g = t.grants.get(_as_id(gid) or -1)
    if g is None:
        return refusal(
            NOT_FOUND,
            f"{verb}: grant {gid!r} does not exist — revoked (unshare) or its rows were pruned",
            arg="grant",
        )
    if g["reader"] != me and not _is_admin(agent, display):
        return refusal(
            UNAUTHORIZED,
            f"{verb}: grant {gid} was given to {_display(agent, g['reader'])!r}, not {display!r}",
            side="store",
        )
    return g["owner"], _scope_ids(t, g), g


# ─── verbs ─────────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + posture: how many owners/rows/grants, who is admin, whether the operator is served, which optional verbs exist. No args."""
    t = _tables(agent)
    return {
        "id": id,
        "sentence": "Memory store in RAM — rows per owner (the caller), folds, grants, "
        "search; ephemeral: everything is lost when the process exits.",
        "durable": False,
        "owners": len([o for o in t.rows if t.rows[o]]),
        "rows": sum(len(v) for v in t.rows.values()),
        "grants": len(t.grants),
        "operator": bool(agent.record.get("operator")),
        "admins": agent.record.get("admins") or [],
        "optional_verbs": ["search"],
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, agent):
    """No-op — the tables start empty (RAM) or are filled by `inject`."""
    return None


async def _append(id, payload, agent):
    """args: rows:list[dict] (req), owner:str? (admins only). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}."""
    o = _owner_for(agent, payload, "append")
    if isinstance(o, dict):
        return o
    owner = o[0]
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
    """args: grant:int? (read another owner's scope), owner:str? (admins), after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}."""
    t = _tables(agent)
    r = _readable(agent, t, payload, "read")
    if isinstance(r, dict):
        return r
    owner, allowed, _g = r
    ids = list(t.ids(owner))
    if allowed is not None:
        ids = [i for i in ids if i in allowed]
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
    """args: query:str (req — case-insensitive substring), grant:int?, owner:str? (admins), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}."""
    q = str(payload.get("query") or "").strip().lower()
    if not q:
        return missing("search", "query")
    t = _tables(agent)
    r = _readable(agent, t, payload, "search")
    if isinstance(r, dict):
        return r
    owner, allowed, _g = r
    ids = t.ids(owner)
    if allowed is not None:
        ids = [i for i in ids if i in allowed]
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


async def _share(id, payload, agent):
    """args: with:str (req — the reader's agent id), scope:dict (req — ONE of {head_until:<row>}, {range:{from,to}}, {fold:<id>}, {row:<id>}), mode:'frozen'|'live'? (default frozen), why:str?. Grants `with` read access to that scope of YOUR rows — only rows you own (never a foreign row, never one shared with you). An identical share returns the existing grant. Returns {grant, anchor, as_of, created}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    me, display = who
    reader = _key(agent, payload.get("with"))
    if reader is None:
        return missing("share", "with")
    if reader == me:
        return refusal(BAD_ARG, "share: you already read your own rows", arg="with")
    mode = payload.get("mode") or "frozen"
    if mode not in MODES:
        return refusal(BAD_ARG, f"share: mode must be one of {MODES}", arg="mode")
    t = _tables(agent)
    parsed = _parse_scope(t, me, payload.get("scope"))
    if isinstance(parsed, dict) and "reason" in parsed:
        return parsed
    scope, anchor = parsed
    ids = t.ids(me)
    gid, created = t.add_grant(
        {
            "owner": me,
            "reader": reader,
            "scope": scope,
            "mode": mode,
            "anchor": anchor,
            "as_of": ids[-1] if mode == "frozen" and ids else None,
            "until": None,  # reserved: a row limit / time bound — unused
            "why": " ".join(str(payload.get("why") or "").split())[:300] or None,
            "created_by": current_task(),
            "ts": time.time(),
        }
    )
    g = t.grants[gid]
    return {"grant": gid, "anchor": anchor, "as_of": g.get("as_of"), "created": created}


async def _unshare(id, payload, agent):
    """args: grant:int (req). Revokes a grant YOU gave (admins: any). The reader's next read under it fails. Returns {unshared:true, grant}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    me, display = who
    gid = _as_id(payload.get("grant"))
    if gid is None:
        return missing("unshare", "grant")
    t = _tables(agent)
    g = t.grants.get(gid)
    if g is None:
        return refusal(NOT_FOUND, f"unshare: no grant {gid}", arg="grant")
    if g["owner"] != me and not _is_admin(agent, display):
        return refusal(
            UNAUTHORIZED, f"unshare: grant {gid} is not yours to revoke", side="store"
        )
    t._drop_grant(gid)
    return {"unshared": True, "grant": gid}


async def _grants(id, payload, agent):
    """args: side:'given'|'received' (req). The grants YOU gave, or the ones given TO you. Returns {grants:[{grant, owner, reader, scope, mode, as_of, until, why, created_by, ts}]}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    me, _display_ = who
    side = payload.get("side")
    if side not in ("given", "received"):
        return refusal(
            BAD_ARG, "grants: side must be 'given' or 'received'", arg="side"
        )
    field = "owner" if side == "given" else "reader"
    t = _tables(agent)
    return {
        "grants": [
            _grant_view(agent, g) for _, g in sorted(t.grants.items()) if g[field] == me
        ]
    }


async def _prune(id, payload, agent):
    """args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept), owner:str? (admins). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref and every grant anchored on a deleted row goes with it (FK cascade). Returns {pruned, grants_removed}."""
    o = _owner_for(agent, payload, "prune")
    if isinstance(o, dict):
        return o
    owner = o[0]
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
    n, g = t.delete(owner, doomed)
    return {"pruned": n, "grants_removed": g}


async def _inject(id, payload, agent):
    """args: tables:dict (req — a `dump`). ADMINS ONLY. Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows, grants}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    if not _is_admin(agent, who[1]):
        return refusal(
            UNAUTHORIZED, f"inject: {who[1]!r} is not an admin of {id}", side="store"
        )
    t = load_tables(payload.get("tables"))
    if isinstance(t, dict):
        return t
    agent._ram_tables = t
    return {"rows": sum(len(v) for v in t.rows.values()), "grants": len(t.grants)}


async def _dump(id, payload, agent):
    """No args. ADMINS ONLY. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}], grants:[…]} — exactly what `inject` takes back. Returns {tables}."""
    who = _caller(agent)
    if isinstance(who, dict):
        return who
    if not _is_admin(agent, who[1]):
        return refusal(
            UNAUTHORIZED, f"dump: {who[1]!r} is not an admin of {id}", side="store"
        )
    return {"tables": _tables(agent).dump()}


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "append": _append,
    "read": _read,
    "search": _search,
    "share": _share,
    "unshare": _unshare,
    "grants": _grants,
    "prune": _prune,
    "inject": _inject,
    "dump": _dump,
}


def validate_meta(meta: dict, record: dict) -> str | None:
    """`operator` is a bool; `admins` a list of ids."""
    if "operator" in meta and not isinstance(meta["operator"], bool):
        return "operator must be true or false"
    if "admins" in meta:
        a = meta["admins"]
        if isinstance(a, str):
            return None
        if not isinstance(a, list) or not all(isinstance(x, str) and x for x in a):
            return "admins must be a list of agent ids ('kernel' = the operator)"
    return None


async def handler(id: str, payload: dict, agent) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"ram_store: unknown type {t!r}", verb=t)
    return await fn(id, payload, agent)
