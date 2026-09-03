"""scheduler bundle — recurring tasks as an agent.

State (sidecars in `.fantastic/agents/{id}/`, persisted THROUGH the loader):
  schedules.json   — list of {id, target, payload, interval_seconds, next_run, paused, run_count, timeout?}
  history.jsonl    — append-only schedule_fired events, ring-trimmed to HISTORY_MAX

There is NOTHING to wire — the scheduler persists through `kernel_state` (the loader),
which owns the `.fantastic` store. A write failfasts only when no store is wired at the
root (read-empty until then); a denied/failed write is surfaced, not lost.

Verbs:
  reflect      -> {sentence, tick_sec, paused, ...}
  boot         -> start tick loop (idempotent)
  schedule     args: target, payload, interval_seconds, timeout?  -> {schedule_id}
  unschedule   args: schedule_id                        -> {removed: bool}
  list                                                  -> {schedules: [...]}
  pause        args: schedule_id?                       -> pause one or all
  resume       args: schedule_id?                       -> resume one or all
  tick_now     args: schedule_id                        -> fire immediately
  history      args: limit?, schedule_id?              -> {history: [...]}

A tick collects every DUE schedule, bumps `run_count`/`next_run`, persists FIRST
(a crash inside the fire window cannot re-fire on reboot), then fires them all as
ONE `kernel.send([{target, payload, timeout?}, ...])` batch — concurrent, so one
hung target never blocks the others. `timeout` is the schedule's own per-fire
bound (seconds); absent = wait forever (the kernel never defaults it). After
firing, scheduler emits one `{type:"schedule_fired", ...}` per outcome to its OWN
inbox only — the target gets the PAYLOAD, which is the tick; echoing telemetry
at it doubled every delivery.
"""

from __future__ import annotations

import asyncio
import json
import logging
import secrets
import time

from kernel import operator_context
from kernel._refusal import bad

logger = logging.getLogger(__name__)

HISTORY_MAX = 500
#: seconds between tick-loop sweeps when the record does not name one.
DEFAULT_TICK_SEC = 1.0

_tick_tasks: dict[str, asyncio.Task] = {}
_cache: dict[str, list[dict]] = {}  # in-process schedules cache


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


