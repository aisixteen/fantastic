"""AnthropicProvider — streaming chat over the Anthropic Messages API, RAW TEXT.

Matches the OllamaProvider contract: `chat(messages)` takes plain chat messages
(role system/user/assistant, text content) and yields `str` content tokens ONLY.
Tool-calling is owned by ai_core, which teaches the `<tool_call>` envelope in the
prompt and parses it out of this text stream. (ai_core's `_render_for_model`
already maps tool replies to plain user-role text before we see them.)

OPTIONAL `tool_use` ROUTE. Set `tool_use: true` on the record and this provider
declares the one `send` tool and TRANSLATES the `tool_use` blocks it receives
back into that same `<tool_call>` text before yielding. ai_core is unchanged and
never sees a native block: one parser, one envelope, backends still
interchangeable. It is a dialect this provider speaks to its own endpoint, not a
second tool-calling contract — which is why the translation lives here and not in
`tool_parse`, where one model's quirk would land on every model.

Internally translates to the Anthropic wire shape: a top-level `system` string +
alternating user/assistant text turns. The key is read from `ANTHROPIC_KEY` (or
`ANTHROPIC_API_KEY`) at call time (`.env` is loaded into os.environ at boot).
"""

from __future__ import annotations

import os
from typing import AsyncIterator

from ai_core.core import THINK_CLOSE, THINK_OPEN

DEFAULT_ENDPOINT = "https://api.anthropic.com/v1/messages"
DEFAULT_MODEL = "claude-opus-4-8"  # latest, smartest
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

# The ONE tool `ai_core` teaches in prose. Sent only on the `tool_use` route, and
# it is a TRANSLATION of the same contract — not a second one. Kept beside the
# translator so the two can never describe different things.
SEND_TOOL = {
    "name": "send",
    "description": (
        "Reach any agent by id. The payload's `type` names the verb; "
        "{type:'reflect'} returns that agent's verbs and state. `reply_to` is "
        "where the reply goes — omit for yourself, name another agent to hand it "
        "to them, or null to discard."
    ),
    "input_schema": {
        "type": "object",
        "properties": {
            "target_id": {"type": "string"},
            "payload": {"type": "object"},
            "reply_to": {"type": ["string", "null"]},
        },
        "required": ["target_id", "payload"],
    },
}


def _to_anthropic(messages: list[dict]) -> tuple[str | None, list[dict]]:
    """Plain messages → (system string, Anthropic messages).

    - role:system → the top-level `system` string.
    - role:user / role:assistant → {role, content:<str>}.
    Tool calls/replies are already inline text (`<tool_call>`/`<tool_response>`),
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
    def __init__(
        self,
        endpoint: str = DEFAULT_ENDPOINT,
        model: str = DEFAULT_MODEL,
        *,
        tool_use: bool = False,
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
        # TRANSLATE, do not adopt. With `tool_use` this provider declares the one
        # `send` tool and converts the `tool_use` blocks it gets back into the
        # SAME `<tool_call>` text `ai_core` already parses. The tool-calling
        # contract stays owned by `ai_core` — exactly one parser, one envelope,
        # every backend interchangeable — and this stays what it should be: a
        # provider speaking its own endpoint's dialect.
        #
        # Why it exists: an endpoint may parse a model's NATIVE syntax better
        # than we can from raw text. the model reverts to
        # `<|im_start|>function=NAME>` under load; on the raw route that is a
        # silent no-op turn, while the `/cc` route recovers it server-side
        # (measured by the proxy's own A/B: 1 turn / 0 calls -> 5 turns with real
        # work). Teaching OUR parser that syntax would put one model's quirk in
        # the layer every model shares.
        self._tool_use = tool_use

    async def chat(self, messages: list[dict]) -> AsyncIterator[str]:
        """Stream raw text tokens. ai_core parses any `<tool_call>` envelope out
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

        key = os.environ.get("ANTHROPIC_KEY") or os.environ.get("ANTHROPIC_API_KEY")
        if not key:
            raise RuntimeError("ANTHROPIC_KEY not set in environment (.env)")

        system, anth_messages = _to_anthropic(messages)
        body: dict = {
            "model": self._model,
            "max_tokens": self._max_tokens,
            "messages": anth_messages,
            "stream": True,
        }
        if system:
            body["system"] = system
        if self._tool_use:
            body["tools"] = [SEND_TOOL]
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
                        tool_name: str | None = None
                        tool_args = ""
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
                                # `<tool_call>` the model wrote MID-THOUGHT is lost with it.
                                # A raw text protocol reads the text the model produced; it does
                                # not let the provider decide which field counts as output.
                                # WRAPPED once (see `ai_core.core.without_thinking`): parsed for
                                # tool calls, stripped before the turn is persisted.
                                if (
                                    dtype == "input_json_delta"
                                    and tool_name is not None
                                ):
                                    tool_args += delta.get("partial_json") or ""
                                elif dtype == "thinking_delta":
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
                                    tool_name = cb.get("name") or "send"
                                    tool_args = ""
                            elif (
                                etype == "content_block_stop" and tool_name is not None
                            ):
                                # One `tool_use` block finished: re-emit it as the text
                                # envelope `ai_core` parses. Same shape a raw-route model
                                # would have written itself.
                                try:
                                    args = json.loads(tool_args or "{}")
                                except json.JSONDecodeError:
                                    args = {}
                                yield (
                                    "<tool_call>"
                                    + json.dumps({"name": tool_name, "arguments": args})
                                    + "</tool_call>"
                                )
                                tool_name, tool_args = None, ""
                            elif etype == "message_delta":
                                stop_reason = (evt.get("delta") or {}).get(
                                    "stop_reason"
                                )
                            elif etype == "error":
                                err = evt.get("error") or {}
                                raise RuntimeError(f"anthropic stream error: {err}")
                            # message_start / content_block_start/stop / ping: ignored
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
