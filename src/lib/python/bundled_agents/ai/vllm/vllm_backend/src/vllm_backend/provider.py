"""VllmProvider — raw-text streaming chat, DIRECTLY against a vLLM server.

Tool-calling is owned by ai_core (it parses `<call>` text out of this stream);
this provider only streams the model's text. It talks to vLLM's own
OpenAI-compatible surface — `POST {endpoint}/chat/completions` — with no proxy
and no protocol translation in between.

What is vLLM-specific here, and why each piece exists:

- THE THOUGHT CHANNEL. With `--reasoning-parser qwen3` the thought streams as
  `delta.reasoning_content` (some versions: `delta.reasoning`) and only the
  answer as `delta.content`. Reading `content` alone drops whole turns.
- A DEAD TURN — all thinking, no answer — is the worst outcome in an agent
  loop. `thinking_token_budget` is vLLM's hard stop for it: once spent, vLLM
  injects the end-of-reasoning token and the model continues into the answer.
  It must be BELOW `max_tokens` or it never fires.
- ONE system message, at index 0 — Qwen's chat template raises "System message
  must be at the beginning" otherwise.
- A stream that ends with NO `finish_reason` means vLLM died mid-answer; that
  is an error, never a completion.
- The KV-cache SESSION TABLE (`SessionTable`): vLLM's prefix cache makes a warm
  conversation answer ~6.5× faster (318 ms vs 2056 ms time-to-first-token on a
  ~16k prompt); past the server's KV capacity it preempts and a preempted
  conversation re-prefills from scratch. The table keeps the number of LIVE
  conversations under that ceiling and rotates the least-recently-used one out.
- THE KEY, when the door needs one (an HTTPS door in front of a shared GPU takes
  `Authorization: Bearer <key>`). Read before EACH request
  through `key` — the record's `credentials` ref in a credential store
  (`ai_core/credentials.py`), never the environment or the record; a record with
  no `credentials` sends no header at all, so a local vLLM started without
  `--api-key` keeps working. A 401 or 403 says to name `credentials`.
"""

from __future__ import annotations

import hashlib
import json
import logging
import time
from collections import OrderedDict
from collections.abc import AsyncIterator

import httpx
from ai_core.core import THINK_CLOSE, THINK_OPEN
from ai_core.provider import ProviderError

log = logging.getLogger(__name__)

# ─── sampling ────────────────────────────────────────────────────────────────
#
# Applied ONLY where the record sets nothing (a production deployment's agentic
# defaults). Each is a record field; a value set
# on the record is sent verbatim.
SAMPLING_DEFAULTS: dict = {
    "temperature": 0.2,
    "top_p": 0.8,
    "top_k": 20,
    # `reasoning_effort` is a CHAT-TEMPLATE input (a soft steer on how much the
    # model wants to think); xhigh can spend the whole bound thinking and leave no
    # answer, medium and low leave room for one.
    "reasoning_effort": "medium",
    # the HARD limit (see the module docstring); medium thinks ~900 tokens on a
    # normal agent turn, so this only binds on a runaway.
    "thinking_token_budget": 16000,
}
# Below this, NO thinking budget is sent: a handful of tokens of thinking is no
# thinking, and a model starved of it stops with an empty answer.
BUDGET_FLOOR = 256
# vLLM accepts exactly these; "high" answers 400, so it is refused at write.
REASONING_EFFORTS = ("low", "medium", "xhigh")
# Sent only when set on the record — never defaulted. Penalties especially:
# presence_penalty 0.7 caused looping and frequency_penalty collapsed output
# into sub-word garbage at ~31k chars; both 0 stayed clean to ~87k chars.
PASSTHROUGH = (
    "min_p",
    "presence_penalty",
    "frequency_penalty",
    "repetition_penalty",
    "no_repeat_ngram_size",
    "seed",
    "stop",
)

# ─── connection ──────────────────────────────────────────────────────────────
#
# TWO timeouts on purpose. A stopped instance drops SYNs silently, so one long
# timeout would hang a caller for the whole read deadline; connect fails fast.
# Read is long because a max-length generation decodes for many minutes (a
# clean run reached ~24k output tokens). Both are that deployment's values; each
# is a record field.
CONNECT_TIMEOUT = 5.0
READ_TIMEOUT = 900.0

# ─── the KV-cache session table ──────────────────────────────────────────────
#
# Seconds a conversation may sit idle before its slot frees itself (the deployment's
# value; a record field).
KV_TTL = 1800.0
KV_OVERFLOWS = ("evict", "reject")

