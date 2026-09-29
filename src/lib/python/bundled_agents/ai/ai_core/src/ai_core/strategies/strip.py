"""`strip` — the context is the ledger at MIXED RESOLUTION. Bodies go, titles stay.

Every call the model makes carries a one-sentence `title` it wrote at the moment it made
the call (see `_TITLES_HOWTO`). The reply comes back tagged with that title. So when the
window fills, the cheapest safe move is not to drop TURNS but to drop reply BODIES: the
6.8KB read becomes one line — its tag, its title, its size, its pid — and stays
addressable by `recall`. Every assistant turn survives untouched, which matters more than
it looks: the model's own prose after a reply IS its distillation of that reply, written
with the task in mind, and it is tiny.

    L0  full row     [reply to pid · notes read · "survey chunk 2"]\\n<6800 bytes>
    L1  stub         [reply to pid · notes read · "survey chunk 2"] (stripped: 6800 bytes)

TWO RULES THAT KEEP THE KV CACHE WARM. Measured on this stack (vLLM /metrics): an append
after the cached prefix reuses it in full; any edit inside it reuses NOTHING. So —
  * HYSTERESIS. Strip only past the HIGH watermark, and strip down to LOW in one batch:
    ten stubs in one event is one cache miss, not ten.
  * MONOTONE, OLDEST FIRST. Once stripped, always stripped, and the stripped region is a
    contiguous run from the front — the stable prefix only ever EXTENDS, never gets holes.
No LLM call. If every body is stripped and the prose alone still overflows, the second
stage is `halve`'s cut — not a fallback, the next stage of the same strategy, and it
says so in the artifact.
"""

from __future__ import annotations

import re

from ai_core.context import ProjectionCtx, estimate_one, estimate_tokens
from ai_core.strategies.base import (
    NOTICE_ENVELOPE_RESERVE,
    Projection,
    fit_tail,
)

LOW = 0.6  # strip down to this fraction of the budget once over it


def _halve(body: list[dict]) -> list[dict]:
    """The back half — strip's second stage, what happens when every body is already
    a stub and the prose alone still overflows."""
    return body[len(body) // 2 :]


# pid off a reply block's first line: `[reply to <pid> · …`
_PID = re.compile(r"^\[reply to (\S+) · ")

# Per-agent set of stripped pids. In-memory, process-lifetime — the same standing as
# `_projection`/`_compaction_mark`: a strip is a VIEW decision, the durable store is
# untouched, so losing this on restart costs one re-strip and nothing else.
_stripped: dict[str, set[int | str]] = {}


def _pid_of(pm) -> int | str:
    """The pid on a tag as the STORE keys it: an int when it is one (the kernel's
    counter), the raw token otherwise (a `-`, a foreign tag)."""
    tok = pm.group(1)
    return int(tok) if tok.isdigit() else tok


def _blocks(content: str) -> list[str]:
    # Lazy: `core` imports this package at module load, so the reverse import has to
    # wait until call time. Same grammar, one owner (`core._reply_blocks`).
    from ai_core.core import _reply_blocks

    return _reply_blocks(content)


def _is_reply(m: dict) -> bool:
    c = m.get("content")
    return m.get("role") == "user" and isinstance(c, str) and c.startswith("[reply to ")


def _stub(block: str) -> str:
    head, _, body = block.partition("\n")
    return f"{head} (stripped: {len(body)} bytes — recall by pid)"


def _render(body: list[dict], stripped: set[str]) -> tuple[list[dict], int, int]:
    """Project `body` with every stripped block reduced to its stub. Deterministic in
    (body, stripped) — the same inputs render the same bytes, which is what keeps the
    prefix stable between strip events."""
    out, n, saved = [], 0, 0
    for m in body:
        if not _is_reply(m):
            out.append(m)
            continue
        parts = []
        for b in _blocks(m["content"]):
            pm = _PID.match(b)
            if pm and _pid_of(pm) in stripped:
                parts.append(_stub(b))
                n += 1
                saved += len(b) - len(parts[-1])
            else:
                parts.append(b)
        out.append({**m, "content": "\n\n".join(parts)})
    return out, n, saved


async def strip(
    body: list[dict], system_block: list[dict], rec: dict, ctx: ProjectionCtx
) -> Projection:
    if len(body) <= 1:
        return Projection(body=fit_tail(body, ctx.budget))
    if (
        ctx.self_id not in _stripped
        and ctx.kernel is not None
        and hasattr(ctx.kernel, "send")
    ):
        # SEEDED FROM THE VIEW ARRAY on first use — stubs survive a restart, and the
        # prefix bytes are the same before and after it.
        from ai_core.core import _load_view

        seed: set[int | str] = set()
        view = await _load_view(ctx.self_id, ctx.kernel)
        if view:
            stubbed = {e.get("id") for e in view["ids"] if e.get("res") == "stub"}
            for m in body:
                if m.get("id") in stubbed and _is_reply(m):
                    for b in _blocks(m["content"]):
                        pm = _PID.match(b)
                        if pm:
                            seed.add(_pid_of(pm))
        _stripped[ctx.self_id] = seed
    done = _stripped.setdefault(ctx.self_id, set())
    budget = ctx.budget - NOTICE_ENVELOPE_RESERVE
    rendered, n, saved = _render(body, done)
    if estimate_tokens(rendered) > budget:
        # OVER THE HIGH WATERMARK: strip oldest-first, in ONE batch, down to LOW.
        target = int(budget * LOW)
        for m in body[1:-1]:  # never the goal, never the live turn
            if estimate_tokens(rendered) <= target:
                break
            if not _is_reply(m):
                continue
            for b in _blocks(m["content"]):
                pm = _PID.match(b)
                if pm:
                    done.add(_pid_of(pm))
            rendered, n, saved = _render(body, done)
    cut = False
    if estimate_tokens(rendered) > budget:
        # Everything strippable is stripped and the prose alone still overflows. Stage
        # two: the cut. The goal turn is pinned for the reason `halve` documents.
        first = rendered[0]
        tail = fit_tail(_halve(rendered[1:]), max(0, budget - estimate_one(first)))
        rendered, cut = [first] + tail, True
    return Projection(
        body=rendered, omitted_marker=cut, stripped=n, stripped_bytes=saved
    )
