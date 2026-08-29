"""THE AI CONNECTOR — create the agent, wire it, drive it, stop it.

This is the other half of what every probe duplicated. Creating a live AI agent
is never one call: it is a record with a backend-shaped meta, a reciprocal
ingress grant so its stream can reach `cli`, an api key for the backends that
need one, a provider wrapped in the proxy reader, and a drive loop that knows
when a chain has ended. Ten copies of that, each subtly behind the others.

The failures this consolidates are all measured, not imagined:

  * `ps`/`cancel` are KERNEL verbs. Addressed to the AI agent they came back
    `not_here`, and a harness that discards the reply cancelled NOTHING — capped
    runs were not killed, they were abandoned, and their turns kept running.
  * A dead chain announces itself as `status(reason=error)` on the AI's own
    inbox. With no subscriber it went nowhere and a chain that died mid-flight
    read as a model that chose to stop.
  * `ai_core` RETURNS `{"error": …}` rather than raising, so a first send that
    timed out scored as "the model produced nothing".
  * A fixed turn budget cannot tell "slower" from "worse" — under parallel load
    probes hit a 40-turn cap mid-work that they had finished solo an hour before.
    The cap counts turns since the last NEW call instead.

Deny-all is the substrate's posture, so `can_call` is a REQUIRED argument here:
there is no "sensible default toolset" to fall back to, and a silent default is
exactly how a probe would stop measuring what it claims to measure.
"""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass, field
from typing import Any

from helpers.proxy import Limits, distinct_calls


BASELINE = {"$self": "*", "$parent": "*", "$children": "*"}


@dataclass
class Connected:
    """A live, wired AI agent plus everything needed to watch and stop it."""

    id: str
    root: Any
    #: a `helpers.transcript.TranscriptSource` — a wrapped provider in process,
    #: a stream subscription over a wire. The drive loop needs only the
    #: interface, which is what lets it run against a kernel it cannot import.
    proxy: Any
    limits: Limits
    trips: list[str] = field(default_factory=list)
    _tasks: list = field(default_factory=list)

    @property
    def turns(self) -> int:
        return self.proxy.n

    @property
    def text(self) -> str:
        return self.proxy.text

    async def close(self) -> None:
        for t in self._tasks:
            t.cancel()


async def connect(
    root,
    core,
    backend,
    *,
    can_call: dict,
    callable_by: dict | None = None,
    agent_id: str = "ai",
    limits: Limits | None = None,
    stream_to: str = "cli",
    record: dict | None = None,
    watch=None,
) -> Connected:
    """Create the AI agent, wire both sides of the gate, install the reader."""
    L = limits or Limits.from_env(os.environ)
    meta = {
        "type": "create_agent",
        "parent_id": root.id,
        "id": agent_id,
        "context_window": L.ctx,
        "num_ctx": L.ctx,
        "can_call": can_call,
        # THE INGRESS HALF. A peer granted `can_call: {ai: [...]}` still cannot
        # reach the AI: the gate is two-sided and a sibling has no baseline edge.
        # A fixture that opens only the egress half builds a wire that carries
        # nothing in the direction it was drawn — measured here as a probe whose
        # push-back never arrived, scoring a model failure that never happened.
        # THE DRIVER'S OWN INGRESS, on the wire only. In process the probe is
        # the OPERATOR and its sends are ungated; across REST it arrives as the
        # `rest` leg — an agent, gated like any other — so the AI must accept it
        # or the very first `send` is refused at ingress. Granted at CREATE
        # time, from above, because the leg may not raise its own authority.
        **({"callable_by": {**(callable_by or {}),
                            getattr(root, "leg_id", "rest"): "*"}}
           if getattr(root, "leg_id", None) else
           ({"callable_by": callable_by} if callable_by else {})),
        **backend.record(),
        **(record or {}),
    }
    made = (await root.send([{"target": root.id, "payload": meta}]))[0]["result"]
    if isinstance(made, dict) and made.get("error"):
        raise SystemExit(f"create_agent({agent_id}) refused: {made}")
    aid = made["id"]

    # TWO-SIDED DENY-ALL. The AI streams its turn to `cli`; cli's baseline
    # `callable_by` accepts only its own subtree and creator, so without this the
    # egress grant above is half a wire and the stream is refused at ingress.
    if stream_to:
        # `{}` and `None` are DIFFERENT answers: an ephemeral agent like `cli`
        # carries an empty meta, and collapsing the two skipped opening its
        # ingress — the AI's stream was then refused and the chain died after
        # one turn.
        # IN-PROCESS ONLY. A spawned kernel is reached over REST, which has no
        # handle on the live agent objects; there the render sink is whatever
        # the roster wired, and its ingress was opened when it was created.
        ctx = getattr(root, "ctx", None)
        _peer = ctx.agents.get(stream_to) if ctx is not None else None
        peer = dict(_peer._meta) if _peer is not None else None
        if peer is not None:
            # READ, add one key, write back — never replace. Clobbering the map
            # would silently drop grants the fixture already placed on `cli`.
            cb = dict(peer.get("callable_by") or {})
            cb[aid] = "*"
            await root.send([{
                "target": root.id,
                "payload": {"type": "update_agent", "id": stream_to,
                            "callable_by": cb},
            }])

    # NOTHING TO DELIVER. Backends read their key from the ENVIRONMENT, and this
    # kernel runs in THIS process — so the variable the probe was started with is
    # already the one the backend reads. The key used to be pushed in through a
    # `set_api_key` verb that then persisted it.
    if key_var := getattr(backend, "api_key_var", None):
        assert os.environ.get(key_var), f"{key_var} is unset — the backend will refuse"

    conn = Connected(id=aid, root=root, proxy=None, limits=L)  # type: ignore[arg-type]

    # THE TRANSCRIPT, from whichever side of the wire we are on. In process the
    # reader WRAPS the provider, which is what gives it the per-turn tripwires
    # (chars/second bounds) and the system block. Across a socket there is no
    # provider object to wrap, so the same transcript is read from the agent's
    # own durable history — the numbers `distinct_calls` and the turn tripwires
    # see are identical either way.
    if core is None:
        from helpers.host.wire import WireTranscript

        conn.proxy = WireTranscript(root.k, aid)
    else:
        from helpers.proxy import ProxyReader

        conn.proxy = ProxyReader(
            await backend.provider(aid, root), L, trips=conn.trips, watch=watch)
        core._providers[aid] = conn.proxy

    # SEE A DEAD CHAIN — a failed handoff announces itself as status(error) on
    # the agent's own stream, and with nobody listening it goes nowhere.
    def _failed(msg: str) -> None:
        print(f"\n  ** TURN FAILED: {msg} **\n", flush=True)
        conn.trips.append(f"turn failed: {msg}")

    # SEE A DEAD CHAIN — subscribe to the AI's own status events. A failed
    # handoff announces itself this way; with nobody listening, a chain that died
    # mid-flight looks exactly like a model that chose to stop.
    if core is None:
        # No inbox tap across the wire — a dead chain is caught by the drive
        # loop's `ps` check, which works on every runtime.
        return conn

    inbox = f"{aid}_errspy"
    root.ctx.ensure_inbox(inbox)
    root.watch(aid, inbox)

    async def _errspy():
        q = root.ctx.inboxes[inbox]
        while True:
            ev = await q.get()
            det = ev.get("detail") or {}
            if ev.get("type") == "status" and det.get("error"):
                _failed(det["error"])

    conn._tasks.append(asyncio.create_task(_errspy()))
    return conn


