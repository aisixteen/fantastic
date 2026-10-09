"""scheduler bundle — recurring tasks as an agent.

State (sidecars in `.fantastic/agents/{id}/`, persisted THROUGH the loader):
  schedules.yaml         — a YAML sequence of {id, target, payload, interval_seconds, next_run, paused, run_count, timeout?}
  schedule_events.yaml   — a YAML STREAM, one `---` document per schedule_fired event,
                           ring-trimmed to HISTORY_MAX
  Both are written by `kernel_state.yaml_io.dump` — the one emitter config the
  kernel's own files use (block style, indent 2, key order kept).

There is NOTHING to wire — the scheduler persists through `kernel_state` (the loader),
which owns the `.fantastic` store. A write failfasts only when no store is wired at the
root (read-empty until then); a denied/failed write is surfaced, not lost.

Verbs:
  reflect      -> {sentence, paused, running}
  boot         -> start the fire loop (idempotent)
  schedule     args: target, payload, interval_seconds, timeout?  -> {schedule_id}
  unschedule   args: schedule_id                        -> {removed: bool}
  list                                                  -> {schedules: [...]}
  pause        args: schedule_id?                       -> pause one or all
  resume       args: schedule_id?                       -> resume one or all
  tick_now     args: schedule_id                        -> fire immediately
  history      args: limit?, schedule_id?              -> {history: [...]}

A SCHEDULE FIRES AS THE SCHEDULER ITSELF: every fire goes out under the
scheduler's own id, so the gate judges the SCHEDULER's `can_call` and the
target's `callable_by` for the scheduler. Every verb — list, history, tick_now,
unschedule, pause, resume — sees every schedule.

NO IDLE WAKEUPS (owner, 2026-10-08): the fire loop sleeps until the EARLIEST due
schedule, or — none due, or the scheduler paused — until something changes: a
schedule added, removed, paused or resumed, or the scheduler's own record updated
(`paused`). An idle scheduler wakes never.

A tick collects every DUE schedule, bumps `run_count`/`next_run`, persists FIRST
(a crash inside the fire window cannot re-fire on reboot), then fires them all —
ONE `kernel.send([{target, payload, timeout?}, ...])` batch, the items concurrent,
so one hung target never blocks the others. `timeout` is the schedule's own per-fire
bound (seconds); absent = wait forever (the kernel never defaults it). After
firing, scheduler emits one `{type:"schedule_fired", ...}` per outcome to its OWN
inbox only — the target gets the PAYLOAD, which is the tick; echoing telemetry
at it doubled every delivery.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import secrets
import time

import yaml
from kernel_state import yaml_io
from kernel_state.tools import load_blob, persist_blob

from kernel._refusal import UNHANDLED, bad, refusal

logger = logging.getLogger(__name__)

HISTORY_MAX = 500

_tick_tasks: dict[str, asyncio.Task] = {}
_cache: dict[str, list[dict]] = {}  # in-process schedules cache
# Per scheduler: what wakes its fire loop (a change), and the state tap that
# sets it when the scheduler's own record is updated.
_wake: dict[str, asyncio.Event] = {}
_unsub: dict[str, object] = {}


def _poke(sid: str) -> None:
    """Something the fire loop waits on changed: recompute when to wake."""
    ev = _wake.get(sid)
    if ev is not None:
        ev.set()


# ─── argument typing ─────────────────────────────────────────────


def _int_arg(payload: dict, key: str, default: int, verb: str, minimum: int):
    """ABSENT ⇒ `default`. Present, a plain int, ≥ `minimum` ⇒ that value.
    ANYTHING ELSE ⇒ a `bad_arg` refusal.

    A present-but-wrong number is a TYPO, and both old behaviours hid it: `int()`
    raised out of the handler (`{"raised": true}` — no `arg`, nothing to branch
    on), while a bare `or default` turned `interval_seconds: "60"` into a silent
    hourly schedule the caller believed was per-minute. `True` is an int in
    python and is rejected explicitly — `interval_seconds: true` would otherwise
    schedule once a second."""
    if key not in payload:
        return default
    v = payload[key]
    if isinstance(v, bool) or not isinstance(v, int) or v < minimum:
        want = "a non-negative int" if minimum == 0 else f"an int >= {minimum}"
        return bad(verb, key, want)
    return v


# ─── file routing ────────────────────────────────────────────────


# Sidecar filenames — kernel_state computes the dir (`agents/<id>/`) under the one
# `.fantastic` store it owns. The scheduler wires NOTHING; it persists THROUGH the loader.
_SCHED_NAME = "schedules.yaml"
_HISTORY_NAME = "schedule_events.yaml"


def _loader(kernel):
    """The root loader the sidecars persist through, or None when the root is not
    a `kernel_state` (then a write fails loud and a read is empty)."""
    root = kernel.ctx.root
    if root is None or root.handler_module != "kernel_state.tools":
        return None
    return root


async def _file_read(sid: str, kernel, name: str) -> str | None:
    """Read a sidecar THROUGH the loader. Missing / no-store ⇒ None (reads lenient).
    Persistence through the loader is bookkeeping: a direct call of the loader's
    `load_blob`, never a gated send."""
    loader = _loader(kernel)
    if loader is None:
        return None
    r = await load_blob(loader, sid, name)
    return r.get("content") if isinstance(r, dict) else None


async def _file_write(sid: str, kernel, name: str, content: str) -> dict | None:
    """Write a sidecar THROUGH the loader (a direct `persist_blob` call — see
    `_file_read`). Returns an error dict on a denied/no-store write (no silent
    loss), or None on success."""
    loader = _loader(kernel)
    if loader is None:
        return {"error": "scheduler: the root is not a kernel_state loader — no store"}
    w = await persist_blob(loader, sid, name, content)
    if isinstance(w, dict) and w.get("error"):
        out = {"error": w["error"]}
        if w.get("reason"):
            out["reason"] = w["reason"]
        return out
    return None


# ─── schedules persistence (through the loader) ─────────────────


async def _load(sid: str, kernel) -> list[dict]:
    if sid in _cache:
        return _cache[sid]
    raw = await _file_read(sid, kernel, _SCHED_NAME)
    # FIRST FILL WINS: a verb that loaded and wrote while this read was in flight
    # already holds the cache — the read must not replace it with the older list.
    if not raw:
        return _cache.setdefault(sid, [])
    try:
        doc = yaml_io.load(raw)
    except yaml.YAMLError:
        doc = None
    return _cache.setdefault(sid, doc if isinstance(doc, list) else [])


async def _save(sid: str, kernel) -> dict | None:
    """Persist the cache THROUGH the loader; returns an error dict on failure."""
    return await _file_write(
        sid, kernel, _SCHED_NAME, yaml_io.dump(_cache.get(sid, []))
    )


# ─── history persistence (through the loader — read-modify-write) ──


async def _append_history(sid: str, kernel, event: dict) -> dict | None:
    raw = await _file_read(sid, kernel, _HISTORY_NAME) or ""
    appended = raw + yaml_io.row_to_doc(event)
    # Ring-trim if oversize: keep the newest HISTORY_MAX documents.
    events = _parse_events(appended)
    if len(events) > 2 * HISTORY_MAX:
        appended = "".join(yaml_io.row_to_doc(e) for e in events[-HISTORY_MAX:])
    return await _file_write(sid, kernel, _HISTORY_NAME, appended)


def _parse_events(raw: str) -> list[dict]:
    """The event stream's documents that are mappings. A malformed stream reads
    empty (reads are lenient, as they were for the JSON-lines file)."""
    try:
        docs = list(yaml.load_all(raw, Loader=yaml.CSafeLoader))
    except yaml.YAMLError:
        return []
    return [d for d in docs if isinstance(d, dict)]


async def _read_history(sid: str, kernel, limit: int) -> list[dict]:
    raw = await _file_read(sid, kernel, _HISTORY_NAME)
    if not raw:
        return []
    return _parse_events(raw)[-limit:]


# ─── tick loop ──────────────────────────────────────────────────


async def _tick_loop(sid: str, kernel) -> None:
    """Fire what is due, then sleep until the earliest next fire — or, with nothing
    to wait for, until a change (`_poke`). Never on a fixed tick."""
    wake = _wake.setdefault(sid, asyncio.Event())
    try:
        while True:
            wake.clear()
            rec = kernel.get(sid)
            if rec is None:
                return
            schedules = await _load(sid, kernel)
            delay = None
            if not rec.get("paused"):
                now = time.time()
                due = [
                    sch
                    for sch in list(schedules)
                    if not sch.get("paused") and now >= sch.get("next_run", 0)
                ]
                if due:
                    # Persist BEFORE the fire: run_count/next_run are bumped and
                    # saved first, so a crash inside the fire window does not
                    # re-fire the same tick on reboot (at-most-once).
                    for sch in due:
                        sch["run_count"] = sch.get("run_count", 0) + 1
                        sch["next_run"] = now + sch["interval_seconds"]
                    if err := await _save(sid, kernel):
                        logger.warning(
                            "scheduler %s: persist failed: %s", sid, err.get("error")
                        )
                    await _fire_many(sid, due, kernel)
                    continue
                pending = [
                    sch.get("next_run", 0) for sch in schedules if not sch.get("paused")
                ]
                if pending:
                    delay = max(0.0, min(pending) - now)
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(wake.wait(), delay)
    except asyncio.CancelledError:
        pass
    except Exception:
        logger.exception("scheduler tick crashed for %s", sid)


def _fire_item(sch: dict) -> dict:
    item = {"target": sch.get("target", ""), "payload": sch.get("payload") or {}}
    if sch.get("timeout") is not None:
        item["timeout"] = sch["timeout"]
    return item


async def _fire_many(sid: str, due: list[dict], kernel) -> list[dict]:
    """Fire every due schedule AS THE SCHEDULER — one batch send, the items
    concurrent, no head-of-line blocking — then record one `schedule_fired` per
    outcome, and return those events in `due` order. The tick loop is a task the
    scheduler's `boot` spawned and `tick_now` is its own dispatch, so the batch is
    the scheduler's, gated as it."""
    ts = time.time()
    try:
        outcomes = list(await kernel.send([_fire_item(sch) for sch in due]))
    except Exception as e:
        failure = {"status": "failed", "result": {"error": f"{type(e).__name__}: {e}"}}
        outcomes = [failure for _ in due]
    duration_ms = int((time.time() - ts) * 1000)
    events: list[dict] = []
    for sch, out in zip(due, outcomes):
        status = (out or {}).get("status")
        res = (out or {}).get("result")
        result = None
        error = None
        if status == "completed":
            result = res
        else:
            detail = res.get("error") if isinstance(res, dict) else res
            error = f"{status}: {detail}"
        event = {
            "type": "schedule_fired",
            "schedule_id": sch["id"],
            "scheduler_id": sid,
            "target": sch.get("target", ""),
            "payload": sch.get("payload") or {},
            "result": result,
            "error": error,
            "ts": ts,
            "duration_ms": duration_ms,
        }
        if err := await _append_history(sid, kernel, event):
            logger.warning(
                "scheduler %s: history persist failed: %s", sid, err.get("error")
            )
        # ONE delivery per tick. This used to also emit `schedule_fired` onto the
        # TARGET's inbox, so a live program (exec_bridge mail → on_message)
        # received every tick TWICE — the payload send and then this telemetry
        # echo of the same tick, result included. Measured 2026-08-07 (combined
        # probe, opus): the model discovered it live, named it "the double-fire
        # hazard", and had to design its tool around filtering the duplicate.
        # Telemetry about a fire is the SCHEDULER's story: its own inbox carries
        # the event for watchers and `history` already persists it; the target
        # got the actual payload, which IS the tick.
        await kernel.emit(sid, event)
        events.append(event)
    return events


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + loop state. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "Recurring-task scheduler.",
        "paused": bool(rec.get("paused")),
        "running": id in _tick_tasks and not _tick_tasks[id].done(),
    }


