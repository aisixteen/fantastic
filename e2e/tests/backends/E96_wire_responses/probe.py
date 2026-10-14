"""backends/wire_responses — the OpenAI Responses wire, measured raw.

No kernel and no provider: the endpoint our `responses_backend` talks to, asked
directly with the record the plug builds. What the provider relies on, checked at
the source — and the proxy's contract: raw text
out, reasoning as `reasoning_text`, a cut ends `response.incomplete`, stateless,
native tools refused.

  1. non-stream: `object:response`, `status:completed`, a `message` item whose
     `output_text` carries the answer;
  2. stream: `response.created` first, `response.completed` last, strictly rising
     `sequence_number`, output deltas that add up to `output_text.done`, any
     reasoning before the first output delta, closed by the server;
  3. a `max_output_tokens` cut: `response.incomplete` last, status `incomplete`,
     reason `max_output_tokens` — streamed and not;
  4. no `function_call` item for a plain prompt;
  5. refused with a 400: native `tools`, `store:true`, `previous_response_id`.
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
        "authorization": f"Bearer {os.environ['OPENAI_API_KEY']}",
        "content-type": "application/json",
    }


def _body(max_output_tokens: int, *, stream: bool, **over) -> dict:
    return {
        "model": BK.model,
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": ASK}]}],
        "max_output_tokens": max_output_tokens,
        "stream": stream,
        "store": False,
        **(BK.record().get("sampling") or {}),
        **over,
    }


def _message_text(resp: dict) -> str:
    return "".join(p.get("text") or "" for it in (resp.get("output") or [])
                   if it.get("type") == "message"
                   for p in (it.get("content") or []) if p.get("type") == "output_text")


async def main():
    if blockers := BK.blockers():
        verdict(False, "; ".join(blockers))
        return
    url = BK.record()["endpoint"]
    budget = int(BK.record().get("max_tokens") or 8192)
    checks: list = []

    # 1: non-stream
    r = await post_json(url, _headers(), _body(budget, stream=False))
    b = r.body or {}
    check(checks, "non-stream: 200 JSON", r.status == 200 and isinstance(r.body, dict),
          f"{r.status} {r.text[:200]}")
    check(checks, "non-stream: object response, status completed",
          b.get("object") == "response" and b.get("status") == "completed",
          f"object={b.get('object')!r} status={b.get('status')!r}")
    kinds = [it.get("type") for it in (b.get("output") or [])]
    check(checks, "non-stream: message item carries output_text", bool(_message_text(b).strip()),
          f"items={kinds} text={_message_text(b)[:60]!r}")
    check(checks, "non-stream: no function_call item", "function_call" not in kinds, kinds)
    print(f"     usage={b.get('usage')}  (reported only — ai_core calibrates when present)")

    # 2: stream
    r = await post_sse(url, _headers(), _body(budget, stream=True))
    check(checks, "stream: 200 text/event-stream", r.status == 200 and "event-stream" in r.ctype,
          f"{r.status} {r.ctype} {r.text[:200]}")
    ev = r.events
    types = [e.get("type") for e in ev]
    check(checks, "stream: response.created first, response.completed last",
          bool(types) and types[0] == "response.created" and types[-1] == "response.completed",
          f"first={types[:1]} last={types[-1:]} n={len(types)}")
    seq = [e.get("sequence_number") for e in ev]
    check(checks, "stream: sequence_number strictly rising",
          all(isinstance(s, int) for s in seq) and all(a < b for a, b in zip(seq, seq[1:])),
          f"{seq[:3]}…{seq[-2:]}")
    deltas = "".join(e.get("delta") or "" for e in ev if e.get("type") == "response.output_text.delta")
    done = "".join(e.get("text") or "" for e in ev if e.get("type") == "response.output_text.done")
    final = (ev[-1].get("response") or {}) if ev else {}
    check(checks, "stream: output deltas == output_text.done == final message",
          bool(deltas.strip()) and deltas == done == _message_text(final),
          f"{deltas[:40]!r} / {done[:40]!r} / {_message_text(final)[:40]!r}")
    reasoning = [i for i, t in enumerate(types)
                 if t in ("response.reasoning_text.delta", "response.reasoning_summary_text.delta")]
    first_out = next((i for i, t in enumerate(types) if t == "response.output_text.delta"), None)
    check(checks, "stream: reasoning (if any) before the first output delta",
          not reasoning or first_out is None or max(reasoning) < first_out,
          f"reasoning_deltas={len(reasoning)}")
    check(checks, "stream: no function_call item",
          not any((e.get("item") or {}).get("type") == "function_call" for e in ev))
    check(checks, "stream: closed by the server", r.closed)

    # 3: the cut — streamed, then not
    r = await post_sse(url, _headers(), _body(16, stream=True))
    last = r.events[-1] if r.events else {}
    lr = last.get("response") or {}
    check(checks, "cut stream: response.incomplete last, reason max_output_tokens",
          last.get("type") == "response.incomplete" and lr.get("status") == "incomplete"
          and (lr.get("incomplete_details") or {}).get("reason") == "max_output_tokens",
          f"last={last.get('type')!r} status={lr.get('status')!r} details={lr.get('incomplete_details')}")
    check(checks, "cut stream: closed by the server", r.closed)
    r = await post_json(url, _headers(), _body(16, stream=False))
    b = r.body or {}
    check(checks, "cut non-stream: status incomplete, reason max_output_tokens",
          b.get("status") == "incomplete"
          and (b.get("incomplete_details") or {}).get("reason") == "max_output_tokens",
          f"{r.status} status={b.get('status')!r} details={b.get('incomplete_details')}")

    # 5: refusals — raw means no native tools; stateless means no server-side state
    tool = {"type": "function", "name": "ping", "parameters": {"type": "object", "properties": {}}}
    for name, over in (
        ("native tools", {"tools": [tool]}),
        ("store:true", {"store": True}),
        ("previous_response_id", {"previous_response_id": "resp_e2e_probe"}),
    ):
        r = await post_json(url, _headers(), _body(64, stream=False, **over))
        check(checks, f"refused with 400: {name}", r.status == 400, f"{r.status} {r.text[:120]}")

    ok = all(v for _, v in checks)
    verdict(ok, f"{sum(v for _, v in checks)}/{len(checks)} wire checks" + ("" if ok else " — see XX lines"))


if __name__ == "__main__":
    asyncio.run(main())
