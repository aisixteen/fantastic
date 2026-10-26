"""memory/fork_cache — a fork's first send reads the parent's prompt cache.

Python runtime, the Anthropic plug (skipped without it). The parent runs with
`caching: provider_cache` and a primer: its first message is long and its row id is
`cache_primer_until`, so every request marks the end of that prefix for the provider.
Two turns write the cache; the parent marks its history; a fork at the mark sends once.
Scored on the fork's cache-read tokens, from the backend's per-agent usage count
(`anthropic_backend.tools.usage_of`), being above zero.
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, creds, memory, verdict
from helpers.ai import _live_tasks
from helpers.kernel import open_kernel

#: A prefix well above every model's minimum cacheable size, the same every run.
FILLER = "\n".join(
    f"Ledger line {i:04d}: the parser reads tokens, the lexer splits lines, the printer writes bytes."
    for i in range(420)
)
PRIMER = (
    "Here is a ledger. Read it; you will be asked one short thing about it later.\n\n"
    + FILLER
    + "\n\nAcknowledge by calling done."
)
ASK = "Reply by calling done and nothing else."


def _skip(why: str) -> None:
    print(f"E2E-SKIP: {why}", flush=True)


async def _turn(root, aid, text):
    r = (await root.send([{"target": aid, "timeout": 300, "payload": {"type": "send", "text": text}}]))[0]["result"]
    assert not (isinstance(r, dict) and r.get("error")), r
    for _ in range(300):
        await asyncio.sleep(0.1)
        if not await _live_tasks(root, aid):
            await asyncio.sleep(0.3)
            if not await _live_tasks(root, aid):
                return
    raise AssertionError(f"the turn of {aid} never went quiet")


async def main():
    try:
        BK = _resolve_backend()
    except SystemExit as e:  # no plug named, or no model: nothing to measure
        return _skip(str(e))
    if BK.name != "anthropic":
        return _skip(f"needs BACKEND=anthropic, got {BK.name}")
    if blockers := BK.blockers():
        return _skip("; ".join(blockers))
    from anthropic_backend.tools import usage_of

    root, core, close = await open_kernel(prefix="mem_fork_cache_", reach=["ai", "cli"])
    try:
        conn = await connect(
            root, core, BK,
            can_call={"cli": "*"},
            record=BK.record(caching="provider_cache"),
            limits=Limits.from_env(os.environ),
        )
        aid = conn.id
        await _turn(root, aid, PRIMER)
        first = (await memory.rows_of(root, aid, conversation_only=True))[0]["id"]
        # the primer ends at the first row: the provider is told where the prefix ends
        done = (await root.send([{"target": root.id, "payload": {
            "type": "update_agent", "id": aid, "cache_primer_until": first}}]))[0]["result"]
        assert not (done or {}).get("error"), done
        await _turn(root, aid, ASK)  # writes the cache
        marked = (await root.send([{"target": aid, "payload": {"type": "mark", "label": "ledger read"}}]))[0]["result"]
        assert isinstance(marked.get("at"), int), marked
        forked = (await root.send([{"target": aid, "payload": {"type": "fork", "at": marked["at"], "id": "ai_fork"}}]))[0]["result"]
        assert forked.get("id") == "ai_fork", forked
        # the fork reaches the key store and streams to `cli` as the parent does
        if BK.key_ref:
            await memory.open_ingress(root, creds.CREDS, "ai_fork")
        await memory.open_ingress(root, "cli", "ai_fork")

        before = usage_of("ai_fork").cache_read_tokens
        await _turn(root, "ai_fork", ASK)
        read = usage_of("ai_fork").cache_read_tokens - before
        parent_read = usage_of(aid).cache_read_tokens

        print(f"     fork={forked} cache_read(fork)={read} cache_read(parent so far)={parent_read}")
        checks = [
            ("the fork copied the parent's rows up to the mark", forked.get("copied", 0) >= 2),
            ("the fork's first send read the parent's cache: cache-read tokens above zero", read > 0),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "the fork hit the parent's prompt cache" if allok else "the fork did not hit the cache")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
