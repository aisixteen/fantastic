"""THE MEMORY STORE, REACHED THE SAME WAY ON EVERY RUNTIME.

The kernel keeps no conversation: an AI agent's rows live in the MEMORY STORE its
record names (`memory_store_id`), and the kernel's own execution rows live in the
store the ROOT names. Every e2e kernel gets one shared `ram_store` — `memory` —
under the root, named by the root, and every AI agent the harness creates is wired
to it: `memory_store_id`, `can_call` on the store, the store's `callable_by` for
the agent. Explicit on both ends, as the design requires; nothing is defaulted.

The harness reads and plants rows as a store ADMIN (`admins` on the store record:
`kernel` = the operator in process, `rest`/`ws` = the driver's legs over the wire),
naming the `owner` — which is exactly what an admin may do and nobody else may.

A ram_store dies with its process. A restart test carries the tables across:
`dump` before the stop, `inject` after the new kernel is up — what a durable store
(sqlite, the CLI project) does by itself.
"""

from __future__ import annotations

from typing import Any

#: The one store id every e2e kernel carries.
MEMORY = "memory"

#: The store verbs an AI agent uses on its own store (its conversation + sharing).
AI_STORE_VERBS = "*"

#: Rows in an agent's store that are not conversation.
BOOKKEEPING = ("view",)


def store_record(parent_id: str, *, admins: list[str], callable_by: dict) -> dict:
    """The `memory` ram_store record: serves the operator (the kernel's own rows),
    admins as given, ingress as given."""
    return {
        "id": MEMORY,
        "handler_module": "ram_store.tools",
        "parent_id": parent_id,
        "operator": True,
        "admins": list(admins),
        "callable_by": callable_by,
    }


async def _store(root: Any, payload: dict) -> dict:
    out = await root.send([{"target": MEMORY, "payload": payload}])
    r = out[0].get("result")
    return r if isinstance(r, dict) else {}


async def _grant_forensics(root: Any) -> str:
    """What the store's gate looked like when a read was refused, and every call
    THIS driver made that touched the store — so a grant that vanished mid-run
    (rust E30, 2026-09-30: `rest` gone from `memory`'s callable_by, the model
    never having touched it) leaves evidence instead of a bare refusal."""
    lines = []
    r = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]
    rec = next(
        (a for a in ((r.get("result") or {}).get("agents") or []) if a.get("id") == MEMORY),
        None,
    )
    lines.append(f"  memory record now: callable_by={(rec or {}).get('callable_by')!r} "
                 f"admins={(rec or {}).get('admins')!r}")
    led = (await root.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    for e in ((led.get("result") or {}).get("executions") or []):
        if MEMORY in repr(e.get("outcome")) or e.get("target") == MEMORY and e.get("status") != "completed":
            lines.append(f"  ledger: {e!r}"[:400])
    return "\n".join(lines)


async def rows_of(root: Any, aid: str, *, conversation_only: bool = False) -> list[dict]:
    """Every row of `aid`'s conversation, oldest first (all pages)."""
    rows: list[dict] = []
    after = None
    while True:
        q = {"type": "read", "owner": aid, "limit": 1000}
        if after is not None:
            q["after"] = after
        r = await _store(root, q)
        if r.get("error"):
            raise AssertionError(
                f"memory read({aid}) refused: {r}\n" + await _grant_forensics(root)
            )
        rows.extend(r.get("rows") or [])
        after = r.get("next")
        if after is None:
            break
    if conversation_only:
        rows = [x for x in rows if x.get("kind") not in BOOKKEEPING]
    return [{**x, "agent": aid} for x in rows]


async def plant(root: Any, aid: str, rows: list[dict]) -> list[int]:
    """Append rows to `aid`'s conversation — ids from the kernel's counter when a row
    carries none. Returns the ids, in order."""
    r = await _store(root, {"type": "append", "owner": aid, "rows": rows})
    ids = r.get("ids")
    if not isinstance(ids, list):
        raise AssertionError(f"memory append({aid}) refused: {r}")
    return ids


async def dump(root: Any) -> dict:
    r = await _store(root, {"type": "dump"})
    if "tables" not in r:
        raise AssertionError(f"memory dump refused: {r}")
    return r["tables"]


async def inject(root: Any, tables: dict) -> dict:
    r = await _store(root, {"type": "inject", "tables": tables})
    if r.get("error"):
        raise AssertionError(f"memory inject refused: {r}")
    return r


async def latest_view(root: Any, aid: str) -> dict | None:
    r = await _store(
        root,
        {"type": "read", "owner": aid, "kinds": ["view"], "order": "desc", "limit": 1},
    )
    rows = r.get("rows") or []
    return rows[0] if rows and isinstance(rows[0].get("ids"), list) else None


async def open_ingress(root: Any, target: str, caller: str) -> None:
    """Add `caller` to `target`'s `callable_by` — READ, add one key, write back
    (update replaces the field whole)."""
    r = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0][
        "result"
    ]
    rec = next((a for a in (r or {}).get("agents") or [] if a.get("id") == target), None)
    if rec is None:
        raise AssertionError(f"open_ingress: no agent {target!r}")
    cb = dict(rec.get("callable_by") or {})
    if cb.get(caller) == "*":
        return
    cb[caller] = "*"
    out = (
        await root.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "update_agent", "id": target, "callable_by": cb},
                }
            ]
        )
    )[0]["result"]
    if isinstance(out, dict) and out.get("error"):
        raise AssertionError(f"open_ingress({target} ← {caller}) refused: {out}")


