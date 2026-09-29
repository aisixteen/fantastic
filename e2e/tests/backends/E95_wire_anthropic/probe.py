"""backends/wire_anthropic — the Anthropic Messages wire, measured raw.

No kernel and no provider: the endpoint our `anthropic_backend` talks to, asked
directly with the same record the plug builds (endpoint, model, budget, sampling).
Everything the provider relies on is checked at the source, so a wire fault is
never mistaken for a kernel fault:

  1. a plain streamed turn is framed `message_start … message_stop`, its text
     arrives in `text_delta`, its stop reason is `end_turn`;
  2. any thinking arrives on its OWN channel (`thinking_delta`), before the text;
  3. no `tools` declared → no `tool_use` block;
  4. a `max_tokens` cut reports `stop_reason: max_tokens` and still closes;
  5. a malformed request is a deliberate 4xx with a JSON body, not an edge page.
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
from helpers import verdict
from helpers.sse import check, post_json, post_sse

BK = _resolve_backend()
ASK = "Reply with exactly one word: pong"


def _headers() -> dict:
    return {
        "x-api-key": os.environ["ANTHROPIC_API_KEY"],
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
    }


def _body(max_tokens: int, **over) -> dict:
    rec = BK.record()
    return {
        "model": BK.model,
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": ASK}],
        "stream": True,
        **(rec.get("sampling") or {}),
        **over,
    }


def _blocks(events: list[dict]) -> list[str]:
    return [(e.get("content_block") or {}).get("type") for e in events
            if e.get("type") == "content_block_start"]


def _stop(events: list[dict]):
    return next(((e.get("delta") or {}).get("stop_reason") for e in reversed(events)
                 if e.get("type") == "message_delta"), None)


async def main():
    if blockers := BK.blockers():
        verdict(False, "; ".join(blockers))
        return
    url = BK.record()["endpoint"]
    rec_budget = int(BK.record().get("max_tokens") or 8192)
    checks: list = []

    # 1-3: a plain streamed turn
    r = await post_sse(url, _headers(), _body(rec_budget))
    check(checks, "plain turn: 200 text/event-stream", r.status == 200 and "event-stream" in r.ctype,
          f"{r.status} {r.ctype} {r.text[:200]}")
    ev = r.events
    types = [e.get("type") for e in ev]
    check(checks, "framed message_start … message_stop",
          bool(types) and types[0] == "message_start" and types[-1] == "message_stop",
          f"first={types[:1]} last={types[-1:]} n={len(types)}")
    text = "".join((e.get("delta") or {}).get("text") or "" for e in ev
                   if (e.get("delta") or {}).get("type") == "text_delta")
    think = [i for i, e in enumerate(ev) if (e.get("delta") or {}).get("type") == "thinking_delta"]
    first_text = next((i for i, e in enumerate(ev)
                       if (e.get("delta") or {}).get("type") == "text_delta"), None)
    check(checks, "text channel carries the answer", bool(text.strip()), repr(text[:80]))
    check(checks, "thinking (if any) on its own channel, before the text",
          not think or first_text is None or max(think) < first_text,
          f"thinking_deltas={len(think)}")
    check(checks, "stop_reason end_turn", _stop(ev) == "end_turn", _stop(ev))
    check(checks, "no tool_use block without tools", "tool_use" not in _blocks(ev), _blocks(ev))
    check(checks, "stream closed by the server", r.closed)
    start = next((e for e in ev if e.get("type") == "message_start"), {})
    usage = (start.get("message") or {}).get("usage") or {}
    print(f"     usage@message_start={usage}  (reported only — ai_core calibrates when present)")

    # 4: a max_tokens cut
    r = await post_sse(url, _headers(), _body(16))
    types = [e.get("type") for e in r.events]
    check(checks, "cut: stop_reason max_tokens", _stop(r.events) == "max_tokens", _stop(r.events))
    check(checks, "cut: still framed and closed",
          r.closed and bool(types) and types[-1] == "message_stop", types[-1:])

    # 5: a deliberate refusal
    bad = {k: v for k, v in _body(64).items() if k != "messages"}
    bad["stream"] = False
    r = await post_json(url, _headers(), bad)
    check(checks, "malformed request: 4xx with a JSON body",
          400 <= r.status < 500 and r.body is not None, f"{r.status} {r.text[:160]}")

    ok = all(v for _, v in checks)
    verdict(ok, f"{sum(v for _, v in checks)}/{len(checks)} wire checks" + ("" if ok else " — see XX lines"))


if __name__ == "__main__":
    asyncio.run(main())