#: How a server names the thinking budget (#207): vllm sends
#: `thinking_token_budget`, strata `reasoning_budget_tokens`, llamacpp takes
#: no per-request budget (a server flag).
DIALECTS = ("vllm", "strata", "llamacpp")


#: Tokens a chat template adds around one message (role markers, separators) —
#: a generous bound (Qwen's ChatML adds ~5).
MESSAGE_OVERHEAD = 16


def prompt_bound(messages: list[dict]) -> int:
    """An UPPER bound on the prompt's tokens: every byte of each message's role and
    content (a byte-level BPE token is at least one byte) plus `MESSAGE_OVERHEAD`
    per message. Never below the server's count, so a turn it lets keep its
    `max_tokens` fits the window."""
    total = 0
    for m in messages:
        c = m.get("content")
        text = (
            c
            if isinstance(c, str)
            else json.dumps(c, ensure_ascii=False, separators=(",", ":"))
        )
        total += (
            len(str(m.get("role", "")).encode()) + len(text.encode()) + MESSAGE_OVERHEAD
        )
    return total


class KvSlotsFull(RuntimeError):
    """`kv_overflow: reject` and every slot on the endpoint is live."""


class SessionTable:
    """The LIVE conversations on ONE vLLM endpoint, least-recently-used first.

    Shared by every vllm_backend agent in this kernel that names the same
    endpoint (one table per endpoint, the closest a kernel
    gets to "one count per GPU"). It is BOOKKEEPING, not memory management:
    vLLM owns its prefix cache (content-addressed blocks, its own LRU) and the
    table frees no GPU memory — a conversation rotated out here that comes back
    still hits whatever blocks vLLM kept.
    What the table does is keep the number of conversations this kernel
    drives at once under the server's KV ceiling, so vLLM never has to preempt
    one of ours. It sees only THIS kernel's traffic — anything else on the GPU
    is invisible to it — so it is a local policy under the real ceiling.

    Each admission uses the ADMITTING agent's own `kv_slots`/`kv_overflow`/
    `kv_ttl` against the shared live set.
    """

    def __init__(self) -> None:
        self.live: OrderedDict[str, float] = OrderedDict()  # key -> last use
        self.evicted = 0
        self.rejected = 0

    def admit(
        self, key: str, slots: int, overflow: str, ttl: float, now: float
    ) -> str | None:
        """Mark `key` live and most-recently used. Returns the key ROTATED OUT
        to make room (`evict`), or None; raises `KvSlotsFull` (`reject`)."""
        for k, last in list(self.live.items()):
            if now - last > ttl:
                del self.live[k]  # idle past its TTL: the slot frees itself
        if key in self.live:
            self.live.move_to_end(key)
            self.live[key] = now
            return None
        rotated = None
        if len(self.live) >= slots:
            if overflow == "reject":
                self.rejected += 1
                # the soonest a slot frees on its own: the idlest one's TTL
                retry_after = max(0.0, ttl - (now - next(iter(self.live.values()))))
                raise KvSlotsFull(
                    f"vllm_backend: all {slots} KV slots on this endpoint are live "
                    f"(kv_overflow=reject) — retry in ~{retry_after:.0f}s, or set "
                    "kv_overflow=evict to rotate the least-recently-used out"
                )
            rotated, _ = self.live.popitem(last=False)
            self.evicted += 1
        self.live[key] = now
        return rotated

    def view(self) -> dict:
        return {
            "live": len(self.live),
            "conversations": list(self.live),  # least-recently used first
            "evicted": self.evicted,
            "rejected": self.rejected,
        }


# endpoint (normalised) -> its table. Module state, keyed like every other
# per-kernel dict in these bundles (one kernel per process).
_tables: dict[str, SessionTable] = {}


def table_for(endpoint: str) -> SessionTable:
    return _tables.setdefault(endpoint.rstrip("/"), SessionTable())


# agent id -> the `thinking_token_budget` its last pass actually SENT (None: no
# budget). Reflect reports it; the provider is built per turn, so it lives here,
# beside the tables — dropped by the bundle's `on_delete`.
last_budgets: dict[str, int | None] = {}

# agent id -> its token counts since the kernel started: requests, prompt_tokens,
# output_tokens. Reflect and the `usage` verb report it; on_delete drops it.
usage: dict[str, dict] = {}