async def _boot(id, payload, kernel):
    """Idempotent. Starts the tick loop; persistence is THROUGH the loader.
    A write failfasts only when no store is wired (reads are empty until then)."""
    if id not in _unsub:
        # The scheduler's own record changed (`paused`, by a verb or an
        # update_agent): the loop recomputes when to wake.
        def _on_state(ev: dict, sid: str = id) -> None:
            if ev.get("agent_id") == sid and ev.get("kind") == "updated":
                _poke(sid)

        _unsub[id] = kernel.ctx.add_state_subscriber(_on_state)
    if id not in _tick_tasks or _tick_tasks[id].done():
        # REGISTERED. The tick loop outlives this `boot` dispatch and fires
        # sends for the life of the agent; a bare `create_task` left it out of
        # `ps`, beyond `cancel`, and running under the boot dispatch's scope —
        # so every schedule it fired was rowed under a task that had ended.
        tid = kernel.ctx.spawn_task(id, "tick_loop", _tick_loop(id, kernel))
        _tick_tasks[id] = kernel.ctx.tasks[tid]["_task"]
    else:
        _poke(id)  # a boot of a running loop makes it look again
    return {"running": True}


async def _schedule(id, payload, kernel):
    """args: target:str (the agent each fire calls), payload:{type:<verb>, ...} (the call itself: any verb `target` answers, with its args; {type:'send', text:'...'} wakes an AI with that text), interval_seconds:int (default 60), timeout:number? (seconds per fire; absent = wait forever). Every fire runs AS THE SCHEDULER — it reaches what the scheduler's can_call grants. Returns {schedule_id, schedule}. Persisted THROUGH the loader; failfast if no store wired."""
    target = payload.get("target", "")
    sched_payload = payload.get("payload")
    if sched_payload is None:
        sched_payload = {}
    if not isinstance(sched_payload, dict):
        # A list/string here used to raise `.get` out of the handler.
        return bad("schedule", "payload", "an object with type")
    interval = _int_arg(payload, "interval_seconds", 60, "schedule", 1)
    if isinstance(interval, dict):
        return interval
    if not target:
        return {"error": "schedule: target required"}
    if not sched_payload.get("type"):
        return {"error": "schedule: payload.type required"}
    timeout = payload.get("timeout")
    if timeout is not None and (
        isinstance(timeout, bool)
        or not isinstance(timeout, (int, float))
        or timeout <= 0
    ):
        return bad("schedule", "timeout", "a positive number of seconds")
    sch = {
        "id": f"sch_{secrets.token_hex(3)}",
        "target": target,
        "payload": sched_payload,
        "interval_seconds": interval,
        "created_at": time.time(),
        "next_run": time.time() + interval,
        "run_count": 0,
        "paused": False,
    }
    if timeout is not None:
        sch["timeout"] = timeout
    (await _load(id, kernel)).append(sch)
    if err := await _save(id, kernel):
        # roll back the in-cache append so a failed persist leaves no phantom schedule
        _cache[id] = [s for s in _cache.get(id, []) if s["id"] != sch["id"]]
        out = {"error": f"scheduler.schedule: {err['error']}"}
        if err.get("reason"):
            out["reason"] = err["reason"]
        return out
    _poke(id)
    return {"schedule_id": sch["id"], "schedule": sch}


