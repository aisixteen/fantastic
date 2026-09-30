"""AnthropicProvider — streaming chat over the Anthropic Messages API, RAW TEXT.

Matches the OllamaProvider contract: `chat(messages)` takes plain chat messages
(role system/user/assistant, text content) and yields `str` content tokens ONLY.
Tool-calling is owned by ai_core, which teaches the `<call>` grammar in the prompt
and parses it out of this text stream. (ai_core's `_render_for_model` already maps
tool replies to plain user-role text before we see them.) This provider NEVER
declares `tools` and NEVER translates a native `tool_use` block: the model↔harness
path carries no JSON in either direction. A native block that arrives anyway (an
endpoint parsing the model's own tool syntax server-side) is a loud error, not a
silent drop — the model wrote a call in a grammar this kernel does not speak.

Internally translates to the Anthropic wire shape: a top-level `system` string +
alternating user/assistant text turns. The key is read from `ANTHROPIC_API_KEY`
at call time (`.env` is loaded into os.environ at boot).
"""

from __future__ import annotations

import os
from typing import AsyncIterator

from ai_core.core import THINK_CLOSE, THINK_OPEN

API_VERSION = "2023-06-01"

# EDGE TRANSPORT FAILURES — not answers, and not statements about the request.
# A 4xx, or any 5xx carrying a JSON body, is the endpoint DELIBERATELY answering:
# that is the caller's bug and it stays loud. These codes with a NON-JSON body are
# a CDN/proxy error page, meaning the request never reached anything that could
# judge it. Measured 2026-08-24: a `521` from a 1-2s gap while the reverse proxy
# restarted killed a 26-pass run at pass 5. Retrying the same single path on a
# transport failure is not a second mechanism — nothing was tried and nothing is
# being routed around.
TRANSPORT_CODES = frozenset({502, 503, 504, 520, 521, 522, 523, 524})
RETRIES = 2
BACKOFF = 1.5
# Per-turn OUTPUT budget. A FIELD, not a constant, because it is a per-model
# property: the thought of a reasoning model is spent from this same budget, so a
# value that leaves Opus room to finish truncates the model mid-thought. Measured
# 2026-08-23 on `/cc`: with 8192 the run died at turn 20 with
# `stop_reason='max_tokens' blocks=['thinking']` — a whole turn of thinking, no
# content, no tool call. Its own turns ran median ~2.3k / p90 ~5.9k tokens.
# The endpoint's ceiling is `max_tokens + prompt <= the model's context`.
MAX_TOKENS = 8192
#: The environment variable the key is read from, at call time — never the record.
KEY_VAR = "ANTHROPIC_API_KEY"


def _to_anthropic(messages: list[dict]) -> tuple[str | None, list[dict]]:
    """Plain messages → (system string, Anthropic messages).

    - role:system → the top-level `system` string.
    - role:user / role:assistant → {role, content:<str>}.
    Tool calls/replies are already inline text (`<call>` elements and `[reply to …]` blocks),
    so there is no tool_use/tool_result translation — pure text turns.
    """
    system: str | None = None
    out: list[dict] = []
    for m in messages:
        role = m.get("role")
        content = m.get("content", "") or ""
        if role == "system":
            system = content
        elif role in ("user", "assistant"):
            out.append({"role": role, "content": content or "(no content)"})
    return system, out


