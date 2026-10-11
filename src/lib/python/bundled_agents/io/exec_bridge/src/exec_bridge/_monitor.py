"""MONITORS (owner, 2026-10-09): a filter on a running job's OUTPUT that tells an agent
when something it watches for appears — how a long run is stopped EARLY.

A job's output already flows through this bridge line by line, so a monitor POLLS
NOTHING: each line is matched as it arrives (`match`, a regex; absent = every line),
and a match is SENT to `notify` as a turn. `every` throttles delivery — the first
match goes at once, later ones are gathered and delivered at most once per `every`
seconds — and a timer exists only while something waits to be delivered. An idle
monitor wakes never. `tail` caps the lines in one delivery to the newest N.

Each delivery is a call, made by this bridge AS ITSELF (its `can_call` must name
`notify`, and `notify`'s `callable_by` this bridge), fire-and-forget as its own task:

    [monitor 6342 · job 6301 · from your call 6340 · "w5 fatal"]
    FATAL w5: loss diverged (code 1a2b3c4d)

A monitor IS a kernel task (its id is the `monitor_id`): `ps` lists it, kernel
`cancel` (or `unmonitor`) ends it, and it ends with its job — after delivering what
was still waiting. RAM only, like the job it watches.

A DELIVERY THE TARGET REFUSES (the call gate: `unauthorized`; no such agent:
`unknown_agent`) ENDS the monitor — never a silent drop (fantastic-cli, 2026-10-12:
an AI whose `callable_by` lacked the bridge got no turn and nobody learned it). The
dead monitor stays in `monitors` with `status: "dead"` and the refusal as `error`
until `unmonitor` clears it.
"""

from __future__ import annotations

import asyncio
import re
import time

#: Lines in one delivery when the caller names no `tail`.
DEFAULT_TAIL = 100

_END = object()
_DEAD = object()

#: What a refused delivery answers — the target will never take one.
REFUSED = ("unauthorized", "unknown_agent")

# monitor id → its state
_MONITORS: dict[int, dict] = {}


def by_job(job_id: int) -> list[dict]:
    return [
        m for m in _MONITORS.values() if m["job_id"] == job_id and not m.get("error")
    ]


def feed_line(job_id: int, line: str) -> None:
    """One output line of `job_id`: queued for every monitor whose `match` it meets."""
    for m in by_job(job_id):
        if m["match"] is None or m["match"].search(line):
            m["queue"].put_nowait(line)


def feed_text(job_id: int, text: str) -> None:
    """A PTY chunk: split into lines (a partial line waits for its end)."""
    for m in by_job(job_id):
        buf = m["carry"] + text
        *lines, m["carry"] = buf.split("\n")
        for line in lines:
            line = line.rstrip("\r")
            if m["match"] is None or m["match"].search(line):
                m["queue"].put_nowait(line)


def job_ended(job_id: int) -> None:
    """The job is gone: each of its monitors delivers what waits, then ends."""
    for m in by_job(job_id):
        if m["carry"]:
            line = m["carry"].rstrip("\r")
            m["carry"] = ""
            if m["match"] is None or m["match"].search(line):
                m["queue"].put_nowait(line)
        m["queue"].put_nowait(_END)


def listing(owner: str) -> list[dict]:
    return [
        {
            "monitor_id": m["id"],
            "job_id": m["job_id"],
            "notify": m["notify"],
            "match": m["match"].pattern if m["match"] is not None else None,
            "every": m["every"],
            "tail": m["tail"],
            "label": m["label"],
            "delivered": m["delivered"],
            "pending": m["pending"],
            "status": "dead" if m.get("error") else "running",
            "error": m.get("error"),
        }
        for m in _MONITORS.values()
        if m["owner"] == owner
    ]


def compile_match(pattern) -> re.Pattern | None:
    return re.compile(pattern)


def header(m: dict, ended: bool) -> str:
    who = "your" if m["caller"] == m["notify"] else f"{m['caller'] or 'the operator'}'s"
    parts = [f"monitor {m['id']}", f"job {m['job_id']}", f"from {who} call {m['call']}"]
    if m["label"]:
        parts.append(f'"{m["label"]}"')
    if ended:
        parts.append("job ended")
    return "[" + " · ".join(parts) + "]"


async def run(m: dict, kernel) -> None:
    """The monitor's task: gather what matched, deliver it on the throttle, end with
    the job or a cancel. No timer while nothing waits."""
    last: float | None = None
    pending: list[str] = []
    ended = False
    q: asyncio.Queue = m["queue"]
    try:
        while True:
            now = time.monotonic()
            due = pending and (
                m["every"] is None or last is None or now - last >= m["every"]
            )
            if not due:
                timeout = None
                if pending:  # throttled: wait out the rest of the window
                    timeout = m["every"] - (now - last)
                try:
                    item = await asyncio.wait_for(q.get(), timeout)
                except TimeoutError:
                    item = None
                if item is not None:
                    for x in [item, *_drain(q)]:
                        if x is _DEAD:
                            return
                        if x is _END:
                            ended = True
                        else:
                            pending.append(x)
                m["pending"] = len(pending)
            now = time.monotonic()
            if pending and (
                ended or m["every"] is None or last is None or now - last >= m["every"]
            ):
                _deliver(m, pending, ended, kernel)
                last = now
                pending = []
                m["pending"] = 0
            if ended:
                return
    finally:
        if not m.get("error"):
            _MONITORS.pop(m["id"], None)


def _drain(q: asyncio.Queue) -> list:
    out = []
    while True:
        try:
            out.append(q.get_nowait())
        except asyncio.QueueEmpty:
            return out


def _deliver(m: dict, lines: list[str], ended: bool, kernel) -> None:
    if m.get("error"):  # dead: the target refused an earlier delivery
        return
    dropped = max(0, len(lines) - m["tail"])
    body = "\n".join(lines[-m["tail"] :])
    if dropped:
        body += f"\n(+{dropped} earlier lines dropped)"
    m["delivered"] += 1
    text = header(m, ended) + "\n" + body

    async def send():
        out = await kernel.send(
            [{"target": m["notify"], "payload": {"type": "send", "text": text}}]
        )
        got = (out or [{}])[0]
        got = got.get("result", got) if isinstance(got, dict) else got
        if (
            isinstance(got, dict)
            and got.get("reason") in REFUSED
            and not m.get("error")
        ):
            m["error"] = str(got.get("error") or got.get("reason"))
            m["delivered"] -= 1
            m["queue"].put_nowait(_DEAD)

    kernel.ctx.spawn_task(m["owner"], "monitor_notify", send())
