"""Drive an AI agent on a SPAWNED kernel — the wire half of `helpers/ai.py`.

The in-process connector reaches inside the kernel: it wraps the provider
object (`core._providers[aid] = proxy`) to capture the transcript and taps
`root.ctx.inboxes` for status events. Neither exists across a socket, so this
module supplies the same two things over the REST face:

  * `WireRoot`   — `.id` / `.send([...])`, the shape `connect()` and `drive()`
                   already speak, backed by `HostKernel`.
  * `WireTranscript` — the transcript read from the agent's OWN durable history
                   instead of from a wrapped provider.

That keeps ONE connector and ONE drive loop for every runtime. The alternative
— a python-only in-process path beside a wire path — is exactly what made the
scenario suite measure a different thing on python than on rust, and it hid
four real product bugs until the two were unified.
"""

from __future__ import annotations

import asyncio
import re
import time
from typing import Any

#: Turn text the kernel itself wrote (a routed reply's header), not the model.
_ROUTED = re.compile(r"^\[(reply to|context-notice)")


class WireRoot:
    """`root`-shaped adapter over a spawned kernel's REST face."""

    #: The REST leg the driver arrives as. Seeded by `HostKernel.seed`.
    leg_id = "rest"
    #: The agents a `watch` frame may be dispatched AS — each needs the target's
    #: ingress like anyone. Python (canonical) and swift dispatch it as the WS LEG
    #: (`web_ws`'s own id, the record its ingress rule hangs on); rust dispatches
    #: it as the web HOST (`fantastic-web/src/lib.rs`, `with_sender(host_agent_id)`)
    #: — a rust deviation to fix, not a second contract. Until it is, the fixture
    #: grants both so the SAME probe measures the model on every runtime.
    watch_ids = ("ws", "web")

    def __init__(self, k: Any):
        self.k = k
        self.id = k.root_id

    async def send(self, items: list[dict]) -> list[dict]:
        """The batch shape `connect()`/`drive()` use. One POST per item — the
        REST face takes a single target per request, so a batch is a loop; the
        callers only ever send one.

        OFF THE EVENT LOOP. `HostKernel.send` is blocking `urllib`, and awaiting
        it inline froze the whole drive loop — its tripwires, its idle counter
        and its `ps` check all stop ticking while one request is outstanding.
        Measured on `plan:rust`: a turn started an exec_bridge script that
        POSTed back into the kernel, the next poll blocked on the driver's
        300-second default, and a probe with `idle_seconds: 40` sat still for
        eleven minutes without ever capping.
        """
        out = []
        for i in items:
            r = await asyncio.to_thread(self.k.send, i["target"], i["payload"])
            # KEEP THE BODY. The REST face answers a refusal as HTTP 400 with the
            # reason in the body; collapsing that to "HTTP 400" loses the only
            # part worth reading, and callers print `result["error"]`.
            if isinstance(r, dict) and r.get("error") and r.get("body"):
                r = {**r, "error": f"{r['error']}: {r['body']}"}
            out.append({"result": r})
        return out

    @property
    def ctx(self) -> "WireCtx":
        """THE STORE, REACHED OVER THE WIRE.

        A probe's SCORER reads the row store directly — a conversation whole, a
        planted history, the id counter. In-process that is `root.ctx`; across a
        socket there is no kernel object, and every such probe died at its last
        step with `'WireRoot' object has no attribute 'ctx'` AFTER the run had
        finished and paid for itself (measured 2026-09-11: `subai` and
        `subai_graph`, 27 minutes each).

        This is NOT the in-process bypass wearing a wire coat: every method here
        goes through the kernel's own `rows_read` / `rows_write` verbs, which the
        driver is granted by name (`ROOT_VERBS`) precisely because they are
        privileged. One probe, one code path, every runtime — which is the rule
        the scenario suite exists under.
        """
        return WireCtx(self.k)

    def watch(self, *_a, **_kw) -> None:
        """No-op. Status-event spying needs an inbox subscription this face does
        not carry; a dead chain is caught by the drive loop's `ps` check
        instead, which works on every runtime."""

    async def close(self) -> None:
        self.k.close()


