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
- USAGE: each stream's token counts accumulate into the agent's `Usage` (the
  `usage` verb and `reflect` read it), side calls included. The running total is
  kept across a kernel restart in the agent's `usage.json` (#308, Rust #319): every
  count change rewrites it (`persist_to`), so a stream that dies mid-answer keeps
  what it billed.
"""

from __future__ import annotations

import json
import os
import time
from collections.abc import AsyncIterator
from pathlib import Path

import httpx
from ai_core.provider import ProviderError
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


#: Models whose cache reads bill at 0.05x the input price; every other model reads at
#: 0.1x. A dated id of one of these (`claude-sonnet-5-5-<date>`) matches too.
CHEAP_READ_MODELS = ("claude-sonnet-5-5", "claude-opus-5-5")


class CacheRates:
    """The three cache rates of one price bucket, USD per million tokens."""

    __slots__ = ("read", "write_5m", "write_1h")

    def __init__(self, read: float, write_5m: float, write_1h: float):
        self.read = read
        self.write_5m = write_5m
        self.write_1h = write_1h


class CachePrices:
    """The record's explicit cache prices, USD per million tokens; `None` = unset."""

    __slots__ = ("read", "write_5m", "write_1h")

    def __init__(self, read=None, write_5m=None, write_1h=None):
        self.read = read
        self.write_5m = write_5m
        self.write_1h = write_1h


def model_cache_rates(model: str, price_in: float) -> CacheRates:
    """The default cache rates of `model` at the input price `price_in`: a 5-minute
    write 1.25x, a 1-hour write 2x, a read 0.1x (0.05x for the CHEAP_READ_MODELS)."""
    cheap = any(model == m or model.startswith(m + "-") for m in CHEAP_READ_MODELS)
    return CacheRates(
        read=price_in * (0.05 if cheap else 0.1),
        write_5m=price_in * 1.25,
        write_1h=price_in * 2.0,
    )


def bucket_rates(
    model: str, bucket_in: float, base_in: float, prices: CachePrices
) -> CacheRates:
    """The rates one bucket bills at: the record's explicit cache price when set, else
    the model default at the bucket's input price `bucket_in`. An explicit price is USD
    at the base input price `base_in`, so the long-context bucket scales it by
    `bucket_in / base_in` (unscaled when `base_in` is not positive)."""
    default = model_cache_rates(model, bucket_in)

    def scaled(explicit: float) -> float:
        return explicit * bucket_in / base_in if base_in > 0 else explicit

    return CacheRates(
        read=default.read if prices.read is None else scaled(prices.read),
        write_5m=default.write_5m
        if prices.write_5m is None
        else scaled(prices.write_5m),
        write_1h=default.write_1h
        if prices.write_1h is None
        else scaled(prices.write_1h),
    )


def bucket_cost(
    model: str,
    tokens: tuple,
    pi: float,
    po: float,
    base_in: float,
    prices: CachePrices,
) -> tuple[float, float]:
    """A bucket's (cost, saved) in USD. `tokens` is (input, 5-minute write, 1-hour
    write, read, output). Cost: every token at its rate. Saved: the same tokens
    against no cache, reads and writes at the input price."""
    inp, w5, w1, rd, out = tokens
    r = bucket_rates(model, pi, base_in, prices)
    cost = inp * pi + w5 * r.write_5m + w1 * r.write_1h + rd * r.read + out * po
    saved = rd * (pi - r.read) - w5 * (r.write_5m - pi) - w1 * (r.write_1h - pi)
    return cost / 1e6, saved / 1e6


#: The attributes of a `Usage` that are counts, in the order `counters()` lists them.
_COUNT_ATTRS = (
    "requests",
    "input_tokens",
    "cache_write_tokens",
    "cache_write_1h_tokens",
    "cache_read_tokens",
    "output_tokens",
    "long_requests",
    "long_input_tokens",
    "long_cache_write_tokens",
    "long_cache_write_1h_tokens",
    "long_cache_read_tokens",
    "long_output_tokens",
)
#: The usage.json keys, one per count, the same names as Rust's sidecar.
FIELDS = (
    "requests",
    "input",
    "cache_write",
    "cache_write_1h",
    "cache_read",
    "output",
    "long_requests",
    "long_input",
    "long_cache_write",
    "long_cache_write_1h",
    "long_cache_read",
    "long_output",
)
#: The sidecar's file name, beside the agent's agent.yaml.
USAGE_FILE = "usage.json"


