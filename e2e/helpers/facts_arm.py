"""The two-arm harness for fact-survival experiments: same fixture, same task, same
window, same model — `context_strategy` is the only variable.

Every probe that compares `halve`+facts against recency needs the same dance: create the
bridge and editor with both arms' ids on their ingress, create the human, run each arm to
completion, rename its artifact out of the way, read its history and fact log. This is
that dance, once. A probe supplies the fixture, the task and the scorer.

THE CONTROL CONDITION IS "NOT CARRIED", NOT "NOT STORED". Measured 2026-09-04: the model
finds the `facts` verb without being taught it (the agent's own reflect lists its verbs)
and dumps anyway. That is a BETTER control, not contamination — both arms then behave the
same and the only difference is whether the strategy carries the dumps.
"""

from __future__ import annotations

import asyncio
import os
import re
from pathlib import Path

from helpers import Limits, connect, ctx_score, drive, human
from helpers.ai import Connected  # noqa: F401  (re-exported for probes)

# THE SQUEEZE. The model itself has 256k; this is the window the probes hand the agent so
# folds, strips and recalls fire inside a run. 12k (2026-09-05) starved the multi-root
# tests — 24 roots at 12k lost every deliverable, at 32k all three passed (2026-09-06).
# Fillers scale with it (`plant_fillers`), so a read still dehydrates at any setting.

def _self_stub(content: str, aid: str) -> bool:
    """Did a reply FROM `aid` ITSELF arrive as a stub?

    One user row can carry a whole batch: `[reply to N · <agent> <verb> · …]` tag lines,
    each followed by that reply's body. The old check looked for the stub prefix anywhere
    in the row's first 400 chars, so a SHORT inlined self-reply followed by some OTHER
    agent's dehydrated reply scored as a dehydrated self-reply (measured 2026-09-21,
    `recall_after_fold:rust`: an inlined 1-match grep + a 6950-byte `notes read` stub).
    Judge each reply by its own tag."""
    for seg in content.split("[reply to ")[1:]:
        tag, _, body = seg.partition("]\n")
        if f"· {aid} " in tag and body.startswith("(settled, "):
            return True
    return False

WINDOW = int(os.environ.get("E2E_WINDOW", "32000"))
SQUEEZE = WINDOW / 12000  # filler bodies are sized against the 12k baseline
LOREM = "the quick brown fox jumps over the lazy dog " * 150
# (agent id, context_strategy, extra record fields). The first arm is always the
# recency control; the second is the mechanism under test.
ARMS = (("ai_recency", "truncate", {}), ("ai_facts", "halve", {"context_facts": True}))
STRIP_ARMS = (("ai_recency", "truncate", {}), ("ai_strip", "strip", {"context_titles": True}))
# FULL ASYNC: the strategy is the plain recency baseline on BOTH sides — the variable is
# the reply path. Large replies are stored and digested; the model pulls pages.
ASYNC_ARMS = (
    ("ai_recency", "truncate", {}),
    ("ai_async", "truncate", {"context_async": True, "context_titles": True}),
)
# FOLDS: closed chains collapse to marks the model can walk; then strip; then cut.
FOLD_ARMS = (
    ("ai_recency", "truncate", {}),
    ("ai_fold", "fold", {"context_async": True, "context_titles": True}),
)


def leg(fb: str) -> dict:
    """The record fields that name a file leg — NONE on python (rows, bodies and the view
    live in the kernel), `file_bridge_id` on the runtimes that still failfast at `send`
    without one (rust `history.rs`, swift backend). Parity item one."""
    from helpers.kernel import runtime_name

    return {} if runtime_name() == "python" else {"file_bridge_id": fb}


