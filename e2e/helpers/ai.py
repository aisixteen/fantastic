"""THE AI CONNECTOR — create the agent, wire it, drive it, stop it.

This is the other half of what every probe duplicated. Creating a live AI agent
is never one call: it is a record with a backend-shaped meta, its own `can`
lines (the one grant table), an api key for the backends that need one, a provider wrapped in the proxy reader, and a drive loop that knows
when a chain has ended — once, instead of a copy per probe.

The failure modes it handles:

  * `ps`/`cancel` are KERNEL verbs. Addressed to the AI agent they come back
    `not_here`, and a harness that discards the reply cancels NOTHING — capped
    runs are abandoned, not killed, and their turns keep running.
  * A dead chain announces itself as `status(reason=error)` on the AI's own
    inbox. With no subscriber it goes nowhere and a chain that died mid-flight
    reads as a model that chose to stop.
  * `ai_core` RETURNS `{"error": …}` rather than raising, so a first send that
    timed out would score as "the model produced nothing".
  * A fixed turn budget cannot tell "slower" from "worse" — under parallel load
    a probe can hit a turn cap mid-work it finishes solo. The cap counts turns
    since the last NEW call instead.

`can` is a REQUIRED argument (deny-all posture: a new agent holds no lines and
reaches only its own tree): there is no "sensible default toolset" to fall back
to, and a silent default is exactly how a probe would stop measuring what it
claims to measure.
"""

from __future__ import annotations

import asyncio
import os
import re
from dataclasses import dataclass, field
from typing import Any

from helpers.kernel import home
from helpers import creds, lines, memory
from helpers.stream import StreamJunction
from helpers.proxy import Limits, distinct_calls


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
    #: THE STREAM JUNCTION (`helpers/stream.py`) — the agent's streaming parts, as
    #: drafts settled by end/abort/match. In process only (None over a wire).
    streams: StreamJunction | None = None
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


# THE CHAT/PROVIDER SPLIT (#615, #632). An AI is TWO agents: the PROVIDER (`<id>_prov`, the
# backend record: endpoint, model, key, KV slots — its identity) completes turns and holds no
# conversation; the CHAT (`ai_chat.tools`) holds the conversation, every line and the request
# knobs it sends with each request (sampling, max_tokens), and completes through the provider
# its `provider_id` names, by its own line on it (`complete`, `watch`, `capabilities`).
# `make_chat` is the ONE place the suite builds an AI: a new pair, or a chat on a provider
# that is already there (`provider_id`): `connect` uses it, and so does every probe that
# wires its own AI.
CHAT_MODULE = "ai_chat.tools"
PROVIDER_CALLS = lines.PROVIDER
#: record fields the CHAT takes too (it sizes its own context)
CHAT_SHARED = ("model", "context_window", "caching")
#: the request knobs: the CHAT's alone, wherever a backend record or a probe puts them (#632)
KNOBS = ("temperature", "top_p", "top_k", "min_p", "reasoning_effort", "thinking_token_budget",
         "presence_penalty", "frequency_penalty", "repetition_penalty", "no_repeat_ngram_size",
         "seed", "stop", "max_tokens")
#: the fields every chat takes (ai_core's record): the CHAT's alone too
CHAT_ONLY = KNOBS + ("output_reserve", "context_strategy", "recent_n", "cache_primer_until",
                     "transport_retries", "misfire_limit", "system_prompt", "file_bridge_id",
                     "system_prompt_file", "pre_system_prompt", "post_system_prompt",
                     "image_max_bytes", "image_max_pixels")
#: record fields that are the PROVIDER's only, wherever a probe puts them
PROVIDER_ONLY = ("handler_module", "endpoint", "credentials", "num_ctx", "kv_slots",
                 "kv_overflow")


