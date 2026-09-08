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
    scores calls and tokens needs the rows themselves). Over the wire it pages `recall`
    from the newest turn back until the store is exhausted."""
    if core is not None:
        return await core._load_history(aid, root)
    out: list[dict] = []
    before = None
    while True:
        payload = {"type": "recall", "limit": 100}
        if before is not None:
            payload["before"] = before
        r = await call(root, aid, payload)
        page = (r or {}).get("messages") or []
        if not page:
            break
        out = page + out
        before = page[0]["index"]
        if before == 0:
            break
    return out


async def plant_store(core, root, aid: str, rows: list[dict], bodies: dict[str, str] | None = None) -> None:
    """Write a conversation FROM ABOVE before the agent exists (or beside it): rows into
    the kernel's row store under the agent's id, dehydrated bodies under their pids. The
    agent need not exist yet — agent id is conversation id, and rows outlive the agent."""
    await core._save_history(aid, root, rows)
    for pid, body in (bodies or {}).items():
        await core._store_reply(aid, root, pid, body)


async def read_view(core, root, aid: str) -> dict | None:
    return await core._load_view(aid, root)


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
        "roots": sum(1 for m in msgs if m.get("role") == "user" and m.get("id") and not m.get("parent")),
        "view_ids": ((await call(root, aid, {"type": "context_status"})).get("last_projection") or {}).get("view_ids") or [],
        "view": (await read_view(core, root, aid)) if core is not None else None,
        "recall_rows": sum(1 for m in msgs if m.get("kind") == "recall"),
        "dehydrated_self": sum(1 for m in msgs if m.get("role") == "user" and f"· {aid} " in str(m.get("content")) and "NOT inlined" in str(m.get("content"))),
        # WHICH self-reply overflowed, so a failing run names the verb (kind, tag head)
        "dehydrated_self_tags": [f"{m.get('kind') or m.get('role')}: {str(m.get('content'))[:110]}" for m in msgs if m.get("role") == "user" and f"· {aid} " in str(m.get("content")) and "NOT inlined" in str(m.get("content"))][:3],
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
    folds = {m.get("id") for m in msgs if m.get("kind") == "fold"}

    def _i(x):  # the model may quote an id it copied out of a tag
        return int(x) if isinstance(x, str) and x.isdigit() else x

    out = []
    for c in ctx_score.calls_made(msgs, target=aid, verb="grep"):
        out += [_i(i) for i in (c["payload"].get("ids") or []) if _i(i) in folds]
    for c in ctx_score.calls_made(msgs, target=aid, verb="pull"):
        if _i(c["payload"].get("pid")) in folds:
            out.append(_i(c["payload"]["pid"]))
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
    import asyncio, time

    async def _go():
        end = time.monotonic() + max_wait
        while time.monotonic() < end:
            try:
                if await ready():
                    break
            except Exception:
                pass
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
    verifies by descending. In-process only."""
    await core._save_history(aid, root, [row])


async def copy_rows(root, src: str, dst: str) -> int:
    """FORK from above: the source's rows re-minted under the destination id, parents
    re-linked through the same map so the chains survive. Ids come from the kernel's one
    counter and the table is one dict — copying an id verbatim would clobber the
    original's row, so every copy is re-minted. In-process only."""
    ctx = root.ctx
    remap: dict[int, int] = {}
    # The CONVERSATION forks; the kernel's own bookkeeping (a `call` row per send, a
    # `task` row per fork) is the original's ledger and stays with it.
    for r in ctx.rows_for(src):
        if r.get("kind") in ("call", "task"):
            continue
        row = {k: v for k, v in r.items() if k not in ("id", "agent", "ts")}
        if row.get("parent") in remap:
            row["parent"] = remap[row["parent"]]
        if row.get("refs"):
            row["refs"] = [remap.get(x, x) for x in row["refs"]]
        remap[r["id"]] = ctx.rows_put(dst, row)
    return len(remap)


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
