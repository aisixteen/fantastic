"""The two-arm harness for fact-survival experiments: same fixture, same task, same
window, same model — `context_strategy` is the only variable.

Every probe that compares `halve`+facts against recency needs the same dance: create the
two file agents (`fb` and the `notes` editor) with both arms' ids on their ingress, create the human, run each arm to
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

from helpers import Limits, connect, ctx_score, drive, human, memory
from helpers.ai import Connected  # noqa: F401  (re-exported for probes)
from helpers.kernel import WRAPS, home

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
# the reply path. Large replies are held in RAM and digested to their origin line.
ASYNC_ARMS = (
    ("ai_recency", "truncate", {}),
    ("ai_async", "truncate", {"context_async": True, "context_titles": True}),
)
# FOLDS: closed chains collapse to marks the model can walk; then strip; then cut.
FOLD_ARMS = (
    ("ai_recency", "truncate", {}),
    ("ai_fold", "fold", {"context_async": True, "context_titles": True}),
)


async def call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def store_rows(root, core, aid: str) -> list[dict]:
    """The agent's conversation, for SCORING: its rows read from its MEMORY STORE (the
    store's dump — the one read every runtime answers), from the last `clear` on, turns
    only — what the agent's own head holds. ONE PATH (2026-10-11): in process this used
    to call `core._load_history` from outside the agent, so the store answered the read
    under the CALLER's key — empty before the agent's first turn (E75 counted 0 planted
    reads and told the model it had read nothing; E39 saw 0 folds)."""
    rows = await memory.rows_of(root, aid, conversation_only=True)
    last_clear = max(
        (i for i, r in enumerate(rows) if r.get("kind") == "clear"), default=None
    )
    if last_clear is not None:
        rows = rows[last_clear:]
    return [r for r in rows if r.get("role") in ("user", "assistant", "system")]


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
    # planted reply names a row that does not exist and `grep` / `history` find
    # nothing (which is exactly what the fold probes measure).
    c = out.get("content")
    # A FOLD ROW NAMES ITSELF in its head (`[fold <id> · …]`) — the id the model
    # descends by. Left as the placeholder, the model is pointed at a row that
    # does not exist (measured 2026-10-07, misleading_fold:rust: `[fold 1000000001 …]`).
    if isinstance(c, str) and c.startswith("[fold "):
        own = row.get("id")
        if isinstance(own, int) and own in seen:
            out["content"] = c.replace(f"[fold {own} ", f"[fold {seen[own]} ", 1)
    if isinstance(c, str) and c.startswith("[reply to "):
        try:
            old_pid = int(c[len("[reply to "):].split(" ", 1)[0])
        except (ValueError, IndexError):
            old_pid = None
        if old_pid is not None and old_pid in seen:
            out["content"] = c.replace(
                f"[reply to {old_pid} ", f"[reply to {seen[old_pid]} ", 1)
        # …and its `↑<call>` link: the turn that made the call this answers.
        c = out["content"]
        head, _, rest = c.partition("\n")
        for old, new in seen.items():
            head = head.replace(f"↑{old}]", f"↑{new}]").replace(f"↑{old} ", f"↑{new} ")
        out["content"] = head + ("\n" + rest if _ else "")
    return out


#: Where a planted past must land, as a share of the FOLD TRIGGER (`FOLD_AT` of the
#: agent's body budget — owner, 2026-10-11). Just over it, by default: exactly one fold
#: at boot. Well over it, for the tests whose subject is heavy compaction: several
#: folds, folds of folds.
FIT_ONE = (1.00, 1.10)
FIT_MANY = (1.30, 1.40)
#: The window each fitted agent runs at (`fit_planted` sets it; every arm-connect path
#: reads it): the arms' WINDOW unless the planted past was over its band.
FIT_WINDOW: dict[str, int] = {}


def arm_window(aid: str) -> int:
    return FIT_WINDOW.get(aid, WINDOW)


_WIRE_SYS: dict[str, int] = {}


def _wire_sys_tokens(aid: str) -> int:
    """The default system block's size for `aid`, measured by a throwaway in-process
    python kernel in a subprocess (the probe drives a remote kernel; booting a second
    one here would move its cwd and module state)."""
    if aid not in _WIRE_SYS:
        import subprocess
        import sys as _sys

        code = (
            "import asyncio, os, tempfile, sys\n"
            f"sys.path.insert(0, {str(Path(__file__).resolve().parents[2] / 'src/lib/python')!r})\n"
            "os.chdir(tempfile.mkdtemp(prefix='sysmeasure_'))\n"
            "from _testkit import boot_root\n"
            "from ai_core import core\n"
            "from ai_core.context import estimate_one\n"
            "root = boot_root()\n"
            f"t = asyncio.run(core._default_system_content({aid!r}, root))\n"
            "print(estimate_one({'role': 'system', 'content': t}))\n"
        )
        out = subprocess.run(
            [_sys.executable, "-c", code], capture_output=True, text=True, timeout=120
        )
        if out.returncode != 0 or not out.stdout.strip().splitlines():
            raise AssertionError(f"fit[{aid}]: measuring the system block failed: {out.stderr[-800:]}")
        _WIRE_SYS[aid] = int(out.stdout.strip().splitlines()[-1])
    return _WIRE_SYS[aid]


async def fit_planted(core, root, aid: str, rows: list[dict], *, window: int, band) -> None:
    """MEASURE the planted past against the fold trigger, and FIT it into `band`, in
    place: under it, neutral filler turns go in right after the goal (the oldest
    region, folded first); over it the plant fails loud — a probe's own rows are never
    dropped (needles live there). The measure is ai_core's own: the system block this
    agent will carry, the window's budget, the fold trigger, the rendered view.

    Rows are ordered by id, so a filler insert RE-MINTS every row in order and remaps
    every reference in place (`_remap_row`) — a probe holding a row dict keeps a valid
    id. Over the wire there is no in-process ai_core to measure with: said, not fitted."""
    from ai_core.constants import FOLD_AT
    from ai_core.context import estimate_one, estimate_tokens
    from ai_core.folds import render
    from ai_core.strategies.base import NOTICE_ENVELOPE_RESERVE

    if core is not None:
        sys_tok = estimate_one(
            {"role": "system", "content": await core._default_system_content(aid, root)}
        )
    else:
        # OVER THE WIRE (owner, 2026-10-11): the runtime's default block mirrors
        # python's (parity) — measured on a throwaway python kernel, once per run.
        from ai_core import core as pycore

        core = pycore
        sys_tok = _wire_sys_tokens(aid)
    body = core.budget({"context_window": window}) - sys_tok - NOTICE_ENVELOPE_RESERVE
    trigger = int(body * FOLD_AT)
    lo, hi = band

    def ratio() -> float:
        return estimate_tokens(render(rows)) / trigger

    before = ratio()
    if before > hi:
        # OVER: AUTOSIZE THE WINDOW (owner, 2026-10-11) — the planted content and its
        # needles stay whole; the window grows until the past sits mid-band.
        size = estimate_tokens(render(rows))
        want_body = int(size / (((lo + hi) / 2) * FOLD_AT))
        w = want_body + sys_tok + NOTICE_ENVELOPE_RESERVE
        while core.budget({"context_window": w}) - sys_tok - NOTICE_ENVELOPE_RESERVE < want_body:
            w += 256
        FIT_WINDOW[aid] = w
        trigger = int((core.budget({"context_window": w}) - sys_tok - NOTICE_ENVELOPE_RESERVE) * FOLD_AT)
        print(
            f"     fit[{aid}]: planted {before:.2f}× the fold trigger at window {window} "
            f"→ window {w}: {ratio():.2f}× (band {lo}–{hi}, system {sys_tok})",
            flush=True,
        )
        return
    FIT_WINDOW.pop(aid, None)
    added = 0
    if before < lo:
        goal = rows[0]
        fillers: list[dict] = []
        answers: dict[int, dict] = {}  # assistant filler → its user turn (by position)
        k = 0
        while (estimate_tokens(render(rows + fillers)) / trigger) < (lo + hi) / 2:
            u = {"role": "user", "id": 0, "parent": None,
                 "content": f"[from user]\nbackground note {k} (no action needed): "
                            + LOREM[: 600]}
            a = {"role": "assistant", "id": 0, "parent": None,
                 "content": 'Noted. <call name="done"></call>'}
            answers[id(a)] = u
            fillers += [u, a]
            k += 1
        ordered = [goal, *fillers, *rows[1:]]
        seen: dict[int, int] = {}
        for r in ordered:
            new = root.ctx.mint_id()
            if r.get("id"):
                seen[r["id"]] = new
            r["_new"] = new
        for r in ordered:
            remapped = _remap_row(r, seen)
            r.clear()
            r.update(remapped)
            r["id"] = r.pop("_new")
        for r in ordered:
            if (u := answers.get(id(r))) is not None:
                r["parent"] = u["id"]
        rows[:] = ordered
        added = len(fillers)
    print(
        f"     fit[{aid}]: planted {before:.2f}× the fold trigger ({trigger} tok; window "
        f"{window}, system {sys_tok}) → {ratio():.2f}× after +{added} filler rows "
        f"(band {lo}–{hi})",
        flush=True,
    )


async def plant_store(
    core, root, aid: str, rows: list[dict], *, band=FIT_ONE, window: int | None = None
) -> None:
    """Write a conversation FROM ABOVE before the agent exists (or beside it): rows into
    the agent's MEMORY STORE under its id (`memory.plant`). The agent need not
    exist yet — the store keys rows by owner, and rows outlive the agent. Reply BODIES
    are never persisted, so a planted history carries its replies inline.

    IN PROCESS the rows carry real ids (`root.ctx.mint_id`) and go down in one call.
    OVER THE WIRE the ids are PLACEHOLDERS (`WireCtx.mint_id`): `memory.plant` mints a
    real id for every row and remaps every reference (`_remap_row`) in ONE round.

    FITTED first (`fit_planted`, owner 2026-10-11): `band` against the fold trigger at
    `window` (default the arms' WINDOW); `band=None` plants as written."""
    if band is not None:
        await fit_planted(core, root, aid, rows, window=window or WINDOW, band=band)
    if core is not None:
        await memory.plant(root, aid, rows)
        return
    ids = await memory.plant(root, aid, rows, remap=_remap_row)
    for row, real in zip(rows, ids):
        # THE CALLER'S ROW LEARNS ITS REAL ID (a probe scores against it).
        row["id"] = real


async def read_view(core, root, aid: str) -> dict | None:
    """The agent's VIEW — the ordered ids it sees, each at a resolution: the newest
    `view` row in its memory store (in process: what `_load_view` reads)."""
    if core is not None:
        return await core._load_view(aid, root)
    return await memory.latest_view(root, aid)


async def connect_arm(root, core, bk, fb: str, aid: str, strategy: str, extra: dict):
    """`connect` with the arm's standard record — one place for the grant set."""
    return await connect(
        root, core, bk, agent_id=aid,
        can_call={fb: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
        record={"context_window": arm_window(aid), "num_ctx": arm_window(aid),
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
        WRAPS.pop(aid, None)
        core._projection.pop(aid, None)
        core._compaction_mark.pop(aid, None)
    return await connect(
        root, core, bk, agent_id=aid,
        can_call={fb: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
        record={"context_window": arm_window(aid), "num_ctx": arm_window(aid),
                "context_strategy": strategy, **extra},
    )


async def fixture(root, fb: str, *, arms=ARMS, extra_callable: tuple[str, ...] = ()):
    """The two file agents (`fb`, `notes`), both arms granted at CREATE time, then the human."""
    ids = [a for a, _, _ in arms] + list(extra_callable)
    for payload in (
        {
            "type": "create_agent", "parent_id": home(root), "id": fb,
            "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {**{i: "*" for i in ids}, "rest": "*", "web": "*"},
        },
        {
            "type": "create_agent", "parent_id": home(root), "id": "notes",
            "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {**{i: "*" for i in ids}, "rest": "*", "web": "*"},
        },
    ):
        r = await call(root, root.id, payload)
        assert not r.get("error"), f"create {payload['id']}: {r}"
    await human(root, ai=[a for a, _, _ in arms])


async def run_arm(root, core, bk, fb: str, aid: str, strategy: str, extra: dict,
                  task: str, artifact: str, *, via: str | None = "user",
                  window: int | None = None) -> dict:
    window = window or arm_window(aid)
    conn = await connect(
        root, core, bk, agent_id=aid,
        can_call={fb: "*", "notes": "*", "cli": "*", "user": "*", aid: "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
        record={"context_window": window, "num_ctx": window,
                "context_strategy": strategy, **extra},
    )
    try:
        await drive(conn, task, client_id=aid, via=via)
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
    views = [r for r in await memory.rows_of(root, aid) if r.get("kind") == "view"]
    return {
        # THE STARTUP VIEW: the first view the agent wrote — over a planted past, the
        # one its boot rebuild chose before the first live turn.
        "startup_compacted": startup_compacted(views[0] if views else None),
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
        "folds_opened": len([c for c in ctx_score.calls_made(msgs, target=aid, verb="history") if c["payload"].get("fold") is not None]),
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
    """Fold ids the model actually went INTO — a grep scoped to one, or `history {fold}`."""
    # A descent is a grep aimed at a fold — or at a LEAF the fold collapsed: the body
    # (pid) behind one of its member replies — or a `history {fold}` listing its
    # members. The model that reaches the member's body has gone through the fold.
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
    for c in ctx_score.calls_made(msgs, target=aid, verb="history"):
        if c["payload"].get("fold") is not None:
            out += _hit(_i(c["payload"].get("fold")))
    return out


def _pending(aid: str, coro):
    """Schedule a test's message to `aid`, PENDING until sent: the drive loop does not
    call the chain over while the test still has one to deliver (`helpers.ai.LATER`)."""
    import asyncio

    from helpers.ai import LATER

    task = asyncio.create_task(coro)
    LATER.setdefault(aid, set()).add(task)
    task.add_done_callback(lambda t: LATER.get(aid, set()).discard(t))
    return task


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
    return _pending(aid, _go())


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
        rows = await memory.rows_of(root, aid)
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
    return _pending(aid, _go())


async def send_in_turn(root, aid: str, texts: list[str], *, quiet: float = 1.0,
                       max_wait: float = 600.0, client_id: str | None = None):
    """The HUMAN sends each message as soon as the agent has FINISHED the one before —
    its own work was seen running, then stayed quiet for `quiet` s — so messages still
    arrive one after another, over time, without the idle a fixed timer leaves when a
    task ends early (measured 2026-10-07: flat_roots idled 603 of 1067 s, fold_paging
    525 of 782 s, on 20/22 s × i timers). The FIRST waits for the run's own first task to
    start and end. `max_wait` per message bounds a stalled agent: the message goes
    regardless. One pending task for the whole sequence (the drive loop keeps the chain
    open while it is pending)."""
    import asyncio
    import time

    from helpers.ai import _live_tasks

    async def _after_current() -> None:
        end = time.monotonic() + max_wait
        seen, quiet_since = False, None
        while time.monotonic() < end:
            if await _live_tasks(root, aid):
                seen, quiet_since = True, None
            elif seen:
                quiet_since = quiet_since or time.monotonic()
                if time.monotonic() - quiet_since >= quiet:
                    return
            await asyncio.sleep(0.25)

    async def _go():
        for text in texts:
            await _after_current()
            await call(root, "user", {"type": "send", "target": aid,
                                      "payload": {"type": "send", "text": text,
                                                  "client_id": client_id or aid}})
    return _pending(aid, _go())


async def send_payload_later(root, aid: str, payload: dict, delay: float):
    """The HUMAN calls a VERB on the agent mid-run — e.g. {type:'fold', all_closed:true}."""
    import asyncio

    async def _go():
        await asyncio.sleep(delay)
        return await call(root, "user", {"type": "send", "target": aid, "payload": payload})
    return _pending(aid, _go())


async def inject_row(core, root, aid: str, row: dict) -> None:
    """Append a row to the agent's OWN store from above — how a probe plants a
    MISLEADING fold (a summary that contradicts its members) to see if the model
    verifies by descending. In process the row keeps the id the probe minted; over
    the wire the store takes the kernel's next one, and everything the row REFERS TO
    (a fold's `refs`, its `parent`) stays as written — those are real ids on both
    paths."""
    if core is not None:
        await memory.plant(root, aid, [row])
        return
    ids = await memory.plant(root, aid, [row], remap=_remap_row)
    row["id"] = ids[0]  # the caller's row learns its real id (see plant_store)


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
            "greps": len(ctx_score.calls_made(msgs, target=aid, verb="grep"))}


# ─── A PRE-SEEDED PAST (owner, 2026-10-07: "we are testing folding") ───────────
#
# A fold test needs a past long enough to overflow the window. Producing it LIVE — the
# model reading N files one per turn — was most of every fold test's wall (fold_descent:
# 72 turns, 68 of them greps, because at a 6k window each filler is held and read
# blind). The past is PLANTED instead: the original instruction, then per file the
# model's `read` call and its reply with the text INLINE (bodies are never persisted;
# a planted history carries them inline). The LAST reply is delivered LIVE, as the
# reply a live run would have received, so the run continues that very chain: the
# folding, the recall and the answer are the model's own, over the same past.


def read_reply(path: str, text: str, call_id: int, *, target: str = "notes") -> str:
    """The reply a `read` of `path` comes back as — the tag names the call's pid (its
    message id) and the turn that made it (`↑`)."""
    return (f'[reply to {call_id} · {target} read · "read {path}" · ↑{call_id}]\n'
            f"<path>{path}</path>\n<text>{text}</text>")


def planted_reads(mint, goal: str, reads: list[tuple[str, str]], *, target: str = "notes",
                  tail: bool = True) -> list[dict]:
    """The past a live run would have produced: `goal` (the instruction, a root), then
    per `(path, text)` the model's `read` call and its reply. With `tail` the LAST
    reply is left out — `run_planted` delivers it live."""
    goal_row = {"role": "user", "id": mint(), "parent": None, "content": goal}
    rows, prev = [goal_row], goal_row["id"]
    for n, (path, text) in enumerate(reads):
        call_id = mint()
        rows.append({"role": "assistant", "id": call_id, "parent": prev,
                     "content": f'Reading {path}. <call name="send"><target_id>{target}</target_id>'
                                f"<title>read {path}</title><type>read</type><path>{path}</path></call>"})
        if tail and n == len(reads) - 1:
            break
        reply = {"role": "user", "id": mint(), "parent": call_id,
                 "content": read_reply(path, text, call_id, target=target)}
        rows.append(reply)
        prev = reply["id"]
    return rows


def closed_roots(mint, tasks: list[tuple[str, list[tuple[str, str]], str]], *,
                 target: str = "notes") -> list[dict]:
    """Finished roots, one after another: per `(goal, reads, answer)` the goal, its
    reads with their replies, then the model's `answer` ending in `done`."""
    rows: list[dict] = []
    for goal, reads, answer in tasks:
        chain = planted_reads(mint, goal, reads, target=target, tail=False)
        chain.append({"role": "assistant", "id": mint(), "parent": chain[-1]["id"],
                      "content": f'{answer} <call name="done"></call>'})
        rows += chain
    return rows


def file_reads(paths: list[str]) -> list[tuple[str, str]]:
    """`(path, text)` for files the probe wrote in its working dir."""
    return [(p, Path(p).read_text()) for p in paths]


async def run_planted(root, core, bk, fb: str, aid: str, strategy: str, extra: dict,
                      goal: str, reads: list[tuple[str, str]], artifact: str,
                      *, band=FIT_ONE) -> dict:
    """Plant the past for `aid`, then deliver the last read's reply live and run the
    chain to its end — `run_arm` over a planted past."""
    rows = planted_reads(root.ctx.mint_id, goal, reads)
    await plant_store(core, root, aid, rows, band=band)
    path, text = reads[-1]
    # rows[-1] is the last read CALL; its id is real now (plant_store learned it)
    tail = read_reply(path, text, rows[-1]["id"])
    # FROM THE OPERATOR: a reply carries no sender header, so its tag links it to the
    # call it answers and the chain continues (a send `via` an agent opens a root).
    return await run_arm(root, core, bk, fb, aid, strategy, extra, tail, artifact, via=None)


async def both_planted(root, core, bk, fb, goal, reads, artifact, *, arms=ARMS,
                       band=FIT_ONE) -> tuple[dict, dict]:
    out = []
    for aid, strategy, extra in arms:
        out.append(await run_planted(root, core, bk, fb, aid, strategy, extra, goal, reads,
                                     artifact, band=band))
    return out[0], out[1]


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
            f"     digested={a['digested']}  folds_opened={a['folds_opened']}  greps={a['greps']}  "
            f"folded={a['folded']}  levels={a['fold_levels']}  recalls={a['recall_rows']}  "
            f"self-replies dehydrated={a['dehydrated_self']}{(' ' + '; '.join(a['dehydrated_self_tags'])) if a['dehydrated_self'] else ''}  view={len((a.get('view') or {}).get('ids') or [])} rows  "
            f"descents={len(a['descents'])}  roots={a['roots']}\n"
            f"     dumps={a['dumps']}  carried={a['carried']}  failed={a['failed'] or 'no'}\n"
            f"     stored={[(f['kind'], f['text'][:50]) for f in a['stored']]}\n"
            f"     {score_line(a)}"
        )


def startup_compacted(view: dict | None) -> bool:
    """Did this view put the past at less than full resolution — a reply as a stub,
    a stretch as a fold?"""
    ids = (view or {}).get("ids")
    return isinstance(ids, list) and any(
        isinstance(e, dict) and e.get("res") in ("stub", "fold") for e in ids
    )


def base_checks(recency, other, *, planted: bool = False) -> list[tuple[str, bool]]:
    """The preconditions every comparison needs before its claim means anything. The
    mechanism proof is per strategy: facts must have been CARRIED, strip must have
    actually STRIPPED — otherwise the arm was recency wearing a different name.

    `planted`: the past was PLANTED, so each arm compacts it at startup (its boot
    rebuild: the fold arm folds, the control stubs) — measured there, not by a second
    compaction the short live tail may never reach (owner, 2026-10-07)."""
    ran = (
        other["carried"] > 0 if other["strategy"] == "halve"
        else (other["digested"] > 0 or other["folded"] > 0 or other["stripped"] > 0)
    )
    compacted = (
        ("both arms compacted the planted past at startup",
         recency["startup_compacted"] and other["startup_compacted"])
        if planted
        else ("both arms actually compacted", recency["cuts"] > 0 and other["cuts"] > 0)
    )
    return [
        compacted,
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
