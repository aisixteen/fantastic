"""THE METER — what each test cost the model, per request.

A forwarder between the kernels of one run and the model endpoint (vLLM, or
Anthropic's Messages API with `BACKEND=anthropic`). `run.py` starts it when
`E2E_METER=1` and points each test's endpoint at `<meter>/m/<test name>`, so every request lands in `meter.jsonl` under the test
that made it — sub-AIs included, since they inherit the endpoint.

Recorded per request: the endpoint's own `usage` (prompt / completion / cached
tokens — the last chunk of a stream carries it, `stream_options.include_usage`),
`finish_reason`, time to first byte and total time, and the request's message
count and content chars. Headers are forwarded, never logged.

THE TURN TEXT. Each request also lands as `turns/<tag>/<NNN>.txt` beside
`meter.jsonl`: the message the model was answering, its THINKING (the stream's
`reasoning_content` / `reasoning`, which no log or history row shows) and its
reply — written when the stream ends, so a turn cut at a timeout is kept too.

    uv run python e2e/helpers/meter.py --port P --upstream URL --out meter.jsonl
"""

from __future__ import annotations

import argparse
import itertools
import json
import time
from collections import defaultdict
from pathlib import Path

import httpx
import uvicorn
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import Response, StreamingResponse
from starlette.routing import Route

_HOP = {"host", "content-length", "connection", "transfer-encoding", "accept-encoding"}


def _content_chars(body: bytes) -> tuple[int | None, int | None]:
    try:
        msgs = json.loads(body).get("messages") or []
    except (ValueError, AttributeError):
        return None, None
    chars = 0
    for m in msgs:
        c = m.get("content")
        if isinstance(c, str):
            chars += len(c)
        elif isinstance(c, list):
            chars += sum(len(p.get("text") or "") for p in c if isinstance(p, dict))
    return len(msgs), chars


def _last_message(body: bytes) -> str:
    try:
        msgs = json.loads(body).get("messages") or []
    except (ValueError, AttributeError):
        return ""
    c = msgs[-1].get("content") if msgs else ""
    if isinstance(c, list):
        c = "".join(p.get("text") or "" for p in c if isinstance(p, dict))
    return c if isinstance(c, str) else ""


def _text(obj: dict, think: list[str], said: list[str]) -> None:
    if obj.get("type") == "content_block_delta":  # Anthropic Messages stream
        d = obj.get("delta") or {}
        if r := d.get("thinking"):
            think.append(r)
        if c := d.get("text"):
            said.append(c)
        return
    for ch in obj.get("choices") or []:
        d = ch.get("delta") or ch.get("message") or {}
        if r := d.get("reasoning_content") or d.get("reasoning"):
            think.append(r)
        if c := d.get("content"):
            said.append(c)


def _note(rec: dict, obj: dict) -> None:
    # Anthropic Messages stream: the prompt's usage is on `message_start`, the
    # completion's on `message_delta`. The prompt counts every input token:
    # fresh, written to the cache and read from it.
    kind = obj.get("type")
    if kind == "message_start":
        u = (obj.get("message") or {}).get("usage") or {}
        rec["prompt_tokens"] = sum(
            int(u.get(k) or 0)
            for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")
        )
        rec["cached_tokens"] = int(u.get("cache_read_input_tokens") or 0)
        rec["cache_write_tokens"] = int(u.get("cache_creation_input_tokens") or 0)
        return
    if kind == "message_delta":
        if (u := obj.get("usage")) and u.get("output_tokens") is not None:
            rec["completion_tokens"] = u["output_tokens"]
        if fr := (obj.get("delta") or {}).get("stop_reason"):
            rec["finish_reason"] = fr
        return
    if u := obj.get("usage"):
        rec["prompt_tokens"] = u.get("prompt_tokens")
        rec["completion_tokens"] = u.get("completion_tokens")
        if d := u.get("prompt_tokens_details"):
            rec["cached_tokens"] = d.get("cached_tokens")
    for ch in obj.get("choices") or []:
        if fr := ch.get("finish_reason"):
            rec["finish_reason"] = fr


def app(upstream: str, out: str) -> Starlette:
    client = httpx.AsyncClient(timeout=None)
    # Open for the server's whole life, line-buffered: every request lands as it ends.
    sink = open(out, "a", buffering=1)  # noqa: SIM115
    turns = Path(out).parent / "turns"
    seq: defaultdict[str, itertools.count] = defaultdict(lambda: itertools.count(1))

    async def forward(request: Request):
        tag = request.path_params["tag"]
        rest = request.path_params["rest"]
        body = await request.body()
        n_msgs, chars = _content_chars(body)
        rec = {
            "tag": tag,
            "path": rest,
            "t0": time.time(),
            "messages": n_msgs,
            "content_chars": chars,
        }
        headers = {k: v for k, v in request.headers.items() if k.lower() not in _HOP}
        req = client.build_request(
            request.method, f"{upstream}/{rest}", headers=headers, content=body
        )
        up = await client.send(req, stream=True)
        rec["status"] = up.status_code

        think: list[str] = []
        said: list[str] = []

        async def relay():
            buf = b""
            first = True
            try:
                async for chunk in up.aiter_bytes():
                    if first:
                        rec["ttfb"] = round(time.time() - rec["t0"], 3)
                        first = False
                    buf += chunk
                    *lines, buf = buf.split(b"\n")
                    for line in lines:
                        line = line.strip()
                        if line.startswith(b"data:") and line[5:].strip() not in (
                            b"",
                            b"[DONE]",
                        ):
                            try:
                                obj = json.loads(line[5:])
                            except ValueError:
                                continue
                            _note(rec, obj)
                            _text(obj, think, said)
                    yield chunk
                tail = buf.strip()
                if tail.startswith(b"{"):
                    try:
                        obj = json.loads(tail)
                    except ValueError:
                        obj = None
                    if obj is not None:
                        _note(rec, obj)
                        _text(obj, think, said)
            finally:
                await up.aclose()
                rec["dur"] = round(time.time() - rec["t0"], 3)
                sink.write(json.dumps(rec) + "\n")
                d = turns / tag
                d.mkdir(parents=True, exist_ok=True)
                head = {
                    k: rec.get(k)
                    for k in (
                        "t0",
                        "messages",
                        "prompt_tokens",
                        "completion_tokens",
                        "dur",
                        "finish_reason",
                    )
                }
                (d / f"{next(seq[tag]):03d}.txt").write_text(
                    f"{json.dumps(head)}\n\n=== ANSWERING ===\n{_last_message(body)}"
                    f"\n\n=== THINKING ===\n{''.join(think)}"
                    f"\n\n=== REPLY ===\n{''.join(said)}\n"
                )

        keep = {
            k: v
            for k, v in up.headers.items()
            if k.lower() not in _HOP | {"content-encoding"}
        }
        return StreamingResponse(relay(), status_code=up.status_code, headers=keep)

    async def health(_request: Request):
        return Response("ok")

    return Starlette(
        routes=[
            Route("/health", health),
            Route("/m/{tag}/{rest:path}", forward, methods=["GET", "POST"]),
        ]
    )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--upstream", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    uvicorn.run(
        app(a.upstream.rstrip("/"), a.out),
        host="127.0.0.1",
        port=a.port,
        log_level="warning",
        timeout_keep_alive=300,
    )


if __name__ == "__main__":
    main()