class AnthropicProvider:
    #: The prompt size the endpoint REPORTED for the last pass (None: not reported).
    #: ai_core calibrates its own token estimate with it.
    last_prompt_tokens: int | None = None
    last_stop_reason: str | None = None

    def __init__(
        self,
        endpoint: str,
        model: str,
        *,
        max_tokens: int = MAX_TOKENS,
        sampling: dict | None = None,
        transport=None,
    ):
        self._endpoint = endpoint
        self._model = model
        self._max_tokens = max_tokens
        # Extra body params merged verbatim into the request (endpoint-whitelisted
        # sampling knobs — repetition_penalty, no_repeat_ngram_size,
        # thinking_token_budget, top_p/top_k, …). A record edit, not an ai_core
        # concern. Unlisted params the proxy silently drops; None values are omitted.
        self._sampling = {k: v for k, v in (sampling or {}).items() if v is not None}
        # Test seam, same as LlamaCppProvider's: an httpx transport to hand the
        # client. None = the real network.
        self._transport = transport

    async def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        """Stream raw text tokens. ai_core parses any `<call>` element out
        of the text stream — including the ones this provider synthesises from
        `tool_use` blocks when the `tool_use` route is on.

        BOTH channels are read: a reasoning model's `thinking_delta` blocks are
        wrapped in `<think>…</think>` and streamed alongside the `text_delta`
        conclusion, exactly as the ollama/nvidia/llamacpp providers do. ai_core
        parses the RAW text (so a mid-thought tool call still dispatches) and
        persists it stripped."""
        import asyncio
        import json

        import httpx

        key = os.environ.get(KEY_VAR)
        if not key:
            raise RuntimeError(f"{KEY_VAR} not set in environment (.env)")
        if not self._model:
            raise RuntimeError(
                "anthropic_backend: no `model` set — this connector does not guess "
                "one. Name a model on the agent record (model=...)."
            )

        system, anth_messages = _to_anthropic(messages)
        body: dict = {
            "model": self._model,
            # A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
            "max_tokens": max_tokens or self._max_tokens,
            "messages": anth_messages,
            "stream": True,
        }
        if system:
            body["system"] = system
        # Endpoint-whitelisted sampling knobs, merged last so an explicit value wins.
        body.update(self._sampling)
        headers = {
            "x-api-key": key,
            "anthropic-version": API_VERSION,
            "content-type": "application/json",
        }

        # WIRE TRACE. `FANTASTIC_AI_WIRE=1` prints what actually left and what came
        # back — the outgoing `max_tokens` (the CONFIG is not evidence that the value
        # arrives) and each completion's `stop_reason` with its block types. The
        # stop_reason is the fact that separates "the model chose to stop" from
        # "something bounded the output", and nothing logged it before.
        wire = os.environ.get("FANTASTIC_AI_WIRE") == "1"
        if wire:
            print(
                f"[wire] -> max_tokens={body['max_tokens']} "
                f"tools={len(body.get('tools', []))} msgs={len(anth_messages)}",
                flush=True,
            )
        # NO READ CLOCK ON THE STREAM. 120s of read-silence used to abort turns
        # that were merely QUEUED behind other sessions — measured 2026-08-25
        # (10 harnesses on 7 slots, keyless endpoint): the two heavy probes died
        # on ReadTimeout mid-run while the model was legitimately generating.
        # The turn already has exactly one owned bound (FANTASTIC_AI_SEND_TIMEOUT
        # around the whole generation); a second, lower, transport-level clock
        # was redundant and had become the binding constraint. Connect stays
        # finite — a dead endpoint should fail fast, a slow one should not.
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
                        # JSON body = the endpoint answered deliberately; that is the
                        # caller's bug and stays loud. Non-JSON on a transport code =
                        # a CDN/proxy error page: the request never reached anything
                        # that could judge it.
                        if not (
                            resp.status_code in TRANSPORT_CODES
                            and "json" not in ctype.lower()
                            and attempt < RETRIES
                        ):
                            raise RuntimeError(
                                f"anthropic {resp.status_code}: {detail}"
                            )
                        attempt += 1
                        print(
                            f"[anthropic] {resp.status_code} "
                            f"({ctype or 'no content-type'}) — transport error, "
                            f"retry {attempt}/{RETRIES}",
                            flush=True,
                        )
                    else:
                        stop_reason = None
                        thinking = False
                        # What the stream actually CONTAINED. A bare `stop_reason` names the
                        # symptom and nothing else: measured 2026-08-21 (`live_plan`), a
                        # no-tools request came back `stop_reason='tool_use'`, which should be
                        # impossible, and the error could not say what the model had emitted.
                        # The block types are the one fact that distinguishes "the model wrote
                        # a native tool block" from "the connection died before any content".
                        blocks: list[str] = []
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
                            if etype == "content_block_delta":
                                delta = evt.get("delta") or {}
                                dtype = delta.get("type")
                                # REASONING MODELS SPLIT THEIR OUTPUT — the same trap the
                                # nvidia/ollama/llamacpp providers already guard. Anthropic's
                                # wire puts the thought in `thinking_delta` blocks and only the
                                # conclusion in `text_delta`; a model served through an
                                # anthropic-compatible proxy (the model via `/raw`) does this
                                # constantly. Reading `text_delta` alone silently drops whole
                                # turns: a pass spent entirely reasoning arrives as an EMPTY
                                # completion and the chain dies with no error anywhere — and a
                                # `<call>` the model wrote MID-THOUGHT is lost with it.
                                # A raw text protocol reads the text the model produced; it does
                                # not let the provider decide which field counts as output.
                                # WRAPPED once (see `ai_core.core.without_thinking`): parsed for
                                # tool calls, stripped before the turn is persisted.
                                if dtype == "thinking_delta":
                                    think = delta.get("thinking") or ""
                                    if think:
                                        if not thinking:
                                            thinking = True
                                            yield THINK_OPEN
                                        yield think
                                elif dtype == "text_delta":
                                    txt = delta.get("text") or ""
                                    if txt:
                                        if thinking:
                                            thinking = False
                                            yield THINK_CLOSE
                                        yield txt
                            elif etype == "content_block_start":
                                cb = evt.get("content_block") or {}
                                blocks.append(cb.get("type"))
                                if cb.get("type") == "tool_use":
                                    # The endpoint parsed a NATIVE tool call out of the
                                    # model's text (the `/cc` route does this server-side
                                    # whether or not `tools` were declared — measured
                                    # 2026-08-24). Its arguments are JSON we do not read:
                                    # the model wrote a call in a grammar this kernel does
                                    # not speak, and a silent drop would end the turn
                                    # looking finished with the call lost.
                                    raise RuntimeError(
                                        "anthropic stream carried a native tool_use block "
                                        f"({cb.get('name')!r}): the model wrote a JSON tool "
                                        "call, which this kernel does not parse — calls are "
                                        "`<call name=…>` text"
                                    )
                            elif etype == "message_start":
                                # PROVIDER-REPORTED USAGE — what the context window
                                # really held, cache reads included. ai_core calibrates
                                # its own estimate with it (`last_prompt_tokens`).
                                u = (evt.get("message") or {}).get("usage") or {}
                                n = sum(
                                    int(u.get(k) or 0)
                                    for k in (
                                        "input_tokens",
                                        "cache_read_input_tokens",
                                        "cache_creation_input_tokens",
                                    )
                                )
                                self.last_prompt_tokens = n or None
                            elif etype == "message_delta":
                                stop_reason = (evt.get("delta") or {}).get(
                                    "stop_reason"
                                )
                            elif etype == "error":
                                err = evt.get("error") or {}
                                raise RuntimeError(f"anthropic stream error: {err}")
                            # content_block_stop / ping: ignored
                        # A turn spent ENTIRELY reasoning ends inside the think block —
                        # close it so the marker pair is always balanced for the parser.
                        if thinking:
                            thinking = False
                            yield THINK_CLOSE
                        # A stream that did not END TURN did not finish — say so, loudly.
                        # This loop used to ignore `message_delta`, so a completion cut by
                        # `max_tokens`, a refusal, or a dropped connection was returned as
                        # if the model had finished. Measured 2026-08-07 (combined probe,
                        # run 6): a `create_agent` turn stopped at 1,085 chars mid-`code`,
                        # the partial parsed as an unterminated envelope, the next
                        # completion arrived EMPTY, and the chain died — three symptoms,
                        # one silent cause. A cut turn is an error the caller can see and
                        # retry; a fake-complete one is invisible.
                        # Terminal on what the stream CONTAINED, not on what we asked for.
                        # The `/cc` route parses the model's native tool syntax server-side
                        # and returns `tool_use` blocks whether or not the client declared
                        # any tools — measured 2026-08-24: a request with no `tools` key came
                        # back `stop_reason='tool_use'` over five real tool_use blocks, and
                        # gating this on `self._tool_use` failed the turn on arrival. A turn
                        # that ended BECAUSE the model called tools is a complete turn.
                        # Still an anomaly the other way: `tool_use` with no such block means
                        # the stream did not carry what its stop_reason claims.
                        if wire:
                            print(
                                f"[wire] <- stop_reason={stop_reason!r} blocks={blocks!r}",
                                flush=True,
                            )
                        terminal = ("end_turn", "stop_sequence") + (
                            ("tool_use",) if "tool_use" in blocks else ()
                        )
                        self.last_stop_reason = stop_reason
                        # A BOUNDED SIDE CALL that stopped at its bound is an ANSWER,
                        # not a fault: the caller asked for at most `max_tokens` and
                        # reads `last_stop_reason` (a fold refuses a cut summary).
                        if max_tokens is not None and stop_reason == "max_tokens":
                            return
                        if stop_reason not in terminal:
                            raise RuntimeError(
                                "anthropic stream ended without end_turn: "
                                f"stop_reason={stop_reason!r} blocks={blocks!r}"
                                + (
                                    " (none seen — connection cut mid-stream)"
                                    if stop_reason is None
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