def split_record(backend_record: dict, extra: dict | None = None) -> tuple[dict, dict]:
    """(provider fields, chat fields) from a backend record and a probe's extra fields."""
    prov, chat = dict(backend_record), {}
    for k in CHAT_SHARED:
        if k in prov:
            chat[k] = prov[k]
    for k in CHAT_ONLY:
        if k in prov:
            chat[k] = prov.pop(k)
    for k, v in (extra or {}).items():
        if k == "num_ctx" and not str(prov.get("handler_module", "")).startswith("ollama"):
            continue  # the window ollama LOADS the model with; no other provider has the field
        if k in PROVIDER_ONLY:
            prov[k] = v
        else:
            chat[k] = v
            if k in CHAT_SHARED:
                prov[k] = v
    return prov, chat


def chat_pair(backend, chat: dict, *, role: str | None = None,
              provider_id: str | None = None, parent: str) -> tuple[dict, dict]:
    """The two create_agent payloads (provider, chat) for one AI. `chat` is the payload a
    probe would write for ONE agent (id, parent_id, can, memory, and any record fields);
    the chat's `can` gets its line on the provider; backend fields in it go to the provider, the request knobs to the chat.
    Pure: a sync runner (the host runner) sends them through its own wire; `make_chat` sends
    them through a root."""
    chat = dict(chat)
    chat.pop("type", None)
    cid = chat["id"]
    pid = provider_id or f"{cid}_prov"
    prov_fields, chat_fields = split_record(backend.record(role=role), chat)
    prov_fields.pop("id", None)
    for k in ("can", "parent_id", "memory_store_id", "description"):
        prov_fields.pop(k, None)
    prov = {
        "type": "create_agent", "parent_id": chat.get("parent_id") or parent, "id": pid,
        **prov_fields,
        "memory_store_id": memory.MEMORY,
        # only agents holding a line on `pid` reach it (the chat, below)
        "can": {memory.MEMORY: lines.ALL,
                **({creds.CREDS: lines.GET} if backend.key_ref else {})},
    }
    chat_fields["can"] = {**(chat_fields.get("can") or {}), pid: PROVIDER_CALLS}
    # the KEY is the provider's: the chat names no credentials and reads no `creds`
    chat_fields.pop("credentials", None)
    chat_fields["can"].pop(creds.CREDS, None)
    return prov, {"type": "create_agent", **chat_fields, "handler_module": CHAT_MODULE,
                  "provider_id": pid}


async def make_chat(root, backend, chat: dict, *, role: str | None = None,
                    provider_id: str | None = None) -> tuple[str, str]:
    """Create the chat, and the provider it names. THE CALLER NAMES THE PROVIDER (#632):
    `provider_id` is a provider that is already there — the chat is made on it, its own
    `can` carrying the line on it; none given, a new provider is made from the plug
    (`chat_pair`) first, so the chat's line names an id that exists.
    Returns (chat_id, provider_id)."""
    prov, payload = chat_pair(backend, chat, role=role, provider_id=provider_id,
                              parent=home(root))
    pid = prov["id"]
    # A REBOOTED kernel hydrates the pair from its state folder: what is already
    # there is that same pair, so only a missing half is created.
    live = set(root.agent_ids())
    if provider_id is not None:
        if pid not in live:
            raise SystemExit(f"make_chat: the provider {pid} it names is not there")
    elif pid not in live:
        made = (await root.send([{"target": root.id, "payload": prov}]))[0]["result"]
        if isinstance(made, dict) and made.get("error"):
            raise SystemExit(f"create_agent({pid}) refused: {made}")
    if payload["id"] in live:
        return payload["id"], pid
    made = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
    if isinstance(made, dict) and made.get("error"):
        raise SystemExit(f"create_agent({payload['id']}) refused: {made}")
    return made["id"], pid


