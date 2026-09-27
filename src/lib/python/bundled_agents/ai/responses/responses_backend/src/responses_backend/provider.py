"""ResponsesProvider — streaming chat over the OpenAI Responses API, RAW TEXT.

Matches the AnthropicProvider contract: `chat(messages)` takes plain chat messages
(role system/user/assistant, text content) and yields `str` content tokens ONLY.
Tool-calling is owned by ai_core, which teaches the `<call>` grammar in the prompt
and parses it out of this text stream. This provider NEVER declares `tools` and
NEVER translates a native `function_call` item: the model↔harness path carries no
JSON in either direction. A native item that arrives anyway is a loud error, not a
silent drop — the model wrote a call in a grammar this kernel does not speak.

Internally translates to the Responses wire shape: the system message becomes
`instructions`, every other turn a typed `message` item (`input_text` for the user,
`output_text` for the assistant). Stateless: `store:false`, the whole conversation
in `input` every pass — the conversation is ai_core's, kept in its memory store,
never the endpoint's. The key is read from `OPENAI_API_KEY` at call time.
"""

from __future__ import annotations

import os
from typing import AsyncIterator

from ai_core.core import THINK_CLOSE, THINK_OPEN

#: The ONE env var the key is read from. No second spelling.
KEY_VAR = "OPENAI_API_KEY"
# NO guessy defaults (by default, no defaults): this connector names neither a host
# nor a model. The sentinels exist ONLY so `reflect` can say what is true; `chat()`
# fails loud naming the missing meta.
DEFAULT_ENDPOINT = "(no default — set the `endpoint` meta: the full …/v1/responses URL)"
DEFAULT_MODEL = "(no default — set the `model` meta)"

# EDGE TRANSPORT FAILURES — the same rule as the anthropic provider. A 4xx, or any
# 5xx carrying a JSON body, is the endpoint DELIBERATELY answering and stays loud.
# These codes with a NON-JSON body are a CDN/proxy error page: the request never
# reached anything that could judge it, so the one path is retried, bounded.
TRANSPORT_CODES = frozenset({502, 503, 504, 520, 521, 522, 523, 524})
RETRIES = 2
BACKOFF = 1.5
# Per-turn OUTPUT budget (`max_output_tokens`) — thinking is spent from it too. A
# FIELD on the record, not a constant, for the reason the anthropic provider gives.
MAX_TOKENS = 8192

#: Reasoning deltas. `reasoning_text` is the model's raw chain of thought (an
#: open-weights model behind a proxy); `reasoning_summary_text` is what OpenAI's
#: own models stream instead. Both are the thought channel — wrapped, never output.
_REASONING_DELTAS = frozenset(
    {"response.reasoning_text.delta", "response.reasoning_summary_text.delta"}
)
#: Output deltas. A refusal is text the model produced; dropping it would return a
#: refused turn as an EMPTY completion.
_TEXT_DELTAS = frozenset({"response.output_text.delta", "response.refusal.delta"})


def _to_responses(messages: list[dict]) -> tuple[str | None, list[dict]]:
    """Plain messages → (instructions, Responses input items).

    - role:system → `instructions`.
    - role:user → `{type:message, role:user, content:[{type:input_text, text}]}`.
    - role:assistant → `{type:message, role:assistant, content:[{type:output_text, text}]}`.
    Tool calls/replies are already inline text (`<call>` elements and `[reply to …]`
    blocks), so there is no function_call/function_call_output translation.
    """
    instructions: str | None = None
    items: list[dict] = []
    for m in messages:
        role = m.get("role")
        content = m.get("content", "") or ""
        if role == "system":
            instructions = content
        elif role in ("user", "assistant"):
            part = "input_text" if role == "user" else "output_text"
            items.append(
                {
                    "type": "message",
                    "role": role,
                    "content": [{"type": part, "text": content or "(no content)"}],
                }
            )
    return instructions, items


