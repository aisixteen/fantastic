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

from helpers import memory
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
    #: Every `context:compacted` event this agent pushed, in order — the ONLY
    #: non-racy record that compaction actually happened. `context_status` reports
    #: the LAST projection, which is overwritten every turn: a run that compacted
    #: five times and then ended on a turn that fit reads `fired: False` there. A
    #: probe that scores what survived compaction must first prove there WAS one.
    compactions: list[dict] = field(default_factory=list)
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
        # ITS MEMORY — the conversation lives in the kernel's `memory` store, named
        # on the record and reachable by an explicit egress (ingress opened below).
        **memory.ai_memory_meta(can_call),
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
        **(
            {
                "callable_by": {
                    **(callable_by or {}),
                    getattr(root, "leg_id", "rest"): "*",
                    # ...and the EVENT face, which dispatches a `watch`
                    # as itself. The gate is two-sided: without this the
                    # subscription is refused at ingress and the stream
                    # reads as an agent that simply never emits (measured
                    # 2026-09-12, compaction:swift — every `context:compacted`
                    # event fired and none was counted).
                    **{i: "*" for i in getattr(root, "watch_ids", ())},
                }
            }
            if getattr(root, "leg_id", None)
            else ({"callable_by": callable_by} if callable_by else {})
        ),
        **backend.record(),
        **(record or {}),
    }
    made = (await root.send([{"target": root.id, "payload": meta}]))[0]["result"]
    if isinstance(made, dict) and made.get("error"):
        raise SystemExit(f"create_agent({agent_id}) refused: {made}")
    aid = made["id"]
    # THE STORE'S HALF of the memory wire: its `callable_by` names the agent.
    await memory.open_ingress(root, meta.get("memory_store_id") or memory.MEMORY, aid)

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
        # LIVE AGENT OBJECTS, which only the in-process kernel has — the wire
        # ctx answers rows and tasks, never `agents`. Ask for the capability;
        # the attribute's mere presence no longer means in-process.
        agents = getattr(getattr(root, "ctx", None), "agents", None)
        _peer = agents.get(stream_to) if agents is not None else None
        peer = dict(_peer._meta) if _peer is not None else None
        if peer is not None:
            # READ, add one key, write back — never replace. Clobbering the map
            # would silently drop grants the fixture already placed on `cli`.
            cb = dict(peer.get("callable_by") or {})
            cb[aid] = "*"
            await root.send(
                [
                    {
                        "target": root.id,
                        "payload": {
                            "type": "update_agent",
                            "id": stream_to,
                            "callable_by": cb,
                        },
                    }
                ]
            )

    # NOTHING TO DELIVER. The record names `credentials` (the plug's ref in the
    # kernel's `creds` store, `helpers/creds.py`); the backend reads the key from
    # the store per request, the store from the variable the run exported.

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
        from helpers.kernel import WRAPS, wrap_with
        from helpers.proxy import ProxyReader

        # NO CONNECT-TIME BUILD: `inner` is the provider ai_core builds for each
        # turn (`wrap_with` seats it), so nothing is built before the first turn.
        conn.proxy = ProxyReader(None, L, trips=conn.trips, watch=watch)
        WRAPS[aid] = wrap_with(conn.proxy)

    # SEE A DEAD CHAIN — subscribe to the AI's own status events. A failed
    # handoff announces itself this way; with nobody listening, a chain that died
    # mid-flight looks exactly like a model that chose to stop.
    # SEE A DEAD CHAIN — a failed handoff announces itself as status(error) on
    # the agent's own stream, and with nobody listening it goes nowhere.
    def _failed(msg: str) -> None:
        print(f"\n  ** TURN FAILED: {msg} **\n", flush=True)
        conn.trips.append(f"turn failed: {msg}")

    def _died(ev: dict, det: dict) -> bool:
        """A turn that ENDED for any reason but `ok`.

        `detail.error` alone is not enough: the AI layer's own 180s per-send
        ceiling closes a turn with `done{reason:"timeout"}` and NO error key, and
        a cancel closes it with `done{reason:"cancelled"}`. Both were invisible,
        so a chain that died at the ceiling and a model that chose to stop wrote
        the identical transcript — measured on `agentic.build:rust` and `:swift`,
        2026-09-03, where the chain ended after 160s and 280s with no reason
        recorded anywhere in the run.
        """
        return (
            ev.get("type") == "status"
            and ev.get("phase") == "done"
            # `awaiting` is the model's EXPLICIT wait (`<call name="await">`): a benign
            # end of turn, not a failure — the next delivery wakes it (2026-09-10).
            and det.get("reason") not in (None, "ok", "awaiting")
        )

    if core is None:
        # THE SAME EVENTS, over the socket. A failed pass announces itself as a
        # `status` carrying `detail.error`; with nobody listening a chain that
        # died mid-flight looks exactly like a model that chose to stop.
        from helpers.host.ws import WSWatcher

        async def _wire_errspy() -> None:
            watcher = WSWatcher(root.k.port, aid)
            try:
                await watcher.connect()
                async for ev in watcher.events():
                    det = ev.get("detail") or {}
                    if ev.get("type") == "status" and det.get("error"):
                        _failed(str(det["error"]))
                    if _died(ev, det):
                        _failed(f"turn ended: {det.get('reason')}")
                    if ev.get("type") == "context" and ev.get("phase") == "too_small":
                        _failed(f"context too_small: {det}")
                    if ev.get("type") == "context" and ev.get("phase") == "compacted":
                        conn.compactions.append(det)
            except Exception as e:  # the watcher must never kill the run
                print(f"  ** event watch unavailable: {e} **", flush=True)
            finally:
                await watcher.close()

        conn._tasks.append(asyncio.create_task(_wire_errspy()))
        return conn

    inbox = f"{aid}_errspy"
    root.ctx.ensure_inbox(inbox)
    root.watch(aid, inbox)

    async def _errspy():
        q = root.ctx.inboxes[inbox]
        while True:
            ev = await q.get()
            det = ev.get("detail") or {}
            if _died(ev, det):
                _failed(f"turn ended: {det.get('reason')}")
            if ev.get("type") == "status" and det.get("error"):
                _failed(det["error"])
            if ev.get("type") == "context" and ev.get("phase") == "too_small":
                _failed(f"context too_small: {det}")
            if ev.get("type") == "context" and ev.get("phase") == "compacted":
                conn.compactions.append(det)

    conn._tasks.append(asyncio.create_task(_errspy()))
    return conn