async def connect(
    root,
    core,
    backend,
    *,
    can: dict,
    agent_id: str = "ai",
    limits: Limits | None = None,
    stream_to: str = "cli",
    record: dict | None = None,
    watch=None,
) -> Connected:
    """Create the AI agent with its own `can` lines, install the reader.

    `rest` (the leg) reaches the AI by position, `ws` by its seeded `rest` line;
    nothing is written on any target.
    """
    L = limits or Limits.from_env(os.environ)
    # `docs` IS `reflect` on the root (`tier`, `verb`, `part`): every AI the suite
    # connects reads docs by a `reflect` line on the root, merged into whatever
    # the fixture already gives the root. A higher level already includes `call`.
    have = can.get(lines.ROOT) or {}
    if not ({"update", "create"} & set(have)):
        can = {**can, lines.ROOT: {"call": sorted({*have.get("call", []), "reflect"})}}
    meta = {
        "type": "create_agent",
        "parent_id": home(root),
        "id": agent_id,
        "context_window": L.ctx,
        "num_ctx": L.ctx,
        # ITS MEMORY — the conversation lives in the kernel's `memory` store, named
        # on the record and reached by the AI's own line on it.
        **memory.ai_memory_meta(can),
        **(record or {}),
    }
    # THE PAIR: the provider from the backend record, the chat from the rest (make_chat).
    aid, _ = await make_chat(root, backend, meta)

    # NOTHING TO DELIVER. The record names `credentials` (the plug's ref in the
    # kernel's `creds` store, `helpers/creds.py`); the backend reads the key from
    # the store per request, the store from the variable the run exported.

    conn = Connected(id=aid, root=root, proxy=None, limits=L)  # type: ignore[arg-type]
    # Parts that go to the caller land on the agent's own inbox; a call part lands
    # on its addressee's. Watch both ends the run can see.
    if getattr(getattr(root, "ctx", None), "agents", None) is not None:
        live = root.ctx.agents
        conn.streams = StreamJunction.watch(
            root, *[a for a in (aid, stream_to) if a and a in live]
        )

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
        """A call that ENDED for any reason but `ok` (#714 phases).

        `failed` is an error, the per-send ceiling (`timeout`, NO error key) or
        retries run out (`gave_up`); `done{reason:"cancelled"}` is a cancel.
        `detail.error` alone is not enough: without this, a chain that died at the
        ceiling and a model that chose to stop write the identical transcript.
        Not an end: `stalled` (`dead_turn`, an empty answer the kernel REROLLS with
        the no-call nudge — capping on it would cancel live chains) and `awaiting`
        (the model's explicit wait: the next delivery wakes it).
        """
        if ev.get("type") != "status":
            return False
        return ev.get("phase") == "failed" or (
            ev.get("phase") == "done" and det.get("reason") not in (None, "ok")
        )

    def _errored(ev: dict, det: dict) -> bool:
        return errored_status(ev)

    def _ended(ev: dict) -> bool:
        return ev.get("type") == "status" and ev.get("phase") in ("done", "failed")

    def _note_end(det: dict) -> None:
        """EVERY CALL'S END, not only a failed one, so a chain that stopped after a
        clean end says HOW it ended."""
        print(f"  · turn ended: {det.get('reason') or 'ok'}", flush=True)

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
                    if _errored(ev, det):
                        _failed(str(det["error"]))
                    if _died(ev, det):
                        _failed(f"turn ended: {det.get('reason')}")
                    elif _ended(ev):
                        _note_end(det)
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
            elif _ended(ev):
                _note_end(det)
            if _errored(ev, det):
                _failed(det["error"])
            if ev.get("type") == "context" and ev.get("phase") == "too_small":
                _failed(f"context too_small: {det}")
            if ev.get("type") == "context" and ev.get("phase") == "compacted":
                conn.compactions.append(det)

    conn._tasks.append(asyncio.create_task(_errspy()))
    return conn


def errored_status(ev: dict) -> bool:
    """A status carrying `detail.error` that is not a `retrying` one — THE one rule every
    watcher trips on (`connect` here, and probes that watch a chain themselves).

    `retrying` (a malformed call re-asked, `kind: "call"`, or a dropped link
    retried) carries the error it retries; the call is still live, and
    tripping on it cancels the re-ask in flight. A call that then runs out of
    attempts ends `failed` or `done` with a reason, which a watcher catches as the end.
    """
    det = ev.get("detail") or {}
    return ev.get("type") == "status" and bool(det.get("error")) and ev.get("phase") != "retrying"


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