class WireTranscript:
    """The agent's transcript, read from its durable history.

    `n` counts ASSISTANT turns — the same thing the in-process reader counts
    (one per provider pass), so the tripwires and `distinct_calls` see the same
    numbers on every runtime. Kernel-authored turns (a routed reply's header)
    are user-role and never counted as passes.
    """

    def __init__(self, k: Any, aid: str, client_id: str = "probe"):
        self.k, self.aid, self.client_id = k, aid, client_id
        self.n = 0
        self.transcript: list[str] = []
        self._seen_user = 0
        self._last_poll = 0.0

    def _refresh(self) -> None:
        from helpers.proxy import box

        # The drive loop ticks every 2s; polling faster only burns requests.
        if time.monotonic() - self._last_poll < 1.0:
            return
        self._last_poll = time.monotonic()
        # BOUNDED. A poll is a status check, not the work — it must fail fast
        # and let the loop keep counting rather than inherit the driver's
        # 300-second default (see `WireRoot.send`).
        # THE STORE, not the backend's `history` verb: python's `history` answers
        # a DIGEST page ({rows:[{id, head, bytes…}], next, total}) and never the
        # bodies, so the text this reader scores lives only in the kernel's
        # `rows_read` — the same rows the in-process reader walks, bookkeeping
        # (call/task/body/view/settle) filtered the same way.
        r = self.k.send(
            "kernel", {"type": "rows_read", "agent": self.aid}, timeout=15)
        rows = r.get("rows") if isinstance(r, dict) else None
        if not isinstance(rows, list):
            return
        msgs = [m for m in rows if m.get("kind") not in WireCtx.BOOKKEEPING]
        # PRINT BOTH SIDES, count ONE. `n` and `transcript` stay assistant-only so
        # the tripwires and `distinct_calls` see exactly what the in-process
        # reader sees — but a routed reply arrives as a USER turn, and without
        # showing it there is no way to tell "the model got its answer and chose
        # to stop" from "the answer never came back". Both end the chain with
        # nothing in flight and no error.
        incoming = [
            str(m.get("content") or "")
            for m in msgs
            if m.get("role") == "user"
        ]
        if len(incoming) > self._seen_user:
            for t in incoming[self._seen_user:]:
                box("-- USER ->", t, "<")
            self._seen_user = len(incoming)
        turns = [
            str(m.get("content") or "")
            for m in msgs
            if m.get("role") == "assistant"
        ]
        # EXTEND IN PLACE — never rebind. Callers capture the list ONCE
        # (`transcript = spy.transcript`) and read it for the rest of the run,
        # exactly as they do with the in-process reader, which appends. Assigning
        # a fresh list here left every captured reference empty forever: a run
        # scored 8 passes with a 0-char transcript and no planning markers, which
        # reads as a model that produced nothing.
        #
        # Append-only is safe: history is never trimmed, so what is already
        # recorded cannot change under us.
        if len(turns) > len(self.transcript):
            fresh = turns[len(self.transcript):]
            self.transcript.extend(fresh)
            self.n = len(self.transcript)
            # PRINT THE TURN, as the in-process reader does. Without this a wire
            # run's log carries a verdict and nothing to explain it — the whole
            # transcript is on the far side of a socket.
            for i, t in enumerate(fresh, start=self.n - len(fresh) + 1):
                box(f"-- turn {i} - ASSISTANT <-", t, ">")

    @property
    def text(self) -> str:
        self._refresh()
        return "".join(self.transcript)

    def sees_prompt(self) -> bool:
        """The system block is assembled INSIDE the kernel and never crosses
        this face, so it cannot be inspected here. Reported honestly rather
        than guessed at."""
        return False

    def __getattribute__(self, name):
        # `n` and `transcript` are read directly by the drive loop's tripwires;
        # refresh on access so a poll happens without the loop knowing which
        # transport it is on.
        if name in ("n", "transcript"):
            object.__getattribute__(self, "_refresh")()
        return object.__getattribute__(self, name)


