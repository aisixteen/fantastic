"""THE MEMORY STORE, REACHED THE SAME WAY ON EVERY RUNTIME.

The kernel keeps no conversation: an AI agent's rows live in the MEMORY STORE its
record names (`memory_store_id`), and the kernel's own execution rows live in the
store the ROOT names. Every e2e kernel gets one shared `ram_store` — `memory` —
under the root, named by the root, and every AI agent the harness creates is wired
to it: `memory_store_id`, `can_call` on the store, the store's `callable_by` for
the agent. Explicit on both ends, as the design requires; nothing is defaulted.

A caller reads and appends only under its OWN key, so the harness reads another
agent's rows from the store's `dump` (the whole tables), and plants them in process
AS that agent (`_as_self`) and over the wire through `dump` → add → `inject`.

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


def store_record(parent_id: str, *, callable_by: dict) -> dict:
    """The `memory` ram_store record. Its rows are keyed by WHO writes them (the kernel
    writes its own as bookkeeping); who may reach it at all is its `callable_by`."""
    return {
        "id": MEMORY,
        "handler_module": "ram_store.tools",
        "parent_id": parent_id,
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
    lines.append(f"  memory record now: callable_by={(rec or {}).get('callable_by')!r}")
    led = (await root.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    for e in ((led.get("result") or {}).get("executions") or []):
        if MEMORY in repr(e.get("outcome")) or e.get("target") == MEMORY and e.get("status") != "completed":
            lines.append(f"  ledger: {e!r}"[:400])
    return "\n".join(lines)


async def rows_of(root: Any, aid: str, *, conversation_only: bool = False) -> list[dict]:
    """Every row of `aid`'s conversation, oldest first. Read from the store's `dump`
    (the whole tables): a caller reads only its OWN rows, so the harness — not `aid` —
    reads them there."""
    tables = await dump(root)
    rows = sorted(
        (x["row"] for x in tables.get("rows") or [] if _owned_by(x.get("owner"), aid)),
        key=lambda r: r.get("id") or 0,
    )
    if conversation_only:
        rows = [x for x in rows if x.get("kind") not in BOOKKEEPING]
    return [{**x, "agent": aid} for x in rows]


def _owned_by(owner: Any, aid: str) -> bool:
    """A row key is `<guid>|<agent>`."""
    return isinstance(owner, str) and owner.split("|", 1)[-1] == aid


async def plant(root: Any, aid: str, rows: list[dict]) -> list[int]:
    """Append rows to `aid`'s conversation. Returns the ids, in order.

    IN PROCESS: AS `aid` (`_as_self`) — a caller appends only under its own key.
    OVER THE WIRE the leg is not `aid`, so the tables are rewritten at setup (owner,
    2026-10-04): the leg appends one throwaway row per id-less row under ITS OWN key
    (the kernel stays the only allocator), then `dump` → those rows dropped, `rows`
    added under `<guid>|<aid>` with the minted ids (a row's `refs` become its
    `fold_refs`) → `inject`. The agent's in-memory head is not touched — plant before
    its first turn, as in process."""
    if getattr(root, "leg_id", None) is None:
        with root.ctx.agents[aid]._as_self():
            out = await root.send(
                [{"target": MEMORY, "payload": {"type": "append", "rows": rows}}]
            )
        r = out[0].get("result") or {}
        ids = r.get("ids")
        if not isinstance(ids, list):
            raise AssertionError(f"memory append({aid}) refused: {r}")
        return ids
    need = sum(1 for x in rows if x.get("id") is None)
    minted: list[int] = []
    if need:
        r = await _store(root, {"type": "append", "rows": [{"kind": "_mint"}] * need})
        minted = r.get("ids") if isinstance(r.get("ids"), list) else []
        if len(minted) != need:
            raise AssertionError(f"memory plant({aid}): minting {need} ids refused: {r}")
    out = await root.send([{"target": "kernel", "payload": {"type": "reflect", "tree": "none"}}])
    guid = (out[0].get("result") or {}).get("guid")
    if not isinstance(guid, str) or not guid:
        raise AssertionError(f"memory plant({aid}): the root reflect carries no guid: {out}")
    tables = await dump(root)
    leg_key, owner = f"{guid}|{root.leg_id}", f"{guid}|{aid}"
    gone = set(minted)
    tables["rows"] = [
        x for x in tables.get("rows") or []
        if not (x.get("owner") == leg_key and (x.get("row") or {}).get("id") in gone)
    ]
    fresh = iter(minted)
    ids: list[int] = []
    for x in rows:
        row = {k: v for k, v in x.items() if k != "refs"}
        row["id"] = row["id"] if row.get("id") is not None else next(fresh)
        ids.append(row["id"])
        tables["rows"].append({"owner": owner, "row": row})
        if isinstance(x.get("refs"), list):
            tables.setdefault("fold_refs", []).append(
                {"owner": owner, "fold": row["id"], "members": list(x["refs"])}
            )
    await inject(root, tables)
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
    views = [r for r in await rows_of(root, aid) if r.get("kind") == "view"]
    return views[-1] if views and isinstance(views[-1].get("ids"), list) else None


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
    (the store first, then the flusher)."""
    ctx = root.ctx
    if MEMORY not in ctx.agents:
        rec = root.create("ram_store.tools", id=MEMORY)
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