def ai_memory_meta(can_call: dict) -> dict:
    """The create_agent fields an AI agent needs for its memory: the store's id and
    egress on it (merged into `can_call`). The store's ingress is opened separately
    (`open_ingress`), once the agent exists."""
    return {"memory_store_id": MEMORY, "can_call": {**can_call, MEMORY: AI_STORE_VERBS}}


async def wire_in_process(root: Any) -> None:
    """In-process kernel: create `memory` under the root, name it, START the kernel
    (the store first, then the flusher). The operator is admin (`kernel`)."""
    ctx = root.ctx
    if MEMORY not in ctx.agents:
        rec = root.create(
            "ram_store.tools", id=MEMORY, operator=True, admins=["kernel"]
        )
        assert rec.get("id") == MEMORY, rec
    ctx.update(root.id, memory_store_id=MEMORY)
    out = await ctx.start(boot_all=False)
    if not out.get("started"):
        raise SystemExit(f"kernel_start refused: {out}")


async def watch_store_writers(port: int, root_id: str) -> None:
    """Print WHO rewrites `memory`'s grants, and to what — every `update_agent`
    naming the store, read off the kernel's state stream with its `sender` (the
    poll in `watch_store_grants` says WHEN a grant changed, not by whom; E30 lost
    `rest` from the store twice with no writer in the builder's transcript,
    2026-09-30). Run as a background task; cancel to stop."""
    import time

    from helpers.host.ws import StateWatcher

    t0 = time.monotonic()
    w = StateWatcher(port, root_id)
    try:
        await w.connect()
        async for ev in w.state_events():
            p = ev.get("payload") or {}
            if (
                ev.get("kind") == "send"
                and isinstance(p, dict)
                and p.get("type") == "update_agent"
                and p.get("id") == MEMORY
            ):
                cb = p.get("callable_by")
                keys = sorted(cb) if isinstance(cb, dict) else cb
                print(
                    f"  [grants] t={time.monotonic() - t0:6.1f}s update_agent memory "
                    f"from sender={ev.get('sender')!r} via {ev.get('agent_id')!r}: "
                    f"fields={sorted(k for k in p if k not in ('type', 'id'))} "
                    f"callable_by={keys}",
                    flush=True,
                )
    except Exception as e:  # noqa: BLE001 — a forensic probe must not kill the run
        print(f"  [grants] writer watch unavailable: {e}", flush=True)
    finally:
        await w.close()


async def watch_store_grants(root: Any, *, every: float = 5.0) -> None:
    """Print `memory`'s callable_by every time it CHANGES, with the elapsed time —
    run as a background task for the length of a run, so a grant that disappears
    is pinned to the moment (and the transcript turn) it went. Cancel to stop."""
    import asyncio
    import time

    t0 = time.monotonic()
    last = None
    while True:
        try:
            r = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]
            rec = next(
                (a for a in ((r.get("result") or {}).get("agents") or []) if a.get("id") == MEMORY),
                None,
            )
            cb = sorted(((rec or {}).get("callable_by") or {}).keys())
            if cb != last:
                print(f"  [grants] t={time.monotonic() - t0:6.1f}s memory.callable_by = {cb}", flush=True)
                last = cb
        except Exception as e:  # noqa: BLE001 — a forensic probe must not kill the run
            print(f"  [grants] watch error: {e}", flush=True)
        await asyncio.sleep(every)
