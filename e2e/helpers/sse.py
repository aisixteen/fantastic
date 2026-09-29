"""Raw HTTP to an inference endpoint — for the `backends` group's WIRE probes.

No kernel, no provider: what the endpoint itself sends, event by event, so a wire
fault is told apart from a provider fault. Both calls return everything measured —
status, content type, the parsed body or the ordered SSE events — and never raise on
an HTTP error: a refusal is a measurement too.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

import httpx

#: One request's ceiling. A reasoning model thinks before its first byte, and the
#: shared GPU may queue the request behind other sessions.
TIMEOUT = httpx.Timeout(300.0, connect=15.0)


@dataclass
class Reply:
    status: int
    ctype: str
    body: Any = None
    #: SSE events in arrival order, each `{"event": <name|None>, **data}`.
    events: list[dict] = field(default_factory=list)
    #: True when the server ended the stream (the body iterator finished).
    closed: bool = False
    text: str = ""


async def post_json(url: str, headers: dict, body: dict) -> Reply:
    async with httpx.AsyncClient(timeout=TIMEOUT) as c:
        r = await c.post(url, headers=headers, json=body)
    ctype = r.headers.get("content-type", "")
    try:
        parsed = r.json()
    except ValueError:
        parsed = None
    return Reply(r.status_code, ctype, body=parsed, text=r.text[:2000])


async def post_sse(url: str, headers: dict, body: dict) -> Reply:
    events: list[dict] = []
    name: str | None = None
    async with httpx.AsyncClient(timeout=TIMEOUT) as c:
        async with c.stream("POST", url, headers=headers, json=body) as r:
            ctype = r.headers.get("content-type", "")
            if r.status_code >= 400:
                raw = (await r.aread()).decode("utf-8", "replace")
                try:
                    parsed = json.loads(raw)
                except ValueError:
                    parsed = None
                return Reply(r.status_code, ctype, body=parsed, text=raw[:2000])
            async for line in r.aiter_lines():
                if line.startswith("event:"):
                    name = line[len("event:") :].strip()
                elif line.startswith("data:"):
                    data = line[len("data:") :].strip()
                    if not data or data == "[DONE]":
                        continue
                    try:
                        obj = json.loads(data)
                    except ValueError:
                        obj = {"_unparsed": data}
                    events.append({"event": name, **(obj if isinstance(obj, dict) else {"_data": obj})})
                    name = None
            return Reply(r.status_code, ctype, events=events, closed=True)


def check(checks: list, name: str, ok: bool, detail: Any = "") -> bool:
    """Record one named check; print it with what was measured."""
    checks.append((name, bool(ok)))
    print(f"  [{'ok' if ok else 'XX'}] {name}{'  — ' + str(detail) if detail != '' else ''}",
          flush=True)
    return bool(ok)