async def add_lines(root, aid: str, add: dict) -> dict:
    """Merge `add` into `aid`'s own `can` and write it back (`update_agent`
    replaces the whole field), as `rest`: every target `add` names must exist.
    Returns the `can` written."""
    import asyncio

    rec = await asyncio.to_thread(root.record, aid)
    if not rec:
        raise SystemExit(f"add_lines: {aid} is not reachable from {home(root)}")
    can = {**(rec.get("can") or {}), **add}
    r = (await root.send([{"target": root.id, "payload": {
        "type": "update_agent", "id": aid, "can": can}}]))[0]["result"]
    if not isinstance(r, dict) or r.get("updated") is not True:
        raise SystemExit(f"add_lines: update_agent {aid}.can={can} refused: {r}")
    return can


async def human_line(root, aid: str, *, human_id: str = "user") -> dict:
    """The human's line on `aid` (`human(ai=[])` made it with none), written once
    `aid` exists: a line on a missing id is refused. Returns the human's `can`."""
    return await add_lines(root, human_id, {aid: lines.ALL})


#: The bundle of a probe hand: `user_probe`'s `send` dispatches any payload AS the
#: hand, so a call through it is gated on the hand's own lines and position.
HAND_MODULE = "user_probe.tools"


async def probe_agent(root, aid: str, *, can: dict | None = None,
                      parent: str | None = None) -> dict:
    """Create a HAND: a `user_probe` agent `aid` under `parent` (the home when
    none), holding exactly the lines `can` (none when absent). A probe sends
    through it with `relay` to see what those lines and that position reach,
    before any model runs; the probe deletes it when done."""
    payload = {
        "type": "create_agent", "parent_id": parent or home(root), "id": aid,
        "handler_module": HAND_MODULE,
        **({"can": can} if can else {}),
    }
    r = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
    if not isinstance(r, dict) or r.get("error"):
        raise SystemExit(f"probe_agent: create {aid} refused: {r}")
    return r


async def relay(root, via: str, target: str, payload: dict):
    """Send `payload` to `target` AS `via` (a hand or the human: `user_probe`'s
    `send`) and return the target's answer. When the hop to `via` itself fails
    (no `{sent, result}`), the answer is `{"error", "relay": <that reply>}`, so a
    refusal of the hop never reads as the target's answer."""
    out = (await root.send([{"target": via, "payload": {
        "type": "send", "target": target, "payload": payload}}]))[0]["result"]
    if isinstance(out, dict) and out.get("sent") == target and "result" in out:
        return out["result"]
    return {"error": f"relay: the send through {via} failed", "relay": out}


async def human(
    root,
    *,
    agent_id: str = "user",
    ai: str | list[str] = "ai",
    answer: str = HUMAN_ANSWER,
    **meta,
) -> dict:
    """Create THE HUMAN as an ordinary agent, with its line on each AI.

    ONE definition. Sixteen probes each writing their own `create_agent` for the
    person is sixteen copies to drift, and the wiring is the part that is easy to
    get wrong: the AI and the human are SIBLINGS, so neither reaches the other by
    position and siblings need a line each way, or the first task is refused and
    the chain never starts (zero turns, a clean cap, nothing to read).

    The AI's `user` line comes from `connect(can=…)` — it belongs to the AI's own
    record, which this helper does not own. Every AI named here must already exist
    (an agent writing a line on a missing id is refused): a probe whose AI names the
    human makes the human first (`ai=[]`) and adds its line once the AI exists
    (`human_line`).
    """
    # EVERY AI IT MAY TASK, not just one. A probe with a second session (a cold
    # agent, a reviewer) names its own ids; granting only `ai` would leave the
    # second one unreachable and its chain would never start.
    ais = [ai] if isinstance(ai, str) else list(ai)
    payload = {
        "type": "create_agent",
        "parent_id": home(root),
        "handler_module": "user_probe.tools",
        "id": agent_id,
        "answer": answer,
        "description": "The human this kernel belongs to.",
        "can": {a: lines.ALL for a in ais},
        **meta,
    }
    r = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
    assert not r.get("error"), f"create {agent_id}: {r}"
    return r