async def call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def store_rows(root, core, aid: str) -> list[dict]:
    """The agent's whole durable store, for SCORING. In process it is read directly
    (`history` now answers at digest resolution, under the inline cap — a probe that
    scores calls and tokens needs the rows themselves). Over the wire it asks the
    kernel for the same rows (`rows_read`) — every runtime answers it; there is no
    second path (a `recall` fallback carried text without ids, and a probe that
    scores ROWS was silently scoring something else)."""
    if core is not None:
        return await core._load_history(aid, root)
    whole = await call(root, "kernel", {"type": "rows_read", "agent": aid, "limit": 200})
    assert isinstance(whole, dict) and "rows" in whole, f"rows_read refused: {whole}"
    out = list(whole.get("rows") or [])
    after = whole.get("next")
    while after is not None:
        whole = await call(
            root, "kernel", {"type": "rows_read", "agent": aid, "limit": 200, "after": after}
        )
        out += list((whole or {}).get("rows") or [])
        after = (whole or {}).get("next")
    # THE CONVERSATION, as `_load_history` reads it in process: `rows_read`
    # returns the agent's rows WHOLE — the kernel's own bookkeeping included
    # — so the wire path must hide the same kinds, or a probe counts a
    # planted `body` row as a fourth turn (measured 2026-09-12,
    # messages_gate:rust: `[13, 16, 19, 22]` against an expected 3).
    return [r for r in out if r.get("kind") not in _BOOKKEEPING]


#: The kernel's own bookkeeping kinds — in the same table, never the
#: conversation. What `_load_history` hides in process; the wire hides the same.
_BOOKKEEPING = ("call", "task", "body", "view", "settle")

#: Fields of a planted row that hold REFERENCES to other planted rows. A remap
#: has to know these by name: the kernel deliberately does not guess which of a
#: row's fields are ids, because guessing would corrupt a fold's `refs`.
_REF_FIELDS = ("parent",)
_REF_LIST_FIELDS = ("refs",)


def _remap_row(row: dict, seen: dict[int, int]) -> dict:
    """Replace placeholder ids with the ids the store assigned. An id that is not
    in `seen` passes through untouched — which is what makes this the IDENTITY on
    the in-process path, where the planter already holds real ids."""
    out = dict(row)
    out.pop("id", None)  # the store is the only allocator
    for f in _REF_FIELDS:
        v = out.get(f)
        if isinstance(v, int) and v in seen:
            out[f] = seen[v]
    for f in _REF_LIST_FIELDS:
        v = out.get(f)
        if isinstance(v, list):
            out[f] = [seen.get(x, x) if isinstance(x, int) else x for x in v]
    # The reply TAG carries the pid in its text — remap it there too, or a
    # planted reply names a row that does not exist and `grep`/`pull` find
    # nothing (which is exactly what the fold probes measure).
    c = out.get("content")
    if isinstance(c, str) and c.startswith("[reply to "):
        try:
            old_pid = int(c[len("[reply to "):].split(" ", 1)[0])
        except (ValueError, IndexError):
            old_pid = None
        if old_pid is not None and old_pid in seen:
            out["content"] = c.replace(
                f"[reply to {old_pid} ", f"[reply to {seen[old_pid]} ", 1)
    return out