async def _unschedule(id, payload, kernel):
    """args: schedule_id:str (req). Returns {removed:bool, schedule_id}."""
    sid = payload.get("schedule_id")
    if not sid:
        return {"error": "unschedule: schedule_id required"}
    cur = await _load(id, kernel)
    before = len(cur)
    _cache[id] = [s for s in cur if s["id"] != sid]
    removed = len(_cache[id]) < before
    if removed:
        if err := await _save(id, kernel):
            return {
                "error": f"scheduler.unschedule: {err['error']}",
                **({"reason": err["reason"]} if err.get("reason") else {}),
            }
        _poke(id)
    return {"removed": removed, "schedule_id": sid}


async def _list(id, payload, kernel):
    """No args. Returns {schedules: [<schedule record>, ...]} — every schedule."""
    return {
        "schedules": list(await _load(id, kernel)),
        "origin": f"{id} list — re-send; changes as schedules change",
    }


async def _pause(id, payload, kernel):
    """args: schedule_id:str?. With id pauses one schedule; without, pauses the whole scheduler."""
    sid = payload.get("schedule_id")
    if sid:
        touched = 0
        for s in await _load(id, kernel):
            if s["id"] == sid:
                s["paused"] = True
                touched += 1
        if touched:
            if err := await _save(id, kernel):
                return {
                    "error": f"scheduler.pause: {err['error']}",
                    **({"reason": err["reason"]} if err.get("reason") else {}),
                }
            _poke(id)
        return {"paused": touched > 0, "schedule_id": sid}
    kernel.update(id, paused=True)
    return {"paused": True, "scheduler_id": id}


