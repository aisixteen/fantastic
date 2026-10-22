"""ResponsesProvider — streaming chat over the OpenAI Responses API, RAW TEXT.

Matches the `ai_core.provider.Provider` contract: `chat(messages)` takes plain chat messages
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
never the endpoint's. The key is read before each request through `key` — the
record's `credentials` ref in a credential store (`ai_core/credentials.py`).
"""

from __future__ import annotations

import os
from contextlib import aclosing
from typing import AsyncIterator

import httpx
from ai_core.core import THINK_CLOSE, THINK_OPEN
from ai_core.provider import ProviderError

# EDGE TRANSPORT FAILURES — not answers, and not statements about the request.
# A 4xx, or any 5xx carrying a JSON body, is the endpoint DELIBERATELY answering:
# that is the caller's bug and it stays loud. These codes with a NON-JSON body are
# a CDN/proxy error page, meaning the request never reached anything that could
# judge it (e.g. a `521` from a brief gap while a reverse proxy restarts). Retrying the same single path on a
# transport failure is not a second mechanism — nothing was tried and nothing is
# being routed around.
TRANSPORT_CODES = frozenset({502, 503, 504, 520, 521, 522, 523, 524})
RETRIES = 2
BACKOFF = 1.5
# Per-turn OUTPUT budget (`max_output_tokens`). A FIELD on the record, not a
# constant, because it is a per-model property: the thought of a reasoning model is
# spent from this same budget, so a value that leaves one model room to finish
# truncates another mid-thought — a whole turn of thinking, no content, no call.
# The endpoint's ceiling is `max_output_tokens + prompt <= the model's context`.
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


#: Request-body keys this provider sets itself; `sampling` may not touch them.
_OWNED_KEYS = frozenset(
    {"model", "input", "instructions", "max_output_tokens", "stream", "store"}
)


class ResponsesProvider:
    #: The prompt size the endpoint REPORTED for the last pass (None: not reported).
    #: ai_core calibrates its own token estimate with it.
    last_prompt_tokens: int | None = None
    #: The output tokens the server reported for the last pass (None = not reported).
    last_completion_tokens: int | None = None
    #: `completed`, `max_tokens` (an `incomplete` cut at `max_output_tokens`), or the
    #: endpoint's own incomplete reason. ai_core reads `max_tokens` (a fold refuses a
    #: cut summary).
    last_stop_reason: str | None = None

    def __init__(
        self,
        endpoint: str,
        model: str,
        *,
        key,
        max_tokens: int = MAX_TOKENS,
        sampling: dict | None = None,
        transport=None,
    ):
        # `async key(endpoint) -> str`, awaited before EACH request.
        self._key = key
        self._endpoint = endpoint
        self._model = model
        self._max_tokens = max_tokens
        # Extra body params merged verbatim into the request (temperature, top_p,
        # reasoning, …). A record edit, not an ai_core concern. None values omitted.
        # The keys this provider owns are refused, never silently overridden.
        owned = sorted(set(sampling or {}) & _OWNED_KEYS)
        if owned:
            raise ValueError(
                f"responses_backend: `sampling` may not set {', '.join(owned)} — "
                "those are the record's (`model`, `max_tokens`) or the provider's own"
            )
        self._sampling = {k: v for k, v in (sampling or {}).items() if v is not None}
        # Test seam: an httpx transport to hand the client. None = the real network.
        self._transport = transport

    async def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        """Stream raw text tokens (see `_chat`). THE LINK FAILED, not the endpoint's
        answer (#198): a connect or read error, a timeout, or a stream the peer
        broke is a `ProviderError` marked transport."""
        try:
            async with aclosing(self._chat(messages, max_tokens)) as stream:
                async for token in stream:
                    yield token
        except (
            httpx.TimeoutException,
            httpx.NetworkError,
            httpx.RemoteProtocolError,
        ) as e:
            raise ProviderError(f"http: {e}", transport=True) from e

    async def _chat(
        self, messages: list[dict], max_tokens: int | None
    ) -> AsyncIterator[str]:
        """Stream raw text tokens; ai_core parses any `<call>` element out of them.

        BOTH channels are read: reasoning deltas are wrapped in `<think>…</think>`
        and streamed alongside the output text, exactly as the other providers do.
        ai_core parses the RAW text (so a mid-thought call still dispatches) and
        persists it stripped."""
        import asyncio
        import json

        import httpx

        if not self._endpoint:
            raise RuntimeError(
                "responses_backend: no `endpoint` set — this connector does not "
                "guess a host. Name the full …/v1/responses URL on the agent "
                "record (endpoint=...)."
            )
        if not self._model:
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
        key = await self._key(self._endpoint)
        headers = {
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
        }

        # WIRE TRACE — `FANTASTIC_AI_WIRE=1` prints what actually left and what came
        # back: the outgoing budget and each completion's terminal status with its
        # item types (the status separates "the model chose to stop" from
        # "something bounded the output").
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
                            # a gateway is the link, not the request (#198)
                            raise ProviderError(
                                f"responses {resp.status_code}: {detail}",
                                transport=resp.status_code in (502, 503, 504),
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
                                # stripped before the turn is persisted; only the answer's calls run.
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
                            elif etype in (
                                "response.completed",
                                "response.incomplete",
                            ):
                                final = evt.get("response") or {}
                                # PROVIDER-REPORTED USAGE — what the window really held.
                                u = final.get("usage") or {}
                                self.last_prompt_tokens = (
                                    int(u.get("input_tokens") or 0) or None
                                )
                                self.last_completion_tokens = (
                                    int(u.get("output_tokens") or 0) or None
                                )
                            elif etype == "response.failed":
                                err = (evt.get("response") or {}).get("error")
                                raise RuntimeError(
                                    "responses stream failed: " + str(err)
                                )
                            elif etype == "error":
                                raise RuntimeError(
                                    "responses stream error: " + str(evt)
                                )
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
