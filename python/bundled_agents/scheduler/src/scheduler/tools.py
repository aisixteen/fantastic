"""scheduler bundle — recurring tasks as an agent.

State (sidecars in `.fantastic/agents/{id}/`, persisted THROUGH the loader):
  schedules.json   — list of {id, target, payload, interval_seconds, next_run, paused, run_count}
  history.jsonl    — append-only schedule_fired events, ring-trimmed to HISTORY_MAX

There is NOTHING to wire — the scheduler persists through `kernel_state` (the loader),
which owns the `.fantastic` store. A write failfasts only when no store is wired at the
root (read-empty until then); a denied/failed write is surfaced, not lost.

Verbs:
  reflect      -> {sentence, tick_sec, paused, ...}
  boot         -> start tick loop (idempotent)
  schedule     args: target, payload, interval_seconds  -> {schedule_id}
  unschedule   args: schedule_id                        -> {removed: bool}
  list                                                  -> {schedules: [...]}
  pause        args: schedule_id?                       -> pause one or all
  resume       args: schedule_id?                       -> resume one or all
  tick_now     args: schedule_id                        -> fire immediately
  history      args: limit?, schedule_id?              -> {history: [...]}

A schedule fires by `kernel.send(target, payload)`. After firing, scheduler
emits `{type:"schedule_fired", ...}` to its OWN inbox AND the target's inbox.
"""

from __future__ import annotations

import asyncio
import json
import logging
import secrets
import time

logger = logging.getLogger(__name__)

HISTORY_MAX = 500

_tick_tasks: dict[str, asyncio.Task] = {}
_cache: dict[str, list[dict]] = {}  # in-process schedules cache


# ─── file routing ────────────────────────────────────────────────


# Sidecar filenames — kernel_state computes the dir (`agents/<id>/`) under the one
# `.fantastic` store it owns. The scheduler wires NOTHING; it persists THROUGH the loader.
_SCHED_NAME = "schedules.json"
_HISTORY_NAME = "history.jsonl"


async def _file_read(sid: str, kernel, name: str) -> str | None:
    """Read a sidecar THROUGH the loader. Missing / no-store ⇒ None (reads lenient)."""
    r = await kernel.send(
        "kernel_state", {"type": "load_blob", "agent_id": sid, "name": name}
    )
    return r.get("content") if isinstance(r, dict) else None


async def _file_write(sid: str, kernel, name: str, content: str) -> dict | None:
    """Write a sidecar THROUGH the loader. Returns an error dict on a denied/no-store
    write (no silent loss), or None on success."""
    w = await kernel.send(
        "kernel_state",
        {"type": "persist_blob", "agent_id": sid, "name": name, "content": content},
    )
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
            tick_sec = float(rec.get("tick_sec") or 1.0)
            await asyncio.sleep(tick_sec)
            if rec.get("paused"):
                continue
            now = time.time()
            schedules = await _load(sid, kernel)
            for sch in list(schedules):
                if sch.get("paused"):
                    continue
                if now < sch.get("next_run", 0):
                    continue
                await _fire(sid, sch, kernel)
                sch["run_count"] = sch.get("run_count", 0) + 1
                sch["next_run"] = time.time() + sch["interval_seconds"]
                if err := await _save(sid, kernel):
                    logger.warning(
                        "scheduler %s: persist failed: %s", sid, err.get("error")
                    )
    except asyncio.CancelledError:
        pass
    except Exception:
        logger.exception("scheduler tick crashed for %s", sid)


async def _fire(sid: str, sch: dict, kernel) -> None:
    target = sch.get("target", "")
    payload = sch.get("payload") or {}
    ts = time.time()
    result = None
    error = None
    try:
        result = await kernel.send(target, payload)
    except Exception as e:
        error = f"{type(e).__name__}: {e}"

    event = {
        "type": "schedule_fired",
        "schedule_id": sch["id"],
        "scheduler_id": sid,
        "target": target,
        "payload": payload,
        "result": result,
        "error": error,
        "ts": ts,
        "duration_ms": int((time.time() - ts) * 1000),
    }
    if err := await _append_history(sid, kernel, event):
        logger.warning(
            "scheduler %s: history persist failed: %s", sid, err.get("error")
        )
    await kernel.emit(sid, event)
    if target and target != sid:
        await kernel.emit(target, event)


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + tick state. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "Recurring-task scheduler.",
        "tick_sec": float(rec.get("tick_sec") or 1.0),
        "paused": bool(rec.get("paused")),
        "running": id in _tick_tasks and not _tick_tasks[id].done(),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "schedule_fired": "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — broadcast to scheduler's own inbox AND target's inbox after every fire",
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Starts the tick loop. Persistence is THROUGH the loader; a write
    failfasts only when no store is wired (reads are empty until then)."""
    if id not in _tick_tasks or _tick_tasks[id].done():
        _tick_tasks[id] = asyncio.create_task(_tick_loop(id, kernel))
    return {"running": True}


async def _schedule(id, payload, kernel):
    """args: target:str, payload:{type:..,...}, interval_seconds:int (default 60). Returns {schedule_id, schedule}. Persisted THROUGH the loader; failfast if no store wired."""
    target = payload.get("target", "")
    sched_payload = payload.get("payload") or {}
    interval = max(1, int(payload.get("interval_seconds", 60)))
    if not target:
        return {"error": "schedule: target required"}
    if not sched_payload.get("type"):
        return {"error": "schedule: payload.type required"}
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
    """args: schedule_id:str (req). Fires that schedule immediately, bumps run_count, persists. Returns {fired:true, schedule_id}."""
    sid = payload.get("schedule_id")
    if not sid:
        return {"error": "tick_now: schedule_id required"}
    for s in await _load(id, kernel):
        if s["id"] == sid:
            await _fire(id, s, kernel)
            s["run_count"] = s.get("run_count", 0) + 1
            if err := await _save(id, kernel):
                return {
                    "error": f"scheduler.tick_now: {err['error']}",
                    **({"reason": err["reason"]} if err.get("reason") else {}),
                }
            return {"fired": True, "schedule_id": sid}
    return {"error": f"schedule {sid!r} not found"}


async def _history(id, payload, kernel):
    """args: limit:int? (1..500, default 100), schedule_id:str?. Returns {history:[event,...], count}."""
    limit = max(1, min(500, int(payload.get("limit", 100))))
    entries = await _read_history(id, kernel, limit)
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