async def _live_tasks(root, aid) -> list[dict]:
    """The AI's in-flight rows, read from the ROOT — `ps` is a kernel verb."""
    rows = (
        (
            (await root.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0][
                "result"
            ]
        ).get("tasks")
        or []
    )
    return [t for t in rows if t.get("agent") == aid and t.get("verb") != "ps"]


#: What the fixture human answers. One string, so no probe invents its own — a
#: test that needs a DIFFERENT answer writes that agent's `answer` field.
HUMAN_ANSWER = "do your best judgement"


async def human(
    root,
    *,
    agent_id: str = "user",
    ai: str | list[str] = "ai",
    answer: str = HUMAN_ANSWER,
    **meta,
) -> dict:
    """Create THE HUMAN as an ordinary agent, both gate halves wired.

    ONE definition. Sixteen probes each writing their own `create_agent` for the
    person is sixteen copies to drift, and the wiring is the part that is easy to
    get wrong: the AI and the human are SIBLINGS, so there is no baseline edge
    between them and BOTH halves must be granted or the first task is refused at
    ingress and the chain never starts (measured 2026-09-04 on the first
    conversion: zero turns, a clean cap, nothing to read).

    The probe still grants its AI `can_call: {"user": "*"}` and passes
    `callable_by={"user": "*"}` to `connect` — that half belongs to the AI's own
    record, which this helper does not own.
    """
    # EVERY AI IT MAY TASK, not just one. A probe with a second session (a cold
    # agent, a reviewer) names its own ids; granting only `ai` left the second
    # one unreachable and its chain never started — zero turns, a clean cap,
    # nothing to read (measured 2026-09-04 on memory_distill).
    ais = [ai] if isinstance(ai, str) else list(ai)
    payload = {
        "type": "create_agent",
        "parent_id": root.id,
        "handler_module": "user_probe.tools",
        "id": agent_id,
        "answer": answer,
        "description": "The human this kernel belongs to.",
        "can_call": {a: "*" for a in ais},
        "callable_by": {**{a: "*" for a in ais}, "rest": "*", "web": "*"},
        **meta,
    }
    r = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
    assert not r.get("error"), f"create {agent_id}: {r}"
    return r