async def plant_store(core, root, aid: str, rows: list[dict], bodies: dict[str, str] | None = None) -> None:
    """Write a conversation FROM ABOVE before the agent exists (or beside it): rows into
    the kernel's row store under the agent's id, dehydrated bodies under their pids. The
    agent need not exist yet — agent id is conversation id, and rows outlive the agent.

    IN PROCESS the rows already carry real ids and go down in one call. OVER THE
    WIRE they cannot: `rows_write` refuses a caller-chosen id, so the rows go one
    at a time IN DEPENDENCY ORDER and every reference is remapped to the id the
    store just assigned. Every reference a planted history holds points BACKWARD
    (a reply to its call, a fold to rows it collapsed, a turn to its parent), so
    one forward pass is enough.
    """
    if core is not None:
        await core._save_history(aid, root, rows)
        for pid, body in (bodies or {}).items():
            await core._store_reply(aid, root, pid, body)
        return
    seen: dict[int, int] = {}
    for row in rows:
        placeholder = row.get("id")
        got = await call(root, "kernel", {
            "type": "rows_write", "agent": aid, "rows": [_remap_row(row, seen)]})
        ids = (got or {}).get("ids")
        assert isinstance(ids, list) and ids, f"rows_write refused a planted row: {got}"
        if isinstance(placeholder, int):
            seen[placeholder] = ids[0]
        # THE CALLER'S ROW LEARNS ITS REAL ID. A probe scores against the id it
        # planted (`planted["id"] in store`); on the wire that was a placeholder
        # and could never match (measured 2026-09-12, misleading_fold:rust).
        row["id"] = ids[0]
    # THE BODIES, under the pids the store actually assigned.
    for pid, body in (bodies or {}).items():
        real = seen.get(pid, pid)
        got = await call(root, "kernel", {
            "type": "rows_write", "agent": aid,
            "rows": [{"kind": "body", "parent": real, "content": body}]})
        assert (got or {}).get("ids"), f"rows_write refused a planted body: {got}"


async def read_view(core, root, aid: str) -> dict | None:
    """The agent's VIEW — the ordered ids it sees, each at a resolution.

    In process that is `_load_view`. Over the wire the view IS a row (`kind:
    "view"`), so the newest one in the agent's store is the same document —
    `rows_read` carries rows whole. No verb exposes it directly and none should
    be added for a probe's convenience."""
    if core is not None:
        return await core._load_view(aid, root)
    whole = await call(root, "kernel", {"type": "rows_read", "agent": aid})
    latest = None
    for r in (whole or {}).get("rows") or []:
        if r.get("kind") == "view" and isinstance(r.get("ids"), list):
            latest = r
    return latest


