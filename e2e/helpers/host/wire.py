"""Drive an AI agent on a SPAWNED kernel — the wire half of `helpers/ai.py`.

The in-process connector reaches inside the kernel: it wraps the provider
object (`WRAPS[aid] = wrap_with(proxy)`) to capture the transcript and taps
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

#: Message text the kernel itself wrote (a routed reply's header), not the model.
_ROUTED = re.compile(r"^\[(reply to|context-notice)")


class WireRoot:
    """`root`-shaped adapter over a spawned kernel's REST face."""

    #: The REST leg the driver arrives as. Seeded by `HostKernel.seed`.
    leg_id = "rest"

    def __init__(self, k: Any):
        self.k = k
        self.id = k.root_id

    async def send(self, items: list[dict]) -> list[dict]:
        """The batch shape `connect()`/`drive()` use. One POST per item — the
        REST face takes a single target per request, so a batch is a loop; the
        callers only ever send one.

        OFF THE EVENT LOOP. `HostKernel.send` is blocking `urllib`, and awaiting
        it inline would freeze the whole drive loop — its tripwires, its idle
        counter and its `ps` check all stop ticking while one request is
        outstanding.
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
        socket there is no kernel object, so this provides one.

        This is NOT the in-process bypass wearing a wire coat: every method here
        goes through the memory store's own verbs (`dump`/`inject`), as the leg
        the store is a child of. One probe, one code path, every runtime — which is the
        rule the scenario suite exists under.

        ONE PER ROOT: it holds the placeholder-id counter, and a fresh WireCtx per
        access would restart it — every `root.ctx.mint_id()` call would return the
        same id.
        """
        if getattr(self, "_ctx", None) is None:
            self._ctx = WireCtx(self.k)
        return self._ctx

    def record(self, aid: str) -> dict:
        """`aid`'s record as `rest` sees it (`reflect`): whole for agents in
        `rest`'s tree. `{}` when the kernel answers the unknown answer."""
        return self.ctx.record(aid)

    def agent_ids(self) -> list[str]:
        """Every id `rest` sees (`reflect tree=ids` on the root, paged)."""
        return self.ctx.agent_ids()

    def watch(self, *_a, **_kw) -> None:
        """No-op. Status-event spying needs an inbox subscription this face does
        not carry; a dead chain is caught by the drive loop's `ps` check
        instead, which works on every runtime."""

    async def close(self) -> None:
        self.k.close()