def _usage_of(agent_id: str) -> dict:
    return usage.setdefault(
        agent_id, {"requests": 0, "prompt_tokens": 0, "output_tokens": 0}
    )


def conversation_key(messages: list[dict]) -> str:
    """A conversation's identity: its system text + first user turn.

    That is the PREFIX vLLM caches, and it is stable across turns because the
    history is append-only. When compaction rewrites it, the key changes — and
    so has the prefix, so the old cache no longer applies: counting it as a new
    conversation is the truth, and the stale one ages out of the table."""
    system = "\n\n".join(
        str(m.get("content", "")) for m in messages if m.get("role") == "system"
    )
    first_user = next(
        (str(m.get("content", "")) for m in messages if m.get("role") == "user"), ""
    )
    digest = hashlib.sha256(f"{system}\x00{first_user}".encode()).hexdigest()
    return digest[:16]


def one_system_message(messages: list[dict]) -> list[dict]:
    """Every system message merged into ONE at index 0 (Qwen's template refuses
    a system message anywhere else); the other turns keep their order."""
    systems = [str(m.get("content", "")) for m in messages if m.get("role") == "system"]
    rest = [m for m in messages if m.get("role") != "system"]
    if not systems:
        return rest
    return [{"role": "system", "content": "\n\n".join(systems)}, *rest]