#: Seconds of quiet after a `done` call before the chain is checked for the end.
DONE_QUIET = 1.0
#: The drive loop's poll step. The wire transcript throttles its own store read
#: to 1 s, and `ps` is held to PS_EVERY, so a short step costs no extra load.
TICK = 0.5
#: Minimum seconds between two `ps` checks of one chain.
PS_EVERY = 2.0
#: The model's `done` call, in its last turn.
_DONE_CALL = re.compile(r'<call\s+name="done"')
_AWAIT_CALL = re.compile(r'<call\s+name="await"')
#: Kernel service work and the driver's own calls — never a peer still working.
_QUIET_VERBS = {"ps", "persist_writer", "flush_executions", "ws_drain", "inbound_call"}
#: Messages a test has SCHEDULED for an agent (`facts_arm.send_later`) and not yet
#: sent, by agent id. A chain with one pending is not over: ending it on quiet would
#: cut a multi-task test after its first task, before the later tasks arrive.
LATER: dict[str, set] = {}


#: Nothing in flight anywhere for this long ends a wait on a swarm. The swarm tests
#: run no clock, so a quiet kernel stays quiet — without this a swarm that never
#: started would sit out its whole window.
IDLE_END = 30.0


async def busy(root) -> list[tuple]:
    """`(agent, verb)` of the work in flight anywhere — every `ps` row but kernel
    service work and the driver's own calls."""
    out = await root.send([{"target": "kernel", "payload": {"type": "ps"}}])
    rows = (out[0].get("result") or {}).get("tasks") or []
    return [(t.get("agent"), t.get("verb")) for t in rows if t.get("verb") not in _QUIET_VERBS]


class QuietWatch:
    """True from `quiet()` once nothing has been in flight for `IDLE_END` seconds,
    measured across calls — call it from a polling loop."""

    def __init__(self, root, idle_end: float = IDLE_END):
        self.root, self.idle_end = root, idle_end
        self.since: float | None = None

    async def quiet(self) -> bool:
        now = asyncio.get_running_loop().time()
        if await busy(self.root):
            self.since = None
            return False
        self.since = now if self.since is None else self.since
        return now - self.since >= self.idle_end


@dataclass
class ChainEnd:
    """Has the chain ENDED? The one check every drive loop shares.

    A chain is over when no turn has landed for 30 s and nothing of the AI's is
    in flight — or, once its last turn called `done`, after DONE_QUIET seconds
    and nothing in flight ANYWHERE (no sub-AI turn, job or tick of a peer), so a
    test whose peers still report back keeps the 30 s rule. A chain whose last turn
    called `await` stays open while any peer has work in flight (the 15-min hard
    stop still bounds it). Waiting the 30 s window after the model has already
    called done would add that much to every test's wall.

    `check` is called only while no new turn has landed; it reads `ps` at most
    every PS_EVERY seconds. Returns (cap, busy): `cap` is the CHAIN ENDED
    reason, `busy` says the AI itself has work in flight.
    """

    root: Any
    aid: str
    ps_at: float = 0.0

    async def check(self, transcript: list[str], idle: float) -> tuple[str, bool]:
        said_done = bool(transcript) and bool(_DONE_CALL.search(transcript[-1] or ""))
        # AWAITING: the AI asked to be woken by what is in flight — an inquiry's
        # later answer, a peer's report. While a peer works, its chain is not over.
        awaited = bool(transcript) and bool(_AWAIT_CALL.search(transcript[-1] or ""))
        if LATER.get(self.aid):
            said_done = False  # the test still has a message to send this agent
        if not (idle >= 30 or (said_done and idle >= DONE_QUIET)):
            return "", False
        now = asyncio.get_running_loop().time()
        if now - self.ps_at < PS_EVERY:
            return "", False
        self.ps_at = now
        rows = (
            (
                (
                    await self.root.send(
                        [{"target": "kernel", "payload": {"type": "ps"}}]
                    )
                )[0]["result"]
            ).get("tasks")
            or []
        )
        if [t for t in rows if t.get("agent") == self.aid and t.get("verb") != "ps"]:
            return "", True
        peers = [
            t
            for t in rows
            if t.get("agent") != self.aid and t.get("verb") not in _QUIET_VERBS
        ]
        if (idle < 30 or awaited) and peers:
            return "", False  # said done / awaiting, and a peer is still working
        # DUMP THE TABLE. "no turn and nothing in flight" is a conclusion; the
        # rows are the evidence. Without them a chain that ended and a detector
        # that fired too early look identical from the transcript.
        return (
            f"CHAIN ENDED — no turn and nothing in flight after {idle:.0f}s"
            f"{' (the model called done)' if idle < 30 else ''}"
            f" · ps={[(t.get('agent'), t.get('verb')) for t in rows]}"
        ), False