class ResponsesProvider:
    #: The prompt size the endpoint REPORTED for the last pass (None: not reported).
    #: ai_core calibrates its own token estimate with it.
    last_prompt_tokens: int | None = None
    #: `completed`, `max_tokens` (an `incomplete` cut at `max_output_tokens`), or the
    #: endpoint's own incomplete reason. ai_core reads `max_tokens` (a fold refuses a
    #: cut summary).
    last_stop_reason: str | None = None

    def __init__(
        self,
        endpoint: str = DEFAULT_ENDPOINT,
        model: str = DEFAULT_MODEL,
        *,
        max_tokens: int = MAX_TOKENS,
        sampling: dict | None = None,
        transport=None,
    ):
        self._endpoint = endpoint
        self._model = model
        self._max_tokens = max_tokens
        # Extra body params merged verbatim into the request (temperature, top_p,
        # reasoning, …). A record edit, not an ai_core concern. None values omitted.
        self._sampling = {k: v for k, v in (sampling or {}).items() if v is not None}
        # Test seam: an httpx transport to hand the client. None = the real network.
        self._transport = transport

    async def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        """Stream raw text tokens; ai_core parses any `<call>` element out of them.

        BOTH channels are read: reasoning deltas are wrapped in `<think>…</think>`
        and streamed alongside the output text, exactly as the other providers do.
        ai_core parses the RAW text (so a mid-thought call still dispatches) and
        persists it stripped."""
        import asyncio
        import json

        import httpx

        key = os.environ.get(KEY_VAR)
        if not key:
            raise RuntimeError(f"{KEY_VAR} not set in environment (.env)")
        if not self._endpoint or self._endpoint == DEFAULT_ENDPOINT:
            raise RuntimeError(
                "responses_backend: no `endpoint` set — this connector does not "
                "guess a host. Name the full …/v1/responses URL on the agent "
                "record (endpoint=...)."
            )
        if not self._model or self._model == DEFAULT_MODEL:
            raise RuntimeError(
                "responses_backend: no `model` set — this connector does not guess "
                "one. Name a model on the agent record (model=...)."
            )

        instructions, items = _to_responses(messages)
        body: dict = {
            "model": self._model,
            "input": items,
            # A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
            "max_output_tokens": max_tokens or self._max_tokens,
            "stream": True,
            # STATELESS: the conversation is ai_core's, never the endpoint's.
            "store": False,
        }
        if instructions:
            body["instructions"] = instructions
        body.update(self._sampling)
        headers = {
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
        }

        # WIRE TRACE — `FANTASTIC_AI_WIRE=1`, same as the anthropic provider: the
        # outgoing budget and each completion's terminal status with its item types.
        wire = os.environ.get("FANTASTIC_AI_WIRE") == "1"
        if wire:
            print(
                f"[wire] -> max_output_tokens={body['max_output_tokens']} "
                f"items={len(items)}",
                flush=True,
            )
        # NO READ CLOCK ON THE STREAM — the turn's one bound is ai_core's send
        # timeout; a queued turn is not a dead one. Connect stays finite.
        timeout = httpx.Timeout(None, connect=15.0)
        async with httpx.AsyncClient(
            timeout=timeout, transport=self._transport
        ) as client:
            attempt = 0
            while True:
                async with client.stream(
                    "POST", self._endpoint, json=body, headers=headers
                ) as resp:
                    if resp.status_code >= 400:
                        detail = (await resp.aread()).decode("utf-8", "replace")
                        ctype = resp.headers.get("content-type", "")
                        if not (
                            resp.status_code in TRANSPORT_CODES
                            and "json" not in ctype.lower()
                            and attempt < RETRIES
                        ):
                            raise RuntimeError(
                                f"responses {resp.status_code}: {detail}"
                            )
                        attempt += 1
                        print(
                            f"[responses] {resp.status_code} "
                            f"({ctype or 'no content-type'}) — transport error, "
                            f"retry {attempt}/{RETRIES}",
                            flush=True,
                        )
                    else:
                        # The terminal response object (`completed` / `incomplete`)
                        # — None until one arrives. A stream that ends without one
                        # did not finish.
                        final: dict | None = None
                        thinking = False
                        # What the stream actually CONTAINED — the output item types.
                        # The one fact that tells "the model wrote a native call"
                        # from "the connection died before any content".
                        kinds: list[str] = []
                        async for line in resp.aiter_lines():
                            if not line.startswith("data:"):
                                continue
                            data = line[len("data:") :].strip()
                            if not data:
                                continue
                            try:
                                evt = json.loads(data)
                            except json.JSONDecodeError:
                                continue
                            etype = evt.get("type")
                            if etype in _REASONING_DELTAS:
                                # A REASONING MODEL SPLITS ITS OUTPUT. Reading the output
                                # deltas alone returns a turn spent thinking as an EMPTY
                                # completion, and a `<call>` written mid-thought is lost.
                                # WRAPPED once (see `ai_core.core.without_thinking`):
                                # parsed for calls, stripped before the turn is persisted.
                                think = evt.get("delta") or ""
                                if think:
                                    if not thinking:
                                        thinking = True
                                        yield THINK_OPEN
                                    yield think
                            elif etype in _TEXT_DELTAS:
                                txt = evt.get("delta") or ""
                                if txt:
                                    if thinking:
                                        thinking = False
                                        yield THINK_CLOSE
                                    yield txt
                            elif etype == "response.output_item.added":
                                item = evt.get("item") or {}
                                kinds.append(item.get("type"))
                                if item.get("type") == "function_call":
                                    # A NATIVE tool call — JSON arguments this kernel
                                    # does not read. A silent drop would end the turn
                                    # looking finished with the call lost.
                                    raise RuntimeError(
                                        "responses stream carried a native function_call "
                                        f"item ({item.get('name')!r}): the model wrote a "
                                        "JSON tool call, which this kernel does not parse "
                                        "— calls are `<call name=…>` text"
                                    )
                            elif etype in ("response.completed", "response.incomplete"):
                                final = evt.get("response") or {}
                                # PROVIDER-REPORTED USAGE — what the window really held.
                                u = final.get("usage") or {}
                                self.last_prompt_tokens = (
                                    int(u.get("input_tokens") or 0) or None
                                )
                            elif etype == "response.failed":
                                err = (evt.get("response") or {}).get("error")
                                raise RuntimeError(f"responses stream failed: {err}")
                            elif etype == "error":
                                raise RuntimeError(f"responses stream error: {evt}")
                            # created / in_progress / *.done / content_part.*: ignored
                        # A turn spent ENTIRELY reasoning ends inside the think block —
                        # close it so the marker pair is always balanced for the parser.
                        if thinking:
                            thinking = False
                            yield THINK_CLOSE
                        status = (final or {}).get("status")
                        reason = ((final or {}).get("incomplete_details") or {}).get(
                            "reason"
                        )
                        if wire:
                            print(
                                f"[wire] <- status={status!r} reason={reason!r} "
                                f"items={kinds!r}",
                                flush=True,
                            )
                        self.last_stop_reason = (
                            "max_tokens"
                            if reason == "max_output_tokens"
                            else (reason or status)
                        )
                        # A BOUNDED SIDE CALL that stopped at its bound is an ANSWER,
                        # not a fault: the caller asked for at most `max_tokens` and
                        # reads `last_stop_reason`.
                        if (
                            max_tokens is not None
                            and self.last_stop_reason == "max_tokens"
                        ):
                            return
                        # A turn that did not COMPLETE did not finish — say so, loudly.
                        # A cut returned as if finished parses as a truncated envelope
                        # and the chain dies downstream with the cause hidden.
                        if status != "completed":
                            raise RuntimeError(
                                "responses stream ended without completing: "
                                f"status={status!r} reason={reason!r} items={kinds!r}"
                                + (
                                    " (no terminal event — connection cut mid-stream)"
                                    if final is None
                                    else ""
                                )
                            )
                        return
                await asyncio.sleep(BACKOFF * attempt)

    @property
    def model(self) -> str:
        return self._model

    def stop(self) -> None:
        pass