class Usage:
    """One agent's token counts: since this kernel process booted, or the running
    total a `usage.json` sidecar carries. `input_tokens` is the uncached remainder;
    the two cache counts are what the API reports for the prompt."""

    def __init__(self):
        self.reset()

    def counters(self) -> list[int]:
        """The twelve counts, in `FIELDS` order."""
        return [getattr(self, a) for a in _COUNT_ATTRS]

    def plus(self, other: Usage) -> Usage:
        """This usage's counts plus `other`'s. The log error comes from `other`, the
        since-boot side that carries it."""
        out = Usage()
        for attr, mine, theirs in zip(_COUNT_ATTRS, self.counters(), other.counters()):
            setattr(out, attr, mine + theirs)
        out.log_error = other.log_error
        return out

    def to_json(self) -> dict:
        """The sidecar's JSON: the twelve counts, by `FIELDS`."""
        return dict(zip(FIELDS, self.counters()))

    @classmethod
    def from_json(cls, data) -> Usage:
        """The counts a sidecar holds. A missing or non-integer key reads as 0, and a
        value that is not an object reads as all zeros."""
        out = cls()
        if not isinstance(data, dict):
            return out
        for attr, key in zip(_COUNT_ATTRS, FIELDS):
            v = data.get(key)
            if isinstance(v, int) and not isinstance(v, bool):
                setattr(out, attr, v)
        return out

    def reset(self) -> None:
        self.requests = 0
        self.input_tokens = 0
        # cache_write_tokens is the 5-minute part of the writes; the 1-hour part is
        # counted apart (its own price). snapshot reports the total of both.
        self.cache_write_tokens = 0
        self.cache_write_1h_tokens = 0
        self.cache_read_tokens = 0
        self.output_tokens = 0
        # Requests whose prompt was over the record's `long_context_tokens` (#236):
        # their own bucket, priced at the long-context prices.
        self.long_requests = 0
        self.long_input_tokens = 0
        self.long_cache_write_tokens = 0
        self.long_cache_write_1h_tokens = 0
        self.long_cache_read_tokens = 0
        self.long_output_tokens = 0
        # The last usage-log write error, reported in `usage`.
        self.log_error: str | None = None

    def add_request(
        self,
        input_tokens: int,
        cache_write: int,
        cache_read: int,
        long: bool = False,
        cache_write_1h: int = 0,
    ) -> None:
        """One request's counts. `cache_write` is the whole write, the 1-hour part
        (`cache_write_1h`) included; the 5-minute rest is what the write counts keep."""
        write_5m = cache_write - cache_write_1h
        if long:
            self.long_requests += 1
            self.long_input_tokens += input_tokens
            self.long_cache_write_tokens += write_5m
            self.long_cache_write_1h_tokens += cache_write_1h
            self.long_cache_read_tokens += cache_read
        else:
            self.requests += 1
            self.input_tokens += input_tokens
            self.cache_write_tokens += write_5m
            self.cache_write_1h_tokens += cache_write_1h
            self.cache_read_tokens += cache_read

    def add_output(self, n: int, long: bool = False) -> None:
        """Output tokens of the request in flight, in its own bucket."""
        if long:
            self.long_output_tokens += n
        else:
            self.output_tokens += n

    def snapshot(
        self,
        price_in=None,
        price_out=None,
        long_in=None,
        long_out=None,
        model="",
        cache_read=None,
        cache_write_5m=None,
        cache_write_1h=None,
    ) -> dict:
        """The counts and what they derive to. `cost_usd` and `saved_usd` (USD per
        million tokens, from the record's prices) are reported only when BOTH base
        prices are set. `long_in` / `long_out` price the long-context bucket; unset
        (either one), the base prices do. `model` picks the default cache rates;
        `cache_read` / `cache_write_5m` / `cache_write_1h` override them (USD per
        million tokens at the base input price)."""
        inp = self.input_tokens + self.long_input_tokens
        cw = (
            self.cache_write_tokens
            + self.cache_write_1h_tokens
            + self.long_cache_write_tokens
            + self.long_cache_write_1h_tokens
        )
        cr = self.cache_read_tokens + self.long_cache_read_tokens
        prompt = inp + cw + cr
        out = {
            "requests": self.requests + self.long_requests,
            "input_tokens": inp,
            "cache_write_tokens": cw,
            "cache_read_tokens": cr,
            "output_tokens": self.output_tokens + self.long_output_tokens,
            "prompt_tokens": prompt,
            "hit_rate": cr / prompt if prompt else 0,
            "saved_tokens": saved_tokens(cr, cw),
            "long_context_requests": self.long_requests,
        }
        if self.log_error is not None:
            out["usage_log_error"] = self.log_error
        if price_in is not None and price_out is not None:
            if long_in is None or long_out is None:
                long_in, long_out = price_in, price_out
            prices = CachePrices(cache_read, cache_write_5m, cache_write_1h)
            base = (
                self.input_tokens,
                self.cache_write_tokens,
                self.cache_write_1h_tokens,
                self.cache_read_tokens,
                self.output_tokens,
            )
            long = (
                self.long_input_tokens,
                self.long_cache_write_tokens,
                self.long_cache_write_1h_tokens,
                self.long_cache_read_tokens,
                self.long_output_tokens,
            )
            c1, s1 = bucket_cost(model, base, price_in, price_out, price_in, prices)
            c2, s2 = bucket_cost(model, long, long_in, long_out, price_in, prices)
            out["cost_usd"] = c1 + c2
            out["saved_usd"] = s1 + s2
        return out