def _tick_sec(rec: dict) -> float:
    """The record's sweep interval. `validate_meta` refuses a non-positive or
    non-numeric `tick_sec` at WRITE time, so this read is total: an absent field
    is the documented default, and no value that reached the record can raise
    inside the loop."""
    v = rec.get("tick_sec")
    if isinstance(v, bool) or not isinstance(v, (int, float)) or v <= 0:
        return DEFAULT_TICK_SEC
    return float(v)


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a `tick_sec` that is not a positive number at WRITE time — the
    point of the mistake. Silently defaulting it (rust/swift did) or raising out
    of the tick loop (python did) both turn an operator's typo into a behaviour
    change nobody is told about."""
    merged = {**record, **meta}
    if "tick_sec" not in merged:
        return None
    v = merged["tick_sec"]
    if isinstance(v, bool) or not isinstance(v, (int, float)) or v <= 0:
        return "scheduler: tick_sec must be a positive number of seconds"
    return None


# ─── file routing ────────────────────────────────────────────────


# Sidecar filenames — kernel_state computes the dir (`agents/<id>/`) under the one
# `.fantastic` store it owns. The scheduler wires NOTHING; it persists THROUGH the loader.
_SCHED_NAME = "schedules.json"
_HISTORY_NAME = "history.jsonl"


async def _file_read(sid: str, kernel, name: str) -> str | None:
    """Read a sidecar THROUGH the loader. Missing / no-store ⇒ None (reads lenient)."""
    # Persist from ABOVE — sidecar I/O through the loader is operator bookkeeping
    # (a side effect of the scheduler's own authorized verb), not a lateral call.
    with operator_context():
        r = (
            await kernel.send(
                [
                    {
                        "target": "kernel_state",
                        "payload": {"type": "load_blob", "agent_id": sid, "name": name},
                    }
                ]
            )
        )[0]["result"]
    return r.get("content") if isinstance(r, dict) else None


async def _file_write(sid: str, kernel, name: str, content: str) -> dict | None:
    """Write a sidecar THROUGH the loader. Returns an error dict on a denied/no-store
    write (no silent loss), or None on success."""
    with (
        operator_context()
    ):  # persist from above — operator bookkeeping (see _file_read)
        w = (
            await kernel.send(
                [
                    {
                        "target": "kernel_state",
                        "payload": {
                            "type": "persist_blob",
                            "agent_id": sid,
                            "name": name,
                            "content": content,
                        },
                    }
                ]
            )
        )[0]["result"]
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
    if not raw:
        _cache[sid] = []
        return _cache[sid]
    try:
        _cache[sid] = json.loads(raw)
    except json.JSONDecodeError:
        _cache[sid] = []
    return _cache[sid]


async def _save(sid: str, kernel) -> dict | None:
    """Persist the cache THROUGH the loader; returns an error dict on failure."""
    return await _file_write(
        sid, kernel, _SCHED_NAME, json.dumps(_cache.get(sid, []), indent=2)
    )


# ─── history persistence (through the loader — read-modify-write) ──


async def _append_history(sid: str, kernel, event: dict) -> dict | None:
    raw = await _file_read(sid, kernel, _HISTORY_NAME) or ""
    appended = raw + json.dumps(event, default=str) + "\n"
    # Ring-trim if oversize.
    lines = appended.splitlines()
    if len(lines) > 2 * HISTORY_MAX:
        appended = "\n".join(lines[-HISTORY_MAX:]) + "\n"
    return await _file_write(sid, kernel, _HISTORY_NAME, appended)


async def _read_history(sid: str, kernel, limit: int) -> list[dict]:
    raw = await _file_read(sid, kernel, _HISTORY_NAME)
    if not raw:
        return []
    out: list[dict] = []
    for line in raw.splitlines()[-limit:]:
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


# ─── tick loop ──────────────────────────────────────────────────


async def _tick_loop(sid: str, kernel) -> None:
    try:
        while True:
            rec = kernel.get(sid)
            if rec is None:
                return
            await asyncio.sleep(_tick_sec(rec))
            if rec.get("paused"):
                continue
            now = time.time()
            schedules = await _load(sid, kernel)
            due = [
                sch
                for sch in list(schedules)
                if not sch.get("paused") and now >= sch.get("next_run", 0)
            ]
            if not due:
                continue
            # Persist BEFORE the fire: run_count/next_run are bumped and saved
            # first, so a crash inside the fire window does not re-fire the same
            # tick on reboot (restart semantics are explicit: at-most-once).
            for sch in due:
                sch["run_count"] = sch.get("run_count", 0) + 1
                sch["next_run"] = now + sch["interval_seconds"]
            if err := await _save(sid, kernel):
                logger.warning(
                    "scheduler %s: persist failed: %s", sid, err.get("error")
                )
            await _fire_many(sid, due, kernel)
    except asyncio.CancelledError:
        pass
    except Exception:
        logger.exception("scheduler tick crashed for %s", sid)


def _fire_item(sch: dict) -> dict:
    item = {"target": sch.get("target", ""), "payload": sch.get("payload") or {}}
    if sch.get("timeout") is not None:
        item["timeout"] = sch["timeout"]
    return item


async def _fire_many(sid: str, due: list[dict], kernel) -> None:
    """Fire every due schedule as ONE batch send — concurrent, no head-of-line
    blocking — then record one `schedule_fired` per outcome."""
    ts = time.time()
    outcomes: list[dict | None]
    try:
        outcomes = list(await kernel.send([_fire_item(s) for s in due]))
    except Exception as e:
        failure = {"status": "failed", "result": {"error": f"{type(e).__name__}: {e}"}}
        outcomes = [failure for _ in due]
    duration_ms = int((time.time() - ts) * 1000)
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


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + tick state. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "Recurring-task scheduler.",
        "tick_sec": _tick_sec(rec),
        "paused": bool(rec.get("paused")),
        "running": id in _tick_tasks and not _tick_tasks[id].done(),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "schedule_fired": "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — emitted on the scheduler's OWN inbox after every fire (never the target's — the payload send is the tick)",
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Starts the tick loop. Persistence is THROUGH the loader; a write
    failfasts only when no store is wired (reads are empty until then)."""
    if id not in _tick_tasks or _tick_tasks[id].done():
        _tick_tasks[id] = asyncio.create_task(_tick_loop(id, kernel))
    return {"running": True}


async def _schedule(id, payload, kernel):
    """args: target:str, payload:{type:..,...}, interval_seconds:int (default 60), timeout:number? (seconds per fire; absent = wait forever). Returns {schedule_id, schedule}. Persisted THROUGH the loader; failfast if no store wired."""
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
    return {"removed": removed, "schedule_id": sid}


async def _list(id, payload, kernel):
    """No args. Returns {schedules: [<schedule record>, ...]}."""
    return {"schedules": list(await _load(id, kernel))}


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
        return {"paused": touched > 0, "schedule_id": sid}
    kernel.update(id, paused=True)
    return {"paused": True, "scheduler_id": id}


async def _resume(id, payload, kernel):
    """args: schedule_id:str?. With id resumes one; without, resumes the whole scheduler."""
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
        return {"resumed": touched > 0, "schedule_id": sid}
    kernel.update(id, paused=False)
    return {"resumed": True, "scheduler_id": id}


async def _tick_now(id, payload, kernel):
    """args: schedule_id:str (req). Bumps run_count, persists, then fires that schedule immediately. Returns {fired:true, schedule_id}."""
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
            await _fire_many(id, [s], kernel)
            return {"fired": True, "schedule_id": sid}
    return {"error": f"schedule {sid!r} not found"}


async def _history(id, payload, kernel):
    """args: limit:int? (1..500, default 100), schedule_id:str?. Returns {history:[event,...], count}."""
    limit = _int_arg(payload, "limit", 100, "history", 1)
    if isinstance(limit, dict):
        return limit
    entries = await _read_history(id, kernel, min(500, limit))
    sid = payload.get("schedule_id")
    if sid:
        entries = [e for e in entries if e.get("schedule_id") == sid]
    return {"history": entries, "count": len(entries)}


# ─── dispatch ───────────────────────────────────────────────────


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
        return {"error": f"scheduler: unknown type {t!r}"}
    return await fn(id, payload, kernel)
