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

import re
import time
from typing import Any

#: Turn text the kernel itself wrote (a routed reply's header), not the model.
_ROUTED = re.compile(r"^\[(reply to|no call was dispatched|context-notice)")


class WireRoot:
    """`root`-shaped adapter over a spawned kernel's REST face."""

    def __init__(self, k: Any):
        self.k = k
        self.id = k.root_id

    async def send(self, items: list[dict]) -> list[dict]:
        """The batch shape `connect()`/`drive()` use. One POST per item — the
        REST face takes a single target per request, so a batch is a loop; the
        callers only ever send one."""
        return [{"result": self.k.send(i["target"], i["payload"])} for i in items]

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
        self._last_poll = 0.0

    def _refresh(self) -> None:
        # The drive loop ticks every 2s; polling faster only burns requests.
        if time.monotonic() - self._last_poll < 1.0:
            return
        self._last_poll = time.monotonic()
        h = self.k.send(self.aid, {"type": "history", "client_id": self.client_id})
        msgs = h.get("messages") if isinstance(h, dict) else None
        if not isinstance(msgs, list):
            return
        turns = [
            str(m.get("content") or "")
            for m in msgs
            if m.get("role") == "assistant"
        ]
        # Append-only: history is never trimmed, so what is already recorded
        # cannot change under us.
        if len(turns) > len(self.transcript):
            self.transcript = turns
            self.n = len(turns)

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