async def _live_tasks(root, aid) -> list[dict]:
    """The AI's in-flight rows, asked of the ROOT — `ps` is a kernel verb."""
    rows = ((await root.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]
            ["result"]).get("tasks") or []
    return [t for t in rows if t.get("agent") == aid and t.get("verb") != "ps"]


async def drive(conn: Connected, task: str, *, client_id: str = "probe") -> str:
    """Send the task and run until the chain ends, stalls, or trips.

    Returns "" on a clean end, or the reason the run was cut.
    """
    root, aid, L = conn.root, conn.id, conn.limits

    # SURFACE A FAILED SEND — ai_core returns errors, it does not raise.
    first = (await root.send([{
        "target": aid,
        "payload": {"type": "send", "text": task, "client_id": client_id},
    }]))[0]["result"]
    if isinstance(first, dict) and first.get("error"):
        print(f"\n  ** FIRST SEND FAILED: {first['error']} **\n", flush=True)

    idle, last, stale, seen_calls, capped = 0.0, 0, 0, -1, ""
    while idle < L.idle:
        await asyncio.sleep(2.0)
        calls = distinct_calls(conn.proxy.transcript)
        if calls != seen_calls:
            seen_calls, stale = calls, 0
        elif conn.proxy.n != last:
            stale += 1

        # CHAIN DEATH. The per-turn tripwires only fire when a turn LANDS, so a
        # model that simply stops producing turns evades all of them and the run
        # sits out the full idle window before scoring.
        if idle >= 30 and conn.proxy.n == last and last > 0:
            rows = ((await root.send([{"target": "kernel", "payload": {"type": "ps"}}]))
                    [0]["result"]).get("tasks") or []
            if not [t for t in rows if t.get("agent") == aid and t.get("verb") != "ps"]:
                capped = (
                    f"CHAIN ENDED — no turn and nothing in flight after {idle:.0f}s"
                    f" · ps={[(t.get('agent'), t.get('verb')) for t in rows]}"
                )

        if conn.trips:
            capped = "RUNAWAY TURN — " + conn.trips[-1]
        elif L.no_progress and stale >= L.no_progress:
            capped = f"NO_PROGRESS={L.no_progress} turns with no new call"
        elif L.max_turns and conn.proxy.n >= L.max_turns:
            capped = f"MAX_TURNS={L.max_turns}"

        if capped:
            # Cancel through the TASK TABLE — the same ps/cancel an operator
            # would use, addressed to the ROOT because they are kernel verbs.
            for t in await _live_tasks(root, aid):
                c = (await root.send([{
                    "target": "kernel",
                    "payload": {"type": "cancel", "task_id": t["id"]},
                }]))[0]["result"]
                if isinstance(c, dict) and c.get("error"):
                    print(f"  ** CANCEL REFUSED: {c['error']} **", flush=True)
            break

        if conn.proxy.n != last:
            last, idle = conn.proxy.n, 0.0
        else:
            idle += 2.0

    if capped:
        print(f"\n  ** CAPPED: {capped} · {conn.proxy.n} passes **")
    return capped
