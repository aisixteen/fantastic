"""WHAT THE MODEL SAID — one interface, two ways of finding out.

In process we can WRAP the provider: `WRAPS[aid] = wrap_with(ProxyReader(...))`
(`helpers/kernel.py`) puts us between `ai_core` and the model, so we see the
prompt going in and the text coming out. Out of process there is nothing to wrap — the kernel is another
program on the other side of a socket — so the same facts have to come from the
kernel's own event stream instead.

They are not the same facts, and pretending otherwise would be the bug:

  IN PROCESS  prompt + completion. We see what the model was GIVEN.
  OVER A WIRE completion only. The stream carries what the agent EMITTED.

Scoring never reads the prompt — every probe counts tool-call envelopes in the
ASSISTANT text — so the wire view is sufficient for a verdict. It is not
sufficient for `SHOW_SYSTEM=1`, and a wire source says so rather than printing an
empty box and letting you conclude the system block was empty.

Both sources expose the same three things the drive loop needs: how many turns
have landed (`n`), the assistant text so far (`transcript`), and a list of trips
to abort on.
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable


@runtime_checkable
class TranscriptSource(Protocol):
    """The contract the drive loop depends on — not a provider, not a socket."""

    #: turns that have LANDED (completed), not turns started
    n: int
    #: one entry per landed turn: the assistant text of that turn
    transcript: list[str]
    #: appended by whatever notices a runaway; non-empty aborts the run
    trips: list[str]

    @property
    def text(self) -> str:
        """Every turn joined — what the probes' scoring regexes read."""
        ...

    def sees_prompt(self) -> bool:
        """True when this source can show what the model was GIVEN.

        `SHOW_SYSTEM` and the per-turn `USER →` boxes are only meaningful when
        this is true. A wire source returns False so the difference is stated
        rather than silently rendered as nothing.
        """
        ...