async def _resume(id, payload, kernel):
    """args: schedule_id:str?. With id resumes one schedule; without, resumes the whole scheduler."""
    sid = payload.get("schedule_id")
    if sid:
        touched = 0
        for s in await _load(id, kernel):
            if s["id"] == sid:
                s["paused"] = False
                touched += 1
        if touched:
            if err := await _save(id, kernel):
                return {
                    "error": f"scheduler.resume: {err['error']}",
                    **({"reason": err["reason"]} if err.get("reason") else {}),
                }
            _poke(id)
        return {"resumed": touched > 0, "schedule_id": sid}
    kernel.update(id, paused=False)
    return {"resumed": True, "scheduler_id": id}


async def _tick_now(id, payload, kernel):
    """args: schedule_id:str (req). Bumps run_count, persists, then fires that schedule immediately, as the scheduler. Returns {fired:true, schedule_id, result, error} — the fire's outcome as `history` records it: the target's reply, or why the fire failed."""
    sid = payload.get("schedule_id")
    if not sid:
        return {"error": "tick_now: schedule_id required"}
    for s in await _load(id, kernel):
        if s["id"] == sid:
            s["run_count"] = s.get("run_count", 0) + 1
            if err := await _save(id, kernel):
                s["run_count"] -= 1
                return {
                    "error": f"scheduler.tick_now: {err['error']}",
                    **({"reason": err["reason"]} if err.get("reason") else {}),
                }
            # THE OUTCOME, not just the fact. `{fired:true}` alone hid a target that
            # refused every fire: measured 2026-10-06 (rust subai_graph), a schedule
            # sent `{type:'tick'}` to an AI agent, which has no such verb; `tick_now`
            # said fired 16 times and the model never learned the payload was wrong.
            (ev,) = await _fire_many(id, [s], kernel)
            return {
                "fired": True,
                "schedule_id": sid,
                "result": ev["result"],
                "error": ev["error"],
            }
    return {"error": f"schedule {sid!r} not found"}