async def drive(
    conn: Connected, task: str, *, client_id: str = "probe", via: str | None = None
) -> str:
    """Send the task and run until the chain ends, stalls, or trips.

    Returns "" on a clean end, or the reason the run was cut.

    THE HARNESS NO LONGER ANSWERS QUESTIONS. It used to scrape
    `{"type":"inquiry"}` out of the model's prose and post a reply back in from
    outside the kernel, which is why the human had no id, no record, no gate and
    no ledger row. The human is an AGENT now (`e2e/probes/user_probe`) — the
    model calls it, the answer is the reply, and a fixture chooses what the human
    says by writing that agent's `answer` field.
    """
    root, aid, L = conn.root, conn.id, conn.limits
    # THE TRANSCRIPT FOLLOWS THE DRIVEN CLIENT. The wire reader polls durable
    # history by client_id and was born defaulting to "probe" — a probe driving
    # any other client got an empty transcript, a blind no-progress loop, and
    # transcript checks that fail while the model quietly succeeds (measured:
    # coding/file_edit:rust, n=0 with 6 rows under the real client).
    if getattr(conn.proxy, "client_id", None) is not None:
        conn.proxy.client_id = client_id

    # SURFACE A FAILED SEND — ai_core returns errors, it does not raise.
    # WHO GIVES THE MODEL ITS WORK. `via` names the HUMAN agent, and the task is
    # then a gated `user -> ai` send with its own ledger row — the wiring
    # production uses. Without it the task is an OPERATOR injection
    # (`sender=None`, ungated), which is the shape the suite has always used and
    # which no real deployment has.
    #
    # The operator still pokes the human once. That single step is the person
    # pressing enter; it is the only out-of-band action left, and it is honest.
    opening = (
        {
            "target": via,
            "payload": {
                "type": "send",
                "target": aid,
                "payload": {"type": "send", "text": task, "client_id": client_id},
            },
        }
        if via
        else {
            "target": aid,
            "payload": {"type": "send", "text": task, "client_id": client_id},
        }
    )
    first = (await root.send([opening]))[0]["result"]
    if isinstance(first, dict) and first.get("error"):
        print(f"\n  ** FIRST SEND FAILED: {first['error']} **\n", flush=True)

    idle, last, stale, seen_calls, capped = 0.0, 0, 0, -1, ""
    while idle < L.idle:
        await asyncio.sleep(2.0)
        calls = distinct_calls(conn.proxy.transcript)
        # NOTHING IS SCRAPED HERE ANY MORE. The harness used to read the model's
        # own prose for `{"type":"inquiry"}` and post an answer back in from
        # outside the kernel — the human had no id, no record, no gate and no
        # ledger row. The human is an AGENT now (`e2e/probes/user_probe`): the
        # model calls it and the answer is the reply, so a question needs the
        # driver no more than any other call does.
        if calls != seen_calls:
            seen_calls, stale = calls, 0
        elif conn.proxy.n != last:
            stale += 1

        # CHAIN DEATH. The per-turn tripwires only fire when a turn LANDS, so a
        # model that simply stops producing turns evades all of them and the run
        # sits out the full idle window before scoring.
        if idle >= 30 and conn.proxy.n == last and last > 0:
            rows = (
                (
                    (
                        await root.send(
                            [{"target": "kernel", "payload": {"type": "ps"}}]
                        )
                    )[0]["result"]
                ).get("tasks")
                or []
            )
            if not [t for t in rows if t.get("agent") == aid and t.get("verb") != "ps"]:
                capped = (
                    f"CHAIN ENDED — no turn and nothing in flight after {idle:.0f}s"
                    f" · ps={[(t.get('agent'), t.get('verb')) for t in rows]}"
                )
            else:
                # A generation IS in flight — that is activity, not idleness.
                # Without this hold, a single long turn (a big write after a big
                # read) outlived L.idle and the while-condition exited SILENTLY
                # mid-generation (measured 2026-09-01, gym bowling×3 + java: the
                # chain died at turn 3-4, stub never written, no cap printed).
                # Held at 28 so the in-flight check re-runs every tick; a truly
                # hung task still ends via the provider/kernel timeout, after
                # which the cap above fires normally.
                idle = 28.0

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
                c = (
                    await root.send(
                        [
                            {
                                "target": "kernel",
                                "payload": {"type": "cancel", "task_id": t["id"]},
                            }
                        ]
                    )
                )[0]["result"]
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