class WireTranscript:
    """The agent's transcript, read from its durable history.

    `n` counts ASSISTANT passes — the same thing the in-process reader counts
    (one per provider pass), so the tripwires and `distinct_calls` see the same
    numbers on every runtime. Kernel-authored messages (a routed reply's header)
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

        # The drive loop ticks every 0.5 s; a store dump more often than once a
        # second only burns requests.
        if time.monotonic() - self._last_poll < 1.0:
            return
        self._last_poll = time.monotonic()
        # BOUNDED. A poll is a status check, not the work — it must fail fast
        # and let the loop keep counting rather than inherit the driver's
        # 300-second default (see `WireRoot.send`).
        # THE STORE, not the backend's `history` verb: `history` answers a DIGEST
        # page and never the text, so the text this reader scores is read from the
        # agent's memory store — the same rows the in-process reader walks.
        # THE AGENT'S MEMORY STORE, read from its `dump` — the same rows
        # `_load_history` reads in process.
        rows = WireCtx(self.k).rows_for(self.aid)
        if not rows:
            return
        msgs = [m for m in rows if m.get("kind") not in WireCtx.BOOKKEEPING]
        # PRINT BOTH SIDES, count ONE. `n` and `transcript` stay assistant-only so
        # the tripwires and `distinct_calls` see exactly what the in-process
        # reader sees — but a routed reply arrives as a USER message, and without
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
        passes = [
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
        #
        # READ PAST `__getattribute__`. A read of `transcript`/`n` from here would
        # poll again from inside this poll; the 1 s throttle hid that only while a
        # dump took under 1 s, and an hour-long run's dump does not.
        transcript = object.__getattribute__(self, "transcript")
        if len(passes) > len(transcript):
            fresh = passes[len(transcript):]
            transcript.extend(fresh)
            self.n = n = len(transcript)
            # PRINT THE PASS, as the in-process reader does. Without this a wire
            # run's log carries a verdict and nothing to explain it — the whole
            # transcript is on the far side of a socket.
            for i, t in enumerate(fresh, start=n - len(fresh) + 1):
                box(f"-- pass {i} - ASSISTANT <-", t, ">")

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
    """A reader over a spawned kernel's REST face, for the scorers.

    The kernel keeps no conversation: an agent's rows live in the `memory` store,
    read here from the store's `dump` (a caller reads only its own key; the driver's
    `rest` leg is the store's parent — see `HostKernel.seed`). BLOCKING, like `WireTranscript._refresh`: the scorers that
    use it are plain synchronous functions, and every call is bounded.
    """

    #: Rows in an agent's store that are not conversation.
    BOOKKEEPING = ("view",)

    #: Placeholder ids start here — far above anything the kernel's counter reaches
    #: in a test, so a remap tells a placeholder from a real id by value alone.
    PLACEHOLDER_BASE = 1_000_000_000

    def __init__(self, k: Any):
        self.k = k
        self._next_placeholder = self.PLACEHOLDER_BASE

    def mint_id(self) -> int:
        """A PLACEHOLDER id for a row about to be planted. In process a probe mints a
        real id from the kernel's counter; over the wire it cannot, so it plants rows
        WITHOUT ids (the store takes the kernel's next one) and `facts_arm.plant_store`
        remaps every reference from these placeholders to the ids assigned."""
        self._next_placeholder += 1
        return self._next_placeholder

    def _store(self, payload: dict, timeout: float = 30) -> dict:
        r = self.k.send("memory", payload, timeout=timeout)
        return r if isinstance(r, dict) else {}

    def _kernel(self, payload: dict, timeout: float = 30) -> dict:
        r = self.k.send("kernel", payload, timeout=timeout)
        return r if isinstance(r, dict) else {}

    def rows_for(self, aid: str) -> list[dict]:
        """That agent's rows WHOLE, oldest first, from the store's `dump` — a caller
        reads only its OWN rows, and the leg is not `aid`. An agent never written to
        answers an empty list — a legitimate zero."""
        r = self._store({"type": "dump"})
        tables = r.get("tables") if isinstance(r, dict) else None
        if not isinstance(tables, dict):
            return []
        rows = sorted(
            (
                x["row"]
                for x in tables.get("rows") or []
                if isinstance(x.get("owner"), str) and x["owner"].split("|", 1)[-1] == aid
            ),
            key=lambda row: row.get("id") or 0,
        )
        return [{**x, "agent": aid} for x in rows]

    def record(self, aid: str) -> dict:
        """`aid`'s record from `reflect` (whole inside `rest`'s tree); `{}` on the
        unknown answer."""
        from helpers.lines import is_unknown

        r = self.k.send(aid, {"type": "reflect"}, timeout=30)
        if not isinstance(r, dict) or is_unknown(r, aid):
            return {}
        return r

    def agent_ids(self) -> list[str]:
        """Every id of the root's `reflect tree=ids`, following `next`. `rest`
        holds `reflect` on the root, which covers every agent at `call`."""
        ids: list[str] = []
        after: str | None = None
        while True:
            payload = {"type": "reflect", "tree": "ids"}
            if after:
                payload["after"] = after
            r = self._kernel(payload)
            tree = r.get("tree")
            if not isinstance(tree, list):
                return ids
            ids += [x if isinstance(x, str) else x.get("id") for x in tree
                    if isinstance(x, str) or (isinstance(x, dict) and x.get("id"))]
            after = r.get("next")
            if not after:
                return ids

    @property
    def tasks(self) -> list:
        """The live task table — `ps`. A list, so `len(root.ctx.tasks)` reads the
        same number it reads in process."""
        r = self._kernel({"type": "ps"})
        t = r.get("tasks")
        return t if isinstance(t, list) else []

    def view_for(self, aid: str) -> dict | None:
        """That agent's VIEW — the newest `view` row in its store, the same
        document `_load_view` reads."""
        latest = None
        for r in self.rows_for(aid):
            if r.get("kind") == "view" and isinstance(r.get("ids"), list):
                latest = r
        return latest

    @property
    def next_id(self) -> int:
        """At least the id the kernel mints NEXT: the root record's reserved bound
        (`next_id`, written by `kernel_start`) — the counter's durable half. Root
        fields are operator only, so it is read from the state folder (harness-
        owned, the trust root), not from a `reflect`."""
        import yaml

        path = self.k.workdir / ".fantastic" / "agent.yaml"
        return int((yaml.safe_load(path.read_text()) or {}).get("next_id") or 1)

    async def shutdown(self) -> dict:
        """The kernel's TEARDOWN — the wire twin of the in-process
        `Kernel.shutdown()`. Process verbs are operator only, so over REST they
        are refused; the stop is the operator's signal instead (SIGTERM, a
        graceful shutdown), which must end the process with exit 0."""
        import asyncio

        rc = await asyncio.to_thread(self.k.stop)
        if rc != 0:
            raise AssertionError(f"kernel stop: exit {rc}, not 0")
        return {"exit": rc}