class VllmProvider:
    #: The prompt size vLLM REPORTED for the last pass; ai_core calibrates with it.
    last_prompt_tokens: int | None = None
    #: The output tokens the server reported for the last pass (None = not reported).
    last_completion_tokens: int | None = None
    last_stop_reason: str | None = None

    def __init__(
        self,
        *,
        agent_id: str,
        endpoint: str,
        model: str,
        key,
        max_tokens: int,
        window: int | None = None,
        kv_slots: int,
        kv_overflow: str,
        kv_ttl: float = KV_TTL,
        dialect: str = "vllm",
        sampling: dict | None = None,
        explicit_budget: bool = False,
        connect_timeout: float = CONNECT_TIMEOUT,
        read_timeout: float = READ_TIMEOUT,
        transport: httpx.AsyncBaseTransport | None = None,
        clock=time.monotonic,
    ):
        # whose `last_budgets` entry a pass writes — surfaced in reflect, so a
        # caller debugging a cut can see what the adapted default became
        self._agent_id = agent_id
        self._endpoint = endpoint.rstrip("/")
        self._model = model
        # `async key(endpoint) -> str | None`, awaited before EACH request; None =
        # no `credentials` on the record, no header.
        self._key = key
        self._max_tokens = int(max_tokens)
        # The record's `context_window`. vLLM counts the prompt AND the output
        # against one window, so a cap the prompt leaves no room for is refused
        # (HTTP 400). A turn whose `max_tokens` could overrun it — the prompt's
        # token BOUND plus `max_tokens` past the window — sends NO max_tokens:
        # vLLM then gives it exactly what the prompt leaves (a max_tokens near the
        # window size is refused once any prompt is added).
        self._window = int(window) if window else None
        self._kv = (int(kv_slots), kv_overflow, float(kv_ttl))
        self._dialect = dialect
        # The effective sampling: the record's values over SAMPLING_DEFAULTS,
        # plus any PASSTHROUGH field the record set.
        self._sampling = dict(sampling or {})
        # Whether `thinking_token_budget` came from the RECORD (the caller's
        # statement — never touched) or from the default (ours to fit per call).
        self._explicit_budget = explicit_budget
        self._timeouts = (float(connect_timeout), float(read_timeout))
        self._transport = transport
        self._clock = clock

    def _budget_for(self, call_max: int, *, side_call: bool) -> int | None:
        """The `thinking_token_budget` to send on a call bounded by `call_max`:

        - a MAIN turn with an EXPLICIT budget sends it as set — the write keeps
          it below the record's `max_tokens`, so it can fire;
        - otherwise (the DEFAULT on a main turn, or any budget on a bounded
          SIDE CALL such as a fold summary) it is `min(budget, call_max // 2)`,
          so at least half of the call's output is left for the answer (a rule
          of thumb, not tuned) — an explicit 8000 on a ~2000-token fold summary
          becomes 1000 instead of refusing the fold;
        - below `BUDGET_FLOOR` NO budget is sent at all: a few tokens of
          thinking is no thinking — a tiny budget makes the model stop with
          thinking only and no answer (a dead turn)."""
        budget = self._sampling.get("thinking_token_budget")
        if budget is None:
            return None
        budget = int(budget)
        if self._explicit_budget and not side_call:
            return budget
        adapted = min(budget, call_max // 2)
        if adapted < BUDGET_FLOOR:
            log.info(
                "vllm_backend: no thinking_token_budget for a call bounded by "
                "max_tokens=%d (half of it, %d, is under the %d floor)",
                call_max,
                adapted,
                BUDGET_FLOOR,
            )
            return None
        if adapted != budget:
            log.info(
                "vllm_backend: thinking_token_budget %d adapted to %d for a call "
                "bounded by max_tokens=%d",
                budget,
                adapted,
                call_max,
            )
        return adapted

    def _body(self, messages: list[dict], call_max: int, *, side_call: bool) -> dict:
        body: dict = {
            "model": self._model,
            "messages": one_system_message(messages),
            "max_tokens": call_max,
            "stream": True,
            # the final chunk carries usage (and an EMPTY choices array)
            "stream_options": {"include_usage": True},
        }
        if (
            not side_call
            and self._window is not None
            and call_max + prompt_bound(body["messages"]) > self._window
        ):
            del body["max_tokens"]  # the rest of the window — see `_window`
        for key in ("temperature", "top_p", "top_k", "reasoning_effort", *PASSTHROUGH):
            if self._sampling.get(key) is not None:
                body[key] = self._sampling[key]
        budget = self._budget_for(call_max, side_call=side_call)
        last_budgets[self._agent_id] = budget
        if budget is not None:
            # the dialect's own name for the budget (#207); llamacpp takes none
            if self._dialect == "strata":
                body["reasoning_budget_tokens"] = budget
            elif self._dialect != "llamacpp":
                body["thinking_token_budget"] = budget
        return body

    async def chat(
        self, messages: list[dict], max_tokens: int | None = None
    ) -> AsyncIterator[str]:
        """Stream raw text — NO tools array. ai_core parses any `<call>` element
        out of the text. `max_tokens` bounds a side call (a fold summary);
        absent, the record's `max_tokens` bounds the turn."""
        call_max = int(max_tokens) if max_tokens is not None else self._max_tokens
        body = self._body(messages, call_max, side_call=max_tokens is not None)
        slots, overflow, ttl = self._kv
        # A side call is one request, not a conversation kept warm: it takes no slot, so folds cannot fill the table (#184).
        rotated = None
        if max_tokens is None:
            # Admission BEFORE the request: a `reject` must not reach the GPU.
            rotated = table_for(self._endpoint).admit(
                conversation_key(messages), slots, overflow, ttl, self._clock()
            )
        if rotated is not None:
            log.info("vllm_backend: KV slot rotated — conversation %s out", rotated)

        thinking = False
        saw_finish = False
        tool_calls: list[list[str]] = []
        key = await self._key(self._endpoint)
        headers = {"Authorization": f"Bearer {key}"} if key else {}
        connect, read = self._timeouts
        try:
            # ONE client per call: the provider is built per turn, and a client
            # kept on it would hold its sockets past the turn. After connect, one
            # socket deadline covers reads AND writes.
            async with (
                httpx.AsyncClient(
                    base_url=self._endpoint,
                    timeout=httpx.Timeout(read, connect=connect),
                    transport=self._transport,
                ) as client,
                client.stream(
                    "POST", "/chat/completions", json=body, headers=headers
                ) as resp,
            ):
                if resp.status_code >= 400:
                    detail = (await resp.aread()).decode("utf-8", "replace")[:500]
                    why = (
                        " — the door wants a key: name it in `credentials` "
                        "({store, ref})"
                        if resp.status_code in (401, 403)
                        else ""
                    )
                    raise ProviderError(
                        f"vllm_backend: {self._endpoint} answered HTTP "
                        f"{resp.status_code}{why}: {detail}",
                        transport=resp.status_code in (502, 503, 504),
                    )
                async for line in resp.aiter_lines():
                    # `aiter_lines` reassembles lines a read split mid-JSON;
                    # anything that is not a `data:` line (a blank separator, a
                    # `:` keep-alive) carries nothing.
                    if not line.startswith("data:"):
                        continue
                    data = line[5:].strip()
                    if data == "[DONE]":
                        break
                    if not data:
                        continue
                    obj = json.loads(data)
                    usage = obj.get("usage")
                    if isinstance(usage, dict) and usage.get("prompt_tokens"):
                        self.last_prompt_tokens = int(usage["prompt_tokens"])
                    if isinstance(usage, dict) and usage.get("completion_tokens"):
                        self.last_completion_tokens = int(usage["completion_tokens"])
                    if isinstance(usage, dict):
                        u = _usage_of(self._agent_id)
                        u["requests"] += 1
                        u["prompt_tokens"] += int(usage.get("prompt_tokens") or 0)
                        u["output_tokens"] += int(usage.get("completion_tokens") or 0)
                    choices = obj.get("choices") or []
                    if not choices:
                        continue  # the usage-only final chunk
                    choice = choices[0]
                    if choice.get("finish_reason"):
                        saw_finish = True
                        fr = choice["finish_reason"]
                        # a `length` cut is surfaced as a cut, so the caller
                        # can continue — never read as a finished answer
                        self.last_stop_reason = "max_tokens" if fr == "length" else fr
                    delta = choice.get("delta") or {}
                    think = delta.get("reasoning_content") or delta.get("reasoning")
                    if isinstance(think, str) and think:
                        if not thinking:
                            thinking = True
                            yield THINK_OPEN
                        yield think
                    content = delta.get("content")
                    if isinstance(content, str) and content:
                        if thinking:
                            thinking = False
                            yield THINK_CLOSE
                        yield content
                    for piece in delta.get("tool_calls") or []:
                        i = max(0, int(piece.get("index") or 0))
                        while len(tool_calls) <= i:
                            tool_calls.append(["", ""])
                        fn = piece.get("function") or {}
                        if isinstance(fn.get("name"), str):
                            tool_calls[i][0] += fn["name"]
                        if isinstance(fn.get("arguments"), str):
                            tool_calls[i][1] += fn["arguments"]
        except httpx.ConnectTimeout as e:
            raise ProviderError(
                f"vllm_backend: no answer from {self._endpoint} within "
                f"connect_timeout={self._timeouts[0]:g}s — the instance is STOPPED "
                "(its SYNs are dropped), or the tunnel/port is wrong",
                transport=True,
            ) from e
        except httpx.ConnectError as e:
            raise ProviderError(
                f"vllm_backend: {self._endpoint} refused the connection — the host is "
                "up but vLLM is not listening yet (it loads for ~75 s after boot), or "
                "the tunnel in front of it is down",
                transport=True,
            ) from e
        except httpx.ReadTimeout as e:
            raise ProviderError(
                f"vllm_backend: {self._endpoint} sent nothing for "
                f"read_timeout={self._timeouts[1]:g}s",
                transport=True,
            ) from e
        except (httpx.ReadError, httpx.RemoteProtocolError) as e:
            raise ProviderError(
                f"vllm_backend: {self._endpoint} dropped the stream mid-answer — "
                "vLLM died or restarted; this turn is NOT complete",
                transport=True,
            ) from e
        if thinking:
            yield THINK_CLOSE
        if tool_calls:
            rendered = []
            for name, args in tool_calls:
                try:
                    params = json.loads(args or "{}")
                except json.JSONDecodeError:
                    params = None
                if not isinstance(params, dict):
                    raise RuntimeError(
                        f"vllm_backend: the server sent a native tool call '{name}' "
                        f"whose arguments are not a JSON object: {args}"
                    )
                parts = []
                for k, v in params.items():
                    text = v if isinstance(v, str) else json.dumps(v)
                    parts.append(f"<{k}>{text}</{k}>")
                rendered.append(f'<call name="{name}">' + "".join(parts) + "</call>")
            yield "\n".join(rendered)
        if not saw_finish:
            raise ProviderError(
                f"vllm_backend: the stream from {self._endpoint} ended without a "
                "finish_reason — vLLM died mid-answer; this turn is NOT complete",
                transport=True,
            )
        # A CUT: a BOUNDED SIDE CALL (a fold summary, which passed its own
        # `max_tokens`) that stopped at its bound got what it asked for — an
        # answer; its caller reads `last_stop_reason`. A MAIN turn that hit the
        # record's `max_tokens` did not finish, and returned as if it had, it
        # parses as a truncated envelope (or a turn of pure thinking) and the chain dies downstream with the cause hidden — so it
        # fails, naming the cut. Same contract as every other backend.
        if max_tokens is None and self.last_stop_reason == "max_tokens":
            raise RuntimeError(
                f"vllm_backend: the turn was cut at max_tokens={call_max} "
                "(finish_reason=length) — it did not finish; raise max_tokens on "
                "the record, or lower thinking_token_budget / reasoning_effort"
            )

    @property
    def model(self) -> str:
        return self._model