class WireCtx:
    """`root.ctx`-shaped reader over a spawned kernel's REST face.

    BLOCKING, like `WireTranscript._refresh` and for the same reason: the
    scorers that use it are plain synchronous functions called from async code,
    and `HostKernel.send` is blocking `urllib` anyway. Every call is bounded, so
    a dead kernel fails the check rather than hanging the probe.
    """

    #: Kernel bookkeeping — in the same table, never the conversation.
    BOOKKEEPING = ("call", "task", "body", "view", "settle")

    #: Placeholder ids start here — far above anything the kernel's counter will
    #: reach in a test, so a remap can tell a placeholder from a real id by value
    #: alone. See `mint_id`.
    PLACEHOLDER_BASE = 1_000_000_000

    def __init__(self, k: Any):
        self.k = k
        self._next_placeholder = self.PLACEHOLDER_BASE

    def mint_id(self) -> int:
        """A PLACEHOLDER id for a row that is about to be planted.

        In process this returns a real id from the kernel's counter, because the
        planter can write a row under an id it chose. Over the wire it cannot:
        `rows_write` refuses a row that carries its own `id` on purpose — the
        store is the only allocator, and re-ids are the caller's to remap.

        So the wire mints a placeholder and `facts_arm.plant_store` remaps it to
        the id the store actually assigned, writing rows in dependency order. A
        probe writes the SAME code either way; only the remap differs, and in
        process it is the identity.
        """
        self._next_placeholder += 1
        return self._next_placeholder

    def _kernel(self, payload: dict, timeout: float = 30) -> dict:
        r = self.k.send("kernel", payload, timeout=timeout)
        return r if isinstance(r, dict) else {}

    def rows_for(self, aid: str) -> list[dict]:
        """That agent's rows WHOLE, oldest first — the kernel's `rows_read`.

        An agent that has never been written to answers with an empty list, not
        an error: a scorer counting turns on an agent the model never built is
        asking a legitimate question and the answer is zero.
        """
        r = self._kernel({"type": "rows_read", "agent": aid})
        rows = r.get("rows")
        return rows if isinstance(rows, list) else []

    def rows_write(self, aid: str, rows: list[dict]) -> list[int]:
        """Append rows under FRESH ids — the kernel's `rows_write`. Returns the
        ids the store assigned, in order. The store is the only allocator."""
        r = self._kernel({"type": "rows_write", "agent": aid, "rows": rows})
        ids = r.get("ids")
        return ids if isinstance(ids, list) else []

    def body_get(self, pid: int, offset: int = 0, length: int = 4096):
        """One page of a stored reply body by pid — read off the `body` row the
        store keeps, which `rows_read` returns whole. `None` when nothing is
        stored under that pid."""
        for owner in self.agent_ids():
            for r in self.rows_for(owner):
                if r.get("kind") == "body" and r.get("parent") == pid:
                    text = str(r.get("content") or "")
                    chunk = text[offset:offset + length]
                    return chunk, offset + len(chunk), offset + len(chunk) >= len(text), len(text)
        return None

    def agent_ids(self) -> list[str]:
        r = self._kernel({"type": "list_agents"})
        agents = r.get("agents")
        if not isinstance(agents, list):
            return []
        return [a.get("id") for a in agents if isinstance(a, dict) and a.get("id")]

    @property
    def tasks(self) -> list:
        """The live task table — `ps`. A list, so `len(root.ctx.tasks)` reads the
        same number it reads in process."""
        r = self._kernel({"type": "ps"})
        t = r.get("tasks")
        return t if isinstance(t, list) else []

    def view_for(self, aid: str) -> dict | None:
        """That agent's VIEW — the newest `view` row in its store. There is no
        verb that returns it directly; the view IS a row, and `rows_read` carries
        rows whole, so this reads the same document `_load_view` reads."""
        latest = None
        for r in self.rows_for(aid):
            if r.get("kind") == "view" and isinstance(r.get("ids"), list):
                latest = r
        return latest

    @property
    def next_id(self) -> int:
        """The id the store will mint NEXT. Derived from the highest id in the
        table, which is what the counter guarantees — there is no verb that
        exposes the counter itself, and inventing one to satisfy a probe would
        be a product change made for a test."""
        top = 0
        for aid in self.agent_ids():
            for r in self.rows_for(aid):
                rid = r.get("id")
                if isinstance(rid, int) and rid > top:
                    top = rid
        return top + 1

    async def shutdown(self) -> dict:
        """The kernel's TEARDOWN — the wire twin of the in-process
        `Kernel.shutdown()`. Over the wire the kernel is a process, so this is
        `shutdown_kernel` (the root-only verb that stops it) followed by waiting
        for that process to be gone. The agent half (`shutdown_agents`) is the
        probe's own call, made before this exactly as it is in process."""
        import asyncio

        ack = await asyncio.to_thread(
            self.k.send, "kernel", {"type": "shutdown_kernel"}, 30)
        proc = getattr(self.k, "proc", None)
        if proc is not None:
            for _ in range(300):  # the verb defers its exit ~100ms; wait on the PROCESS
                if proc.poll() is not None:
                    break
                await asyncio.sleep(0.1)
            else:
                raise AssertionError("shutdown_kernel acked but the kernel process is still alive")
        return ack if isinstance(ack, dict) else {"ok": ack}