async def connect_arm(root, core, bk, fb: str, aid: str, strategy: str, extra: dict):
    """`connect` with the arm's standard record — one place for the grant set."""
    return await connect(
        root, core, bk, agent_id=aid,
        can_call={fb: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
        record={**leg(fb), "context_window": WINDOW, "num_ctx": WINDOW,
                "context_strategy": strategy, **extra},
    )


async def reboot_agent(root, core, bk, fb: str, aid: str, strategy: str, extra: dict, *, watch_conn=None):
    """Kill the agent's PROCESS state and bring it back on the same store: close the
    connection, `delete_agent` (the record goes — rows, bodies and the view stay in the
    kernel under the same id by construction), forget the provider, then
    `connect` again with the same record. `create_agent` fires `boot`, and a booted
    agent with a store runs `context_recall`. Returns the new connection."""
    if watch_conn is not None:
        await watch_conn.close()
    r = await call(root, root.id, {"type": "delete_agent", "id": aid})
    assert not (isinstance(r, dict) and r.get("error")), f"delete_agent({aid}): {r}"
    if core is not None:
        core._providers.pop(aid, None)
        core._projection.pop(aid, None)
        core._compaction_mark.pop(aid, None)
    return await connect(
        root, core, bk, agent_id=aid,
        can_call={fb: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
        record={**leg(fb), "context_window": WINDOW, "num_ctx": WINDOW,
                "context_strategy": strategy, **extra},
    )


async def fixture(root, fb: str, *, arms=ARMS, extra_callable: tuple[str, ...] = ()):
    """Bridge + editor, both arms granted at CREATE time, then the human."""
    ids = [a for a, _, _ in arms] + list(extra_callable)
    for payload in (
        {
            "type": "create_agent", "parent_id": root.id, "id": fb,
            "handler_module": "file_bridge.tools", "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {**{i: "*" for i in ids}, "notes": "*", "rest": "*", "web": "*"},
        },
        {
            "type": "create_agent", "parent_id": root.id, "id": "notes",
            "handler_module": "text_file.tools", "target_id": fb,
            "can_call": {fb: "*"},
            "callable_by": {**{i: "*" for i in ids}, "rest": "*", "web": "*"},
        },
    ):
        r = await call(root, root.id, payload)
        assert not r.get("error"), f"create {payload['id']}: {r}"
    await human(root, ai=[a for a, _, _ in arms])


async def run_arm(root, core, bk, fb: str, aid: str, strategy: str, extra: dict,
                  task: str, artifact: str) -> dict:
    conn = await connect(
        root, core, bk, agent_id=aid,
        can_call={fb: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
        record={**leg(fb), "context_window": WINDOW, "num_ctx": WINDOW,
                "context_strategy": strategy, **extra},
    )
    try:
        await drive(conn, task, client_id=aid, via="user")
    finally:
        await conn.close()
    msgs = await store_rows(root, core, aid)
    # THE WRITE IS DETACHED. `done` closes the turn, and the calls that turn made run
    # in a spawned task after it — so the artifact can land a moment after `drive`
    # returns. Measured 2026-09-06 (pull_by_choice): the transcript showed
    # "report.md written with checksum: …" and the file read as empty. Wait for it.
    p = Path(artifact)
    for _ in range(40):
        if p.exists() and p.read_text().strip():
            break
        await asyncio.sleep(0.25)
    body = p.read_text() if p.exists() else ""
    if p.exists():
        p.rename(f"{aid}__{artifact}")
    stored = (await call(root, aid, {"type": "facts", "op": "list"})).get("facts") or []
    return {
        "id": aid, "strategy": strategy, "body": body, "msgs": msgs,
        "turns": conn.turns, "cuts": len(conn.compactions),
        "dropped": ctx_score.dropped_total(conn),
        "carried": max((d.get("pinned_facts") or 0) for d in conn.compactions)
        if conn.compactions else 0,
        "stripped": max((d.get("stripped") or 0) for d in conn.compactions)
        if conn.compactions else 0,
        "stripped_bytes": max((d.get("stripped_bytes") or 0) for d in conn.compactions)
        if conn.compactions else 0,
        # TITLE COVERAGE: how many of the model's sends carried a title. The whole
        # mechanism rests on this; a lazy model makes every stub blank.
        "titled": sum(1 for c in ctx_score.calls_made(msgs) if c.get("title")),
        "sends": len(ctx_score.calls_made(msgs)),
        "ledger_calls": len(ctx_score.calls_made(msgs, verb="ledger")),
        "pulls": len(ctx_score.calls_made(msgs, target=aid, verb="pull")),
        "pulled": [c["payload"].get("pid") for c in ctx_score.calls_made(msgs, target=aid, verb="pull")],
        "greps": len(ctx_score.calls_made(msgs, target=aid, verb="grep")),
        "folded": sum(int(d.get("folded") or 0) for d in conn.compactions),
        "fold_ids": [m.get("id") for m in msgs if m.get("kind") == "fold"],
        "descents": _descents(msgs, aid),
        # a ROOT: a message from a person or a peer — it now OPENS with the kernel's
        # `[from … ]` header (2026-09-10), so "starts with [" is no longer the test;
        # the runtime's own lines (reply tags, nudges, notices, folds) are named.
        "roots": sum(
            1
            for m in msgs
            if m.get("role") == "user"
            and m.get("id")
            and not m.get("parent")
            and not m.get("kind")
            and not str(m.get("content", "")).startswith(
                ("[reply to ", "[no call", "[context-notice", "[fold ", "[recall", "[late reply")
            )
        ),
        "view_ids": ((await call(root, aid, {"type": "context_status"})).get("last_projection") or {}).get("view_ids") or [],
        "view": (await read_view(core, root, aid)) if core is not None else None,
        "recall_rows": sum(1 for m in msgs if m.get("kind") == "recall"),
        # a self-reply that came back as a STUB: the stub prefix right after the tag line —
        # not the phrase anywhere (a grep reply quoting another row's stub is not one)
        "dehydrated_self": sum(1 for m in msgs if m.get("role") == "user" and _self_stub(str(m.get("content")), aid)),
        # WHICH self-reply overflowed, so a failing run names the verb (kind, tag head)
        "dehydrated_self_tags": [f"{m.get('kind') or m.get('role')}: {str(m.get('content'))[:110]}" for m in msgs if m.get("role") == "user" and _self_stub(str(m.get("content")), aid)][:3],
        "fold_levels": sorted({int(m.get("level") or 1) for m in msgs if m.get("kind") == "fold"}),
        "digested": sum(
            1 for m in msgs if m.get("role") == "user"
            and "NOT inlined" in str(m.get("content"))),
        "stored": stored,
        "dumps": len(ctx_score.calls_made(msgs, target=aid, verb="facts")),
        "failed": [t for t in conn.trips if "failed" in t],
    }


def _descents(msgs, aid) -> list[str]:
    """Fold ids the model actually went INTO — a grep scoped to one, or a pull of one."""
    # A descent is a grep/pull aimed at a fold — or at a LEAF the fold collapsed: the
    # body (pid) behind one of its member replies. The model that pulls the member's
    # body has gone through the fold to the file, which is the point.
    folds = {m.get("id") for m in msgs if m.get("kind") == "fold"}
    # leaf pid → the fold ids whose members that pid's reply row belongs to
    by_row = {m.get("id"): m for m in msgs if m.get("id") is not None}
    leaf_to_folds: dict = {}
    for f in msgs:
        if f.get("kind") != "fold":
            continue
        for r in f.get("refs") or []:
            m = by_row.get(r)
            if not m or m.get("role") != "user":
                continue
            head = str(m.get("content", "")).split("\n", 1)[0]
            tok = head[len("[reply to "):].split(" ")[0] if head.startswith("[reply to ") else ""
            if tok.isdigit():
                leaf_to_folds.setdefault(int(tok), []).append(f["id"])

    def _i(x):  # the model may quote an id it copied out of a tag
        return int(x) if isinstance(x, str) and x.isdigit() else x

    def _hit(i):  # a fold id names itself; a leaf pid names every fold above it
        if i in folds:
            return [i]
        return list(leaf_to_folds.get(i, []))

    out = []
    for c in ctx_score.calls_made(msgs, target=aid, verb="grep"):
        for i in c["payload"].get("ids") or []:
            out += _hit(_i(i))
    for c in ctx_score.calls_made(msgs, target=aid, verb="pull"):
        out += _hit(_i(c["payload"].get("pid")))
    return out


async def send_later(root, aid: str, text: str, delay: float, *, client_id: str | None = None):
    """The HUMAN sends another message while the agent is mid-run. It queues behind the
    agent's FIFO lock and starts a new ROOT when the current turn ends — which is how two
    chains come to interleave across turns."""
    import asyncio

    async def _go():
        await asyncio.sleep(delay)
        await call(root, "user", {"type": "send", "target": aid,
                                  "payload": {"type": "send", "text": text,
                                              "client_id": client_id or aid}})
    return asyncio.create_task(_go())


def done_calls(rows: list[dict]) -> int:
    """How many times the agent has closed a task — `done` envelopes in its own turns."""
    return sum(1 for m in rows if m.get("role") == "assistant" and '"name":"done"' in re.sub(r"\s", "", str(m.get("content"))))


def after_replies(root, aid: str, n: int = 1):
    """Readiness for a SECOND ROOT: the agent has received at least `n` delivered
    replies in its current run, i.e. it is mid-task. A fixed delay was tried
    (`send_later(…, 45.0)`, 2026-09-08) and raced the model: under the tag grammar
    the fold arm finished a four-read task in six turns, the chain ended before 45 s,
    the later send was cancelled, and the second root never existed (two_roots_live,
    2026-09-09)."""

    async def ready() -> bool:
        rows = root.ctx.rows_for(aid)
        return (
            sum(
                1
                for r in rows
                if r.get("role") == "user"
                and str(r.get("content") or "").startswith("[reply to ")
            )
            >= n
        )

    return ready


def after_tasks(root, aid: str, laters: list):
    """Readiness for a FINAL: every earlier task was DELIVERED and the agent has gone
    quiet (no in-flight task of its own). Polled every few seconds, so it lands well
    inside the driver's 30 s idle cap and restarts the chain instead of missing it. A
    done-count was tried first (2026-09-06) and never fired: a model that batches or
    skips `done` per task keeps the count short and the FINAL arrived after the run."""
    from helpers.ai import _live_tasks

    async def ready() -> bool:
        return all(t.done() for t in laters) and not await _live_tasks(root, aid)

    return ready


async def send_when(root, core, aid: str, text: str, ready, *, max_wait: float, poll: float = 5.0):
    """The HUMAN sends a message ONCE THE WORK IT REFERS TO EXISTS (`ready()` true), or
    at `max_wait` regardless (so a stalled agent still meets the message and the run
    ends). A FINAL on a fixed timer measured 2026-09-06 as landing before the tasks it
    summarized: the model then chose between an unfinished task and a FINAL that told
    it to stop — folds_x_chains wrote 1 of 4 reports and called done. The tasks
    themselves stay on timers; interleaving is what those probes measure."""
    import asyncio
    import time

    async def _go():
        end = time.monotonic() + max_wait
        while time.monotonic() < end:
            # a readiness predicate that raises is a probe bug — let it surface
            if await ready():
                break
            await asyncio.sleep(poll)
        await call(root, "user", {"type": "send", "target": aid,
                                  "payload": {"type": "send", "text": text, "client_id": aid}})
    return asyncio.create_task(_go())


async def send_payload_later(root, aid: str, payload: dict, delay: float):
    """The HUMAN calls a VERB on the agent mid-run — e.g. {type:'fold', all_closed:true}."""
    import asyncio

    async def _go():
        await asyncio.sleep(delay)
        return await call(root, "user", {"type": "send", "target": aid, "payload": payload})
    return asyncio.create_task(_go())


async def inject_row(core, root, aid: str, row: dict) -> None:
    """Append a row to the agent's OWN store from above — how a probe plants a
    MISLEADING fold (a summary that contradicts its members) to see if the model
    verifies by descending.

    In process the row goes down under the id the probe minted. Over the wire
    the store mints: `rows_write` refuses a caller-chosen id, so the row's own
    `id` is dropped and everything it REFERS TO (a fold's `refs`, its `parent`)
    stays as written — those name rows the running agent already holds, which
    are real ids on both paths."""
    if core is not None:
        await core._save_history(aid, root, [row])
        return
    got = await call(root, "kernel", {
        "type": "rows_write", "agent": aid, "rows": [_remap_row(row, {})]})
    ids = (got or {}).get("ids")
    assert ids, f"rows_write refused the injected row: {got}"
    row["id"] = ids[0]  # the caller's row learns its real id (see plant_store)


async def copy_rows(root, src: str, dst: str) -> int:
    """FORK from above: the source's rows re-minted under the destination id, parents
    re-linked through the same map so the chains survive. Ids come from the kernel's one
    counter and the table is one dict — copying an id verbatim would clobber the
    original's row, so every copy is re-minted.

    IN PROCESS BY DEFAULT (`root.ctx`), over the wire through the kernel's own
    `rows_read`/`rows_write` when there is no ctx to reach. The two paths do the
    SAME arithmetic — the wire one just asks the kernel for what the in-process
    one takes directly — so a fork means the same thing on every runtime."""
    remap: dict[int, int] = {}

    def forked(r: dict) -> dict | None:
        """One source row as its copy, or None when it is not the conversation.
        The kernel's own bookkeeping (a `call` row per send, a `task` row per
        fork, a `body`, a `view`) is the ORIGINAL's ledger and stays with it —
        bodies and views resolve through ids the copy does not have."""
        if r.get("kind") in ("call", "task", "body", "view"):
            return None
        row = {k: v for k, v in r.items() if k not in ("id", "agent", "ts")}
        if row.get("parent") in remap:
            row["parent"] = remap[row["parent"]]
        if row.get("refs"):
            row["refs"] = [remap.get(x, x) for x in row["refs"]]
        return row

    # `rows_put` is the IN-PROCESS allocator — it takes a pre-minted id, which
    # the wire's `rows_write` refuses by design. Its presence is what says this
    # is the in-process kernel.
    ctx = getattr(root, "ctx", None)
    if getattr(ctx, "rows_put", None) is not None:
        for r in ctx.rows_for(src):
            row = forked(r)
            if row is not None:
                remap[r["id"]] = ctx.rows_put(dst, row)
        return len(remap)

    # ONE ROW PER CALL, because a parent inside the same batch has no new id
    # until its copy lands — the remap is built as it goes, exactly as above.
    for r in await _wire_rows(root, src):
        row = forked(r)
        if row is None:
            continue
        out = await call(root, "kernel", {"type": "rows_write", "agent": dst, "rows": [row]})
        ids = (out or {}).get("ids") or []
        assert ids, f"rows_write refused a fork row: {out}"
        remap[r["id"]] = ids[0]
    return len(remap)


async def _wire_rows(root, aid: str) -> list[dict]:
    """An agent's rows WHOLE, over the wire — the kernel's `rows_read`, paged."""
    out: list[dict] = []
    after = None
    while True:
        payload = {"type": "rows_read", "agent": aid, "limit": 200}
        if after is not None:
            payload["after"] = after
        r = await call(root, "kernel", payload)
        page = (r or {}).get("rows") or []
        out += page
        after = (r or {}).get("next")
        if after is None:
            break
    return out


async def continue_arm(root, core, aid: str, task: str, artifact: str, *, timeout: float = 300.0) -> dict:
    """A SECOND task for an agent that already ran: sent by the human, no new connect.
    Waits for the artifact (the chain runs detached), then reads the store the same way
    `run_arm` does. For probes whose second phase must see the first phase's folds."""
    import asyncio

    await call(root, "user", {"type": "send", "target": aid,
                              "payload": {"type": "send", "text": task, "client_id": aid}})
    p = Path(artifact)
    for _ in range(int(timeout / 0.5)):
        if p.exists() and p.read_text().strip():
            break
        await asyncio.sleep(0.5)
    msgs = await store_rows(root, core, aid)
    body = p.read_text() if p.exists() else ""
    return {"id": aid, "body": body, "msgs": msgs, "descents": _descents(msgs, aid),
            "greps": len(ctx_score.calls_made(msgs, target=aid, verb="grep")),
            "pulls": len(ctx_score.calls_made(msgs, target=aid, verb="pull"))}


async def both_arms(root, core, bk, fb, task, artifact, *, arms=ARMS) -> tuple[dict, dict]:
    out = []
    for aid, strategy, extra in arms:
        out.append(await run_arm(root, core, bk, fb, aid, strategy, extra, task, artifact))
    return out[0], out[1]


def report(arms, score_line):
    for a in arms:
        print(
            f"\n  ── {a['id']} ({a['strategy']}) ──\n"
            f"     turns={a['turns']}  cuts={a['cuts']} (dropped {a['dropped']})  "
            f"stripped={a['stripped']} ({a['stripped_bytes']}B)  titled={a['titled']}/{a['sends']}  "
            f"ledger calls={a['ledger_calls']}\n"
            f"     digested={a['digested']}  pulls={a['pulls']}  greps={a['greps']}  "
            f"folded={a['folded']}  levels={a['fold_levels']}  recalls={a['recall_rows']}  "
            f"self-replies dehydrated={a['dehydrated_self']}{(' ' + '; '.join(a['dehydrated_self_tags'])) if a['dehydrated_self'] else ''}  view={len((a.get('view') or {}).get('ids') or [])} rows  "
            f"descents={len(a['descents'])}  roots={a['roots']}\n"
            f"     dumps={a['dumps']}  carried={a['carried']}  failed={a['failed'] or 'no'}\n"
            f"     stored={[(f['kind'], f['text'][:50]) for f in a['stored']]}\n"
            f"     {score_line(a)}"
        )


def base_checks(recency, other) -> list[tuple[str, bool]]:
    """The preconditions every comparison needs before its claim means anything. The
    mechanism proof is per strategy: facts must have been CARRIED, strip must have
    actually STRIPPED — otherwise the arm was recency wearing a different name."""
    ran = (
        other["carried"] > 0 if other["strategy"] == "halve"
        else (other["digested"] > 0 or other["folded"] > 0 or other["stripped"] > 0)
    )
    return [
        ("both arms actually compacted", recency["cuts"] > 0 and other["cuts"] > 0),
        ("the control carried no facts and stripped nothing",
         recency["carried"] == 0 and recency["stripped"] == 0),
        (f"the {other['strategy']} arm's mechanism actually ran", ran),
        ("both arms produced the artifact",
         bool(recency["body"].strip()) and bool(other["body"].strip())),
    ]


def conclude(recency, other, checks, ok_note: str, fail_note: str) -> tuple[bool, str]:
    """CONTROL DIED is not MECHANISM FAILED. The verdict is the mechanism arm's checks; the
    control's fate is printed beside it. Three FAILs on 2026-09-06 were controls killed by
    the unbounded ledger while the mechanism under test had passed every check.
    Returns (allok, note) — the probe calls `verdict` itself."""
    control_ok = bool(recency["body"].strip()) and not recency["failed"]
    for name, ok in checks:
        print(f"  [{'ok' if ok else 'XX'}] {name}")
    why = recency["failed"][0][:80] if recency["failed"] else "no artifact"
    print(f"  control: {'ran clean' if control_ok else 'DIED — ' + why}")
    allok = all(ok for _, ok in checks)
    return allok, (ok_note + ("" if control_ok else " (control died)") if allok else fail_note)


# ── shared fixture pieces ─────────────────────────────────────────────────────

def plant_fillers(n: int, *, marker_in: int | None = None, marker: str = "",
                  extra: dict[int, str] | None = None, pad_to: int = 6800) -> None:
    """N fillers padded to ONE length, so size never leaks which one matters. A marker
    (or any `extra` text) goes mid-body — where no head or tail preview reaches."""
    from pathlib import Path

    for i in range(n):
        body = f"chunk {i}: {LOREM}"
        ins = (extra or {}).get(i) or (f">> audit marker: {marker}" if i == marker_in else "")
        if ins:
            h = len(LOREM) // 2
            body = f"chunk {i}: {LOREM[:h]}\n{ins}\n{LOREM[h:]}"
        body = body + "." * max(0, int(pad_to * SQUEEZE) - len(body))
        Path(f"filler_{i}.txt").write_text(body + "\n")


def rules_for(tok: dict, keys: tuple[str, ...]):
    return ([("its FIRST line must be exactly", f"## {tok['first']}")]
            + [("it must contain a line reading exactly", f"{k}: {tok[k]}") for k in keys]
            + [("its LAST line must be exactly", f"{tok['last']}-end")])


def score_rules(body: str, rules) -> tuple[int, list[str]]:
    lines = [ln.strip() for ln in body.strip().splitlines()]
    lost = []
    for i, (_, ln) in enumerate(rules):
        ok = (bool(lines) and lines[0] == ln) if i == 0 else \
             (bool(lines) and lines[-1] == ln) if i == len(rules) - 1 else ln in lines
        if not ok:
            lost.append(ln)
    return len(rules) - len(lost), lost
