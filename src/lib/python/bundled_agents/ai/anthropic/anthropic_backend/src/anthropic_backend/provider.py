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
- CACHING (ai_core's `caching`): `provider_cache` sends ephemeral `cache_control`
  markers: on the `system` block, on the block that ends the primer row, on the
  block that ends the last fold, and the top-level automatic one. At most four.
  Every other mode sends none, and the body is unchanged.
- USAGE: each stream's token counts accumulate into the agent's `Usage` (RAM; the
  `usage` verb and `reflect` read it), side calls included.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator

import httpx
from ai_core.core import THINK_CLOSE, THINK_OPEN
from ai_core import caching as ai_caching

#: The API version every request names.
ANTHROPIC_VERSION = "2023-06-01"
#: Seconds to reach the API. The stream itself has no read clock: the turn's one
#: bound is ai_core's send timeout.
CONNECT_TIMEOUT = 15.0
#: Below this a side call gets no thinking: the API needs budget_tokens >= 1024 and
#: below max_tokens.
THINKING_FLOOR = 1024
#: The cache marker a cached request sends: the 5-minute ephemeral TTL, the only one.
CACHE_CONTROL = {"type": "ephemeral"}


def headers(key: str | None) -> dict:
    """The headers of every request; the key only when the record names one."""
    out = {"anthropic-version": ANTHROPIC_VERSION, "content-type": "application/json"}
    if key:
        out["x-api-key"] = key
    return out


def saved_tokens(cache_read: int, cache_write: int) -> int:
    """round(0.9 * cache_read - 0.25 * cache_write), in full-price input tokens: a
    read costs 0.1x the base input price, a write 1.25x. Exact integer arithmetic,
    halves to even, so every runtime agrees. May be negative."""
    q, r = divmod(18 * cache_read - 5 * cache_write, 20)
    if 2 * r > 20 or (2 * r == 20 and q % 2 == 1):
        q += 1
    return q


class Usage:
    """One agent's token counts since its kernel started (RAM — a restart zeroes
    them). `input_tokens` is the uncached remainder; the two cache counts are what
    the API reports for the prompt."""

    def __init__(self):
        self.reset()

    def reset(self) -> None:
        self.requests = 0
        self.input_tokens = 0
        self.cache_write_tokens = 0
        self.cache_read_tokens = 0
        self.output_tokens = 0

    def add_request(self, input_tokens: int, cache_write: int, cache_read: int) -> None:
        self.requests += 1
        self.input_tokens += input_tokens
        self.cache_write_tokens += cache_write
        self.cache_read_tokens += cache_read

    def snapshot(self, price_in=None, price_out=None) -> dict:
        """The counts and what they derive to. `cost_usd` and `saved_usd` (USD per
        million tokens, from the record's prices) are reported only when BOTH prices
        are set."""
        prompt = self.input_tokens + self.cache_write_tokens + self.cache_read_tokens
        saved = saved_tokens(self.cache_read_tokens, self.cache_write_tokens)
        out = {
            "requests": self.requests,
            "input_tokens": self.input_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "output_tokens": self.output_tokens,
            "prompt_tokens": prompt,
            "hit_rate": self.cache_read_tokens / prompt if prompt else 0,
            "saved_tokens": saved,
        }
        if price_in is not None and price_out is not None:
            cost_in = (
                self.input_tokens
                + 1.25 * self.cache_write_tokens
                + 0.1 * self.cache_read_tokens
            )
            out["cost_usd"] = (
                cost_in * price_in / 1e6 + self.output_tokens * price_out / 1e6
            )
            out["saved_usd"] = saved * price_in / 1e6
        return out


def to_messages(messages: list[dict]) -> tuple[str, list[dict]]:
    """ai_core's messages → (`system`, the user/assistant turns). Every system
    message joins the one `system` string; consecutive turns of one role are merged
    into one turn, blank-line separated. The cache flags are not read: see
    `to_wire`."""
    return to_wire(messages, False)


def to_wire(messages: list[dict], marks: bool) -> tuple[str, list[dict]]:
    """ai_core's messages → (`system`, the turns as the request sends them). `marks`
    reads the cache flags (`cache_primer`, `cache_fold`); off, they are ignored and
    the turns are the plain merged strings.

    With `marks`, a merged turn whose rows carry flags is cut into text blocks at
    the end of each flagged row's text, so the marker lands there, not at the end of
    the merged turn. The block texts concatenate to the merged string exactly (see
    `cut_blocks`)."""
    systems: list[str] = []
    turns: list[tuple[str, list[tuple[str, bool]]]] = []
    for m in messages:
        role = m.get("role")
        content = str(m.get("content") or "")
        flagged = marks and (
            m.get("cache_primer") is True or m.get("cache_fold") is True
        )
        if role == "system":
            if content:
                systems.append(content)
        elif role in ("user", "assistant"):
            if turns and turns[-1][0] == role:
                turns[-1][1].append((content, flagged))
            else:
                turns.append((role, [(content, flagged)]))
    return "\n\n".join(systems), [
        {"role": role, "content": turn_content(rows)} for role, rows in turns
    ]


def turn_content(rows: list[tuple[str, bool]]) -> str | list[dict]:
    """One merged turn's `content`: the plain string when no row in it is flagged,
    else its text blocks, the marked ones carrying the cache marker."""
    if not any(flagged for _, flagged in rows):
        return "\n\n".join(text for text, _ in rows)
    out = []
    for text, marked in cut_blocks(rows):
        block: dict = {"type": "text", "text": text}
        if marked:
            block["cache_control"] = dict(CACHE_CONTROL)
        out.append(block)
    return out


def cut_blocks(rows: list[tuple[str, bool]]) -> list[tuple[str, bool]]:
    """A merged turn cut at the end of each flagged row: (block text, ends a flagged
    row). The texts join to the merged string. A blank tail joins the last block."""
    out: list[tuple[str, bool]] = []
    cur = ""
    for i, (text, flagged) in enumerate(rows):
        if i > 0:
            cur += "\n\n"
        cur += text
        if flagged and text and cur.strip():
            out.append((cur, True))
            cur = ""
    if cur.strip() or not out:
        out.append((cur, False))
    else:
        last, marked = out[-1]
        out[-1] = (last + cur, marked)
    return out


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
        caching: str = ai_caching.DEFAULT_MODE,
        usage: Usage | None = None,
        transport=None,
    ):
        self._endpoint = endpoint.rstrip("/")
        self._model = model
        # `async key(endpoint) -> str | None`, awaited before EACH request.
        self._key = key
        self._max_tokens = int(max_tokens)
        self._temperature = temperature
        self._thinking_budget = thinking_budget
        self._marks = ai_caching.marks(caching)
        # The agent's running counts; None = nothing is counted (a bare provider).
        self._usage = usage
        self._transport = transport

    def body(self, messages: list[dict], max_tokens: int | None) -> dict:
        # Markers only in provider_cache: every other mode's body is the plain one.
        marks = self._marks
        system, turns = to_wire(messages, marks)
        call_max = int(max_tokens) if max_tokens is not None else self._max_tokens
        body: dict = {
            "model": self._model,
            "max_tokens": call_max,
            "messages": turns,
            "stream": True,
        }
        if system:
            body["system"] = (
                [{"type": "text", "text": system, "cache_control": dict(CACHE_CONTROL)}]
                if marks
                else system
            )
        if marks:
            body["cache_control"] = dict(CACHE_CONTROL)
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
        # The output tokens this stream has already billed to the agent's usage.
        billed_out = 0
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
                        inp, cw, cr = (
                            int(u.get(k) or 0)
                            for k in (
                                "input_tokens",
                                "cache_creation_input_tokens",
                                "cache_read_input_tokens",
                            )
                        )
                        self.last_prompt_tokens = (inp + cw + cr) or None
                        if self._usage is not None:
                            self._usage.add_request(inp, cw, cr)
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
                            # message_delta carries the running total: bill the step.
                            if self._usage is not None:
                                self._usage.output_tokens += int(out) - billed_out
                            billed_out = int(out)
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