async def _history(id, payload, kernel):
    """args: limit:int? (1..500, default 100), schedule_id:str?. Returns {history:[event,...], count} — every schedule's fires."""
    limit = _int_arg(payload, "limit", 100, "history", 1)
    if isinstance(limit, dict):
        return limit
    entries = (await _read_history(id, kernel, 2 * HISTORY_MAX))[-min(500, limit) :]
    sid = payload.get("schedule_id")
    if sid:
        entries = [e for e in entries if e.get("schedule_id") == sid]
    return {
        "history": entries,
        "count": len(entries),
        "origin": f"{id} history — re-send; grows as schedules fire",
    }


# ─── dispatch ───────────────────────────────────────────────────


#: Served by `docs` (tier detailed).
EMITS = {
    "schedule_fired": "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — emitted on the scheduler's OWN inbox after every fire (never the target's — the payload send is the tick)",
}

VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "schedule": _schedule,
    "unschedule": _unschedule,
    "list": _list,
    "pause": _pause,
    "resume": _resume,
    "tick_now": _tick_now,
    "history": _history,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"scheduler: unknown type {t!r}", verb=t)
    return await fn(id, payload, kernel)


async def on_delete(agent):
    """Cascade hook: drop the in-process cache and stop the tick loop, so a
    re-created scheduler with the same id does not list and fire the old
    schedules (measured 2026-09-16)."""
    _cache.pop(agent.id, None)
    t = _tick_tasks.pop(agent.id, None)
    if t is not None and not t.done():
        t.cancel()
    unsub = _unsub.pop(agent.id, None)
    if unsub is not None:
        unsub()
    _wake.pop(agent.id, None)


# Shutdown frees the same process resources — declared, never inferred.
on_shutdown = on_delete