def write_usage_file(path: str | Path, total: Usage) -> None:
    """Rewrite a usage.json with `total`: a temp file, then a rename, so a crash
    leaves the old file or the new one, never half of one. Raises OSError."""
    tmp = f"{path}.tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(total.to_json(), f)
    os.replace(tmp, path)


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
        base: Usage | None = None,
        persist_to: str | Path | None = None,
        transport=None,
        long_context_tokens: int | None = None,
        usage_log: str | None = None,
        agent_id: str = "",
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
        # The counts from before this kernel process (the sidecar's), and the file
        # the running total is written to after each count change. None = not kept.
        self._base = base
        self._persist_to = persist_to
        self._transport = transport
        # Prompt tokens above which a request is billed at the long-context prices.
        self._long_context_tokens = long_context_tokens
        # The file each request appends one JSON usage line to (#236), and the agent
        # id written into each line.
        self._usage_log = usage_log
        self._agent_id = agent_id

    def _persist(self) -> None:
        """Rewrite the agent's usage.json with the running total: the base plus this
        process's counts. A write error does not fail the turn: it is kept in `Usage`
        and reported. A no-op when no file is named (an in-memory agent)."""
        if self._persist_to is None:
            return
        base = self._base if self._base is not None else Usage()
        boot = self._usage if self._usage is not None else Usage()
        try:
            write_usage_file(self._persist_to, base.plus(boot))
        except OSError as e:
            if self._usage is not None:
                self._usage.log_error = f"{self._persist_to}: {e}"

    def _log_usage(self, counts: dict, long: bool) -> None:
        """Append this request's own counts as one JSON line to the usage log. A
        write error does not fail the turn: it is kept in `Usage` and reported."""
        if not self._usage_log:
            return
        line = {
            "ts": time.time(),
            "agent": self._agent_id,
            "model": self._model,
            **counts,
            "long_context": long,
        }
        try:
            with open(self._usage_log, "a", encoding="utf-8") as f:
                f.write(json.dumps(line) + "\n")
        except OSError as e:
            if self._usage is not None:
                self._usage.log_error = f"{self._usage_log}: {e}"

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
        # THIS request's own counts and tier (#236): its usage-log line.
        counts = {
            "input_tokens": 0,
            "cache_write_tokens": 0,
            "cache_read_tokens": 0,
            "output_tokens": 0,
        }
        long = False
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
                    # a gateway or an overloaded API (529) is the link, not the request: transport (#198)
                    raise ProviderError(
                        f"anthropic_backend: {self._endpoint} answered HTTP "
                        f"{resp.status_code}{why}: {detail}",
                        transport=resp.status_code in (502, 503, 504, 529),
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
                        # the 1-hour part of the write, priced apart; the rest is 5-minute
                        cw1 = int(
                            (u.get("cache_creation") or {}).get(
                                "ephemeral_1h_input_tokens"
                            )
                            or 0
                        )
                        cw1 = min(max(cw1, 0), cw)
                        self.last_prompt_tokens = (inp + cw + cr) or None
                        limit = self._long_context_tokens
                        long = limit is not None and inp + cw + cr > limit
                        counts.update(
                            input_tokens=inp,
                            cache_write_tokens=cw,
                            cache_read_tokens=cr,
                        )
                        if self._usage is not None:
                            self._usage.add_request(
                                inp, cw, cr, long=long, cache_write_1h=cw1
                            )
                            self._persist()
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
                            step = int(out) - billed_out
                            if self._usage is not None:
                                self._usage.add_output(step, long=long)
                                self._persist()
                            counts["output_tokens"] += step
                            billed_out = int(out)
                    elif kind == "message_stop":
                        stopped = True
                        self._log_usage(counts, long)
                    elif kind == "error":
                        err = evt.get("error") or {}
                        raise RuntimeError(
                            f"anthropic_backend: {self._endpoint} stream error: "
                            f"{err.get('type')}: {err.get('message')}"
                        )
                    # ping / content_block_start / content_block_stop /
                    # signature_delta: nothing to stream
        except httpx.ConnectTimeout as e:
            raise ProviderError(
                f"anthropic_backend: no answer from {self._endpoint} within "
                f"{CONNECT_TIMEOUT:g}s",
                transport=True,
            ) from e
        except httpx.ConnectError as e:
            raise ProviderError(
                f"anthropic_backend: could not connect to {self._endpoint}",
                transport=True,
            ) from e
        except (httpx.ReadError, httpx.RemoteProtocolError) as e:
            raise ProviderError(
                f"anthropic_backend: {self._endpoint} dropped the stream mid-answer; "
                "this turn is NOT complete",
                transport=True,
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