async def drive(
    conn: Connected, task: str, *, client_id: str = "probe", via: str | None = None
) -> str:
    """Send the task and run until the chain ends, stalls, or trips.

    Returns "" on a clean end, or the reason the run was cut.

    THE HARNESS DOES NOT ANSWER QUESTIONS. The human is an AGENT
    (`e2e/probes/user_probe`) with an id, a record, a gate and ledger rows — the
    model calls it, the answer comes back later under its pid, and a fixture chooses what the human
    says by writing that agent's `answer` field.
    """
    root, aid, L = conn.root, conn.id, conn.limits
    # THE TRANSCRIPT FOLLOWS THE DRIVEN CLIENT. The wire reader polls durable
    # history by client_id and was born defaulting to "probe" — a probe driving
    # any other client would get an empty transcript, a blind no-progress loop,
    # and transcript checks that fail while the model quietly succeeds.
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
    if via and isinstance(first, dict) and isinstance(first.get("result"), dict):
        # THE HUMAN'S SEND SUCCEEDS as its own call; the AI's answer — a refusal
        # included — rides inside as `{sent, result}`.
        first = first["result"]
    if isinstance(first, dict) and first.get("error"):
        print(f"\n  ** FIRST SEND FAILED: {first['error']} **\n", flush=True)

    idle, last, stale, seen_calls, capped = 0.0, 0, 0, -1, ""
    ender = ChainEnd(root, aid)
    while idle < L.idle:
        await asyncio.sleep(TICK)
        calls = distinct_calls(conn.proxy.transcript)
        # NOTHING IS SCRAPED HERE. The human is an AGENT (`e2e/probes/user_probe`): the
        # model calls it and the answer arrives later under its pid, so a question needs the
        # driver no more than any other call does.
        if calls != seen_calls:
            seen_calls, stale = calls, 0
        elif conn.proxy.n != last:
            stale += 1

        # CHAIN DEATH. The per-turn tripwires only fire when a turn LANDS, so a
        # model that simply stops producing turns evades all of them and the run
        # sits out the full idle window before scoring. See `ChainEnd`.
        if conn.proxy.n == last and last > 0:
            cap, busy = await ender.check(conn.proxy.transcript, idle)
            if cap:
                capped = cap
            elif busy and idle >= 30:
                # A generation IS in flight — that is activity, not idleness.
                # Without this hold, a single long turn (a big write after a big
                # read) could outlive L.idle and the while-condition would exit
                # SILENTLY mid-generation.
                # Held at 28 so the in-flight check re-runs; a truly hung task
                # still ends via the provider/kernel timeout, after which the cap
                # above fires normally.
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
            idle += TICK

    if capped:
        print(f"\n  ** CAPPED: {capped} · {conn.proxy.n} passes **")
    if conn.streams is not None:
        conn.streams.drain()
        print(f"  streams: {conn.streams.summary()}", flush=True)
    return capped
