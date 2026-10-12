"""AnthropicProvider — raw-text streaming over Anthropic's Messages API.

`POST {endpoint}` (the record's Messages URL, e.g. `https://api.anthropic.com/v1/
messages`) with `stream: true`. NO native tools: no `tools` array is ever sent, and
ai_core parses the `<call>` elements out of the streamed text, as for every backend.

What is Anthropic-specific here:

- ONE `system` string, top level — every system message ai_core assembles is joined
  into it; `messages` holds only `user` / `assistant`, consecutive turns of one role
  merged into one (the API alternates roles).
- `max_tokens` is REQUIRED by the API on every request: the record's (read from the
  model list when left out), or a side call's own bound.
- THE THOUGHT CHANNEL: with `thinking_budget` set, the request asks for extended
  thinking and `thinking_delta`s stream before the answer; they are wrapped once in
  `<think>…</think>` (stripped before the turn is stored; only the answer's calls run).
- The SSE stream ends with `message_stop`; a stream that ends without it did not
  finish. An `error` event carries the API's own message, and is raised with it.
- THE KEY: the record's `credentials` ref, read before each request and sent as
  `x-api-key` (with `anthropic-version`); no `credentials`, no key header — the API
  then answers 401, and the error says to name one.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator

import httpx
from ai_core.core import THINK_CLOSE, THINK_OPEN

#: The API version every request names.
ANTHROPIC_VERSION = "2023-06-01"
#: Seconds to reach the API. The stream itself has no read clock: the turn's one
#: bound is ai_core's send timeout.
CONNECT_TIMEOUT = 15.0
#: Below this a side call gets no thinking: the API needs budget_tokens >= 1024 and
#: below max_tokens.
THINKING_FLOOR = 1024


def headers(key: str | None) -> dict:
    """The headers of every request; the key only when the record names one."""
    out = {"anthropic-version": ANTHROPIC_VERSION, "content-type": "application/json"}
    if key:
        out["x-api-key"] = key
    return out


def to_messages(messages: list[dict]) -> tuple[str, list[dict]]:
    """ai_core's messages → (`system`, the user/assistant turns). Every system
    message joins the one `system` string; consecutive turns of one role are merged
    into one turn, blank-line separated."""
    systems: list[str] = []
    turns: list[dict] = []
    for m in messages:
        role = m.get("role")
        content = str(m.get("content") or "")
        if role == "system":
            if content:
                systems.append(content)
        elif role in ("user", "assistant"):
            if turns and turns[-1]["role"] == role:
                turns[-1]["content"] += "\n\n" + content
            else:
                turns.append({"role": role, "content": content})
    return "\n\n".join(systems), turns


class AnthropicProvider:
    #: The prompt size the API REPORTED for the last pass; ai_core calibrates with it.
    last_prompt_tokens: int | None = None
    #: The output tokens the server reported for the last pass (None = not reported).
    last_completion_tokens: int | None = None
    last_stop_reason: str | None = None

    def __init__(
        self,
        *,
        endpoint: str,
        model: str,
        key,
        max_tokens: int,
        temperature: float | None = None,
        thinking_budget: int | None = None,
        transport=None,
    ):
        self._endpoint = endpoint.rstrip("/")
        self._model = model
        # `async key(endpoint) -> str | None`, awaited before EACH request.
        self._key = key
        self._max_tokens = int(max_tokens)
        self._temperature = temperature
        self._thinking_budget = thinking_budget
        self._transport = transport

    def body(self, messages: list[dict], max_tokens: int | None) -> dict:
        system, turns = to_messages(messages)
        call_max = int(max_tokens) if max_tokens is not None else self._max_tokens
        body: dict = {
            "model": self._model,
            "max_tokens": call_max,
            "messages": turns,
            "stream": True,
        }
        if system:
            body["system"] = system
        if self._temperature is not None:
            body["temperature"] = self._temperature
        # Thinking only where the bound leaves room for it and an answer after it.
        budget = self._thinking_budget
        if budget and THINKING_FLOOR <= budget < call_max:
            body["thinking"] = {"type": "enabled", "budget_tokens": int(budget)}
        return body

    async def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        body = self.body(messages, max_tokens)
        key = await self._key(self._endpoint)
        thinking = False
        stopped = False
        try:
            async with (
                httpx.AsyncClient(
                    timeout=httpx.Timeout(None, connect=CONNECT_TIMEOUT),
                    transport=self._transport,
                ) as client,
                client.stream(
                    "POST", self._endpoint, json=body, headers=headers(key)
                ) as resp,
            ):
                if resp.status_code >= 400:
                    detail = (await resp.aread()).decode("utf-8", "replace")[:500]
                    why = (
                        " — the API wants a key: name it in `credentials` ({store, ref})"
                        if resp.status_code in (401, 403)
                        else ""
                    )
                    raise RuntimeError(
                        f"anthropic_backend: {self._endpoint} answered HTTP "
                        f"{resp.status_code}{why}: {detail}"
                    )
                async for line in resp.aiter_lines():
                    if not line.startswith("data:"):
                        continue
                    data = line[5:].strip()
                    if not data:
                        continue
                    evt = json.loads(data)
                    kind = evt.get("type")
                    if kind == "message_start":
                        u = (evt.get("message") or {}).get("usage") or {}
                        total = sum(
                            int(u.get(k) or 0)
                            for k in (
                                "input_tokens",
                                "cache_creation_input_tokens",
                                "cache_read_input_tokens",
                            )
                        )
                        self.last_prompt_tokens = total or None
                    elif kind == "content_block_delta":
                        delta = evt.get("delta") or {}
                        if delta.get("type") == "thinking_delta":
                            text = delta.get("thinking") or ""
                            if text:
                                if not thinking:
                                    thinking = True
                                    yield THINK_OPEN
                                yield text
                        elif delta.get("type") == "text_delta":
                            text = delta.get("text") or ""
                            if text:
                                if thinking:
                                    thinking = False
                                    yield THINK_CLOSE
                                yield text
                    elif kind == "message_delta":
                        reason = (evt.get("delta") or {}).get("stop_reason")
                        if reason:
                            self.last_stop_reason = reason
                        out = (evt.get("usage") or {}).get("output_tokens")
                        if out:
                            self.last_completion_tokens = int(out)
                    elif kind == "message_stop":
                        stopped = True
                    elif kind == "error":
                        err = evt.get("error") or {}
                        raise RuntimeError(
                            f"anthropic_backend: {self._endpoint} stream error: "
                            f"{err.get('type')}: {err.get('message')}"
                        )
                    # ping / content_block_start / content_block_stop /
                    # signature_delta: nothing to stream
        except httpx.ConnectTimeout as e:
            raise RuntimeError(
                f"anthropic_backend: no answer from {self._endpoint} within "
                f"{CONNECT_TIMEOUT:g}s"
            ) from e
        except httpx.ConnectError as e:
            raise RuntimeError(
                f"anthropic_backend: could not connect to {self._endpoint}"
            ) from e
        except (httpx.ReadError, httpx.RemoteProtocolError) as e:
            raise RuntimeError(
                f"anthropic_backend: {self._endpoint} dropped the stream mid-answer; "
                "this turn is NOT complete"
            ) from e
        if thinking:
            yield THINK_CLOSE
        if not stopped:
            raise RuntimeError(
                f"anthropic_backend: the stream from {self._endpoint} ended without "
                "message_stop; this turn is NOT complete"
            )
        # A MAIN turn cut at the record's `max_tokens` did not finish — it fails,
        # naming the cut. A bounded side call (a fold summary) that stopped at its
        # bound got what it asked for; its caller reads `last_stop_reason`.
        if max_tokens is None and self.last_stop_reason == "max_tokens":
            raise RuntimeError(
                f"anthropic_backend: the turn was cut at max_tokens={self._max_tokens} "
                "(stop_reason=max_tokens) — it did not finish; raise max_tokens on "
                "the record, or lower thinking_budget"
            )

    @property
    def model(self) -> str:
        return self._model
