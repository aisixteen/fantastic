"""THE SPLIT: an AI CHAT and its PROVIDER are two agents (twin of remote_provider.rs).

- A PROVIDER (any `*_backend` bundle) answers `complete`: one stateless pass over the
  messages it is handed — no memory, no calls, no conversation. Its tokens go out as
  `provider_token` events on its own stream, tagged with the caller's `rid`; the reply
  carries the report (model, prompt / completion tokens, stop reason). See `complete`.
- An AI CHAT (`ai_chat.tools`) holds the conversation: the turn loop, the `<call>`
  parse and route, the memory, the folds. It knows its provider only by id
  (`provider_id`, a local id or a peer's `<guid>|<id>`), through `RemoteProvider`: it
  watches the provider, sends `complete`, and reads the tokens tagged with its `rid`.

The chat's grants name the provider: `can_call` `{<provider>: [complete, watch]}`, and
the provider's `callable_by` the chat.
"""

from __future__ import annotations

import asyncio
import contextlib
import inspect
import itertools
import json

from ai_core.provider import ProviderError
from kernel._refusal import UNHANDLED
from kernel._refusal import refusal as _refusal

#: The event type a provider's tokens carry on its stream.
TOKEN_EVENT = "provider_token"

#: The record field naming a chat's provider.
PROVIDER_FIELD = "provider_id"

#: The name of a provider's default-model setting (in its memory store).
DEFAULT_FIELD = "default_model"

#: A provider's settings live in its memory store as `setting` rows.
_SETTING = "setting"

#: The handler module of the chat bundle.
CHAT_HANDLER = "ai_chat.tools"

COMPLETE_DOC = (
    "args: messages:list (req — role + text content, as ai_core assembles them), "
    "model:str? (the model of this pass — the calling chat's own; absent: this provider's default_model), "
    "max_tokens:int? (this call's output bound; absent: the record's), rid:str (req — tags every token event). "
    "ONE stateless pass: no memory, no calls, no conversation kept. Each content token is emitted on this agent's "
    "stream as {type:'provider_token', rid, text} (a watcher reads them); the reply is {rid, model, prompt_tokens, "
    "completion_tokens, stop_reason} or {rid, error, transport}. An AI chat (`ai_chat.tools`) calls it through its "
    "`provider_id`."
)

MODELS_DOC = (
    "No args. The models this provider serves and its default: {models:[name, …], default}. "
    "The list is the record's `models`; a provider that can ask its API lists those when the record names none."
)

SET_DEFAULT_DOC = (
    "args: model:str (req). Sets this provider's default model (one of `models` when the list "
    "is known, else refused); a chat that names no `model` completes on it. Returns {default}."
)

#: The verbs a provider answers (a chat refuses them).
PROVIDER_VERBS = ("complete", "models", "set_default_model")

#: The chat's verbs — every verb of ai_core's table but `reflect`, `boot` and the provider's.
CHAT_VERBS = frozenset(
    {
        "send",
        "stop",
        "pause",
        "resume",
        "rows",
        "inquiry",
        "history",
        "embed",
        "unembed",
        "status",
        "context_status",
        "recall",
        "grep",
        "context_recall",
        "context_clear",
        "context_fold",
        "context_forget",
        "fork",
        "mark",
        "fork_points",
        "watch",
        "unwatch",
    }
)

_counter = itertools.count(1)


def listed_models(rec: dict) -> list[str]:
    """The names a provider's record lists in `models` (strings, or `{id|name}` objects)."""
    models = rec.get("models")
    if not isinstance(models, list):
        return []
    out: list[str] = []
    for m in models:
        if isinstance(m, str):
            out.append(m)
        elif isinstance(m, dict):
            v = m.get("id") if m.get("id") is not None else m.get("name")
            if isinstance(v, str):
                out.append(v)
    return out


def _store_of(id, kernel) -> str | None:
    s = (kernel.get(id) or {}).get("memory_store_id")
    return s if isinstance(s, str) and s else None


async def default_of(id, kernel) -> str | None:
    """A provider's default model: the newest `default_model` setting in its store."""
    store = _store_of(id, kernel)
    if store is None:
        return None
    outs = await kernel.send(
        [
            {
                "target": store,
                "payload": {
                    "type": "read",
                    "kinds": [_SETTING],
                    "match": {"name": DEFAULT_FIELD},
                    "order": "desc",
                    "limit": 1,
                },
            }
        ]
    )
    res = outs[0].get("result") if outs else None
    rows = res.get("rows") if isinstance(res, dict) else None
    if not isinstance(rows, list) or not rows or not isinstance(rows[0], dict):
        return None
    v = rows[0].get("value")
    return v if isinstance(v, str) and v else None


async def _set_default(id, kernel, model) -> str | None:
    """Keep `model` as the provider's default. None = ok, else the reason."""
    store = _store_of(id, kernel)
    if store is None:
        return "the provider names no memory_store_id to keep its default in"
    outs = await kernel.send(
        [
            {
                "target": store,
                "payload": {
                    "type": "append",
                    "rows": [{"kind": _SETTING, "name": DEFAULT_FIELD, "value": model}],
                },
            }
        ]
    )
    if not outs:
        return "the store did not answer"
    res = outs[0].get("result")
    if isinstance(res, dict) and "error" in res:
        e = res["error"]
        return e if isinstance(e, str) else "the store refused"
    return None


def is_chat_verb(verb) -> bool:
    """A provider answers no chat: `send`, `history`, `fork` … belong to an AI chat."""
    return verb in CHAT_VERBS


def chat_refusal(name, verb) -> dict:
    """The refusal a provider answers a chat verb with."""
    return _refusal(
        UNHANDLED,
        f"{name}: `{verb}` is a chat's verb, and this agent is a provider — it holds no "
        f"conversation. Chat with an `ai_chat.tools` agent whose {PROVIDER_FIELD} names "
        "this one; it completes here (`complete`).",
        verb=verb,
    )


def _failed(rid, e) -> dict:
    return {
        "rid": rid,
        "error": str(e),
        "kind": "ValueError" if isinstance(e, ValueError) else "RuntimeError",
        "transport": bool(getattr(e, "transport", False)),
    }


async def _built(build, *args):
    p = build(*args)
    if inspect.isawaitable(p):
        p = await p
    return p


async def complete(id, payload, kernel, build) -> dict:
    """THE PROVIDER SIDE: run one pass of the provider `build()` makes for `payload`
    and stream its tokens on `id`'s own stream, tagged with the caller's `rid`."""
    rid = payload.get("rid")
    if not isinstance(rid, str) or not rid:
        return {"error": "complete: rid required", "reason": "bad_arg"}
    messages = payload.get("messages")
    if not isinstance(messages, list):
        return {
            "rid": rid,
            "error": "complete: messages must be a list",
            "reason": "bad_arg",
        }
    mt = payload.get("max_tokens")
    max_tokens = mt if isinstance(mt, int) and not isinstance(mt, bool) else None
    try:
        provider = await _built(build)
        for f in ("last_prompt_tokens", "last_completion_tokens", "last_stop_reason"):
            try:
                setattr(provider, f, None)
            except AttributeError:
                pass
        async for tok in provider.chat(messages, max_tokens=max_tokens):
            await kernel.emit(
                id, {"type": TOKEN_EVENT, "rid": rid, "text": tok}, quiet=True
            )
    except Exception as e:  # noqa: BLE001 — a failed pass is the reply, not a raise
        return _failed(rid, e)
    return {
        "rid": rid,
        "model": provider.model,
        "prompt_tokens": getattr(provider, "last_prompt_tokens", None),
        "completion_tokens": getattr(provider, "last_completion_tokens", None),
        "stop_reason": getattr(provider, "last_stop_reason", None),
    }


async def provider_verbs(id, payload, kernel, build, live=None) -> dict | None:
    """THE PROVIDER VERBS every backend serves the same way: `complete` (on the call's
    `model`, else the record's `default_model`), `models` and `set_default_model`.
    `build(rec)` makes the backend's provider from a record; `live` (optional) asks
    the API for its models when the record names none. None: not a provider verb."""
    verb = payload.get("type")
    rec = kernel.get(id) or {}
    if verb == "complete":
        chosen = payload.get("model")
        if not isinstance(chosen, str) or not chosen:
            chosen = await default_of(id, kernel)
        over = {k: v for k, v in rec.items() if k != "model"}
        if chosen:
            over["model"] = chosen
        r = await complete(id, payload, kernel, lambda: build(over))
        if not chosen:
            m = r.get("model")
            if isinstance(m, str) and m:
                await _set_default(id, kernel, m)
            elif r.get("kind") == "ValueError":
                r["error"] = (
                    f"complete: no model — the chat names none, this provider has no "
                    f"{DEFAULT_FIELD}, and its server names none ({r['error']})"
                )
        return r
    if verb == "models":
        names = listed_models(rec)
        if not names and live is not None:
            out = await live()
            if isinstance(out, str):
                return {
                    "error": out,
                    "reason": "unhandled",
                    "default": await default_of(id, kernel),
                }
            names = out
        return {"models": names, "default": await default_of(id, kernel)}
    if verb == "set_default_model":
        m = payload.get("model")
        if not isinstance(m, str) or not m:
            return {
                "error": "set_default_model: model required — a model name",
                "reason": "bad_arg",
            }
        names = listed_models(rec)
        if names and m not in names:
            return {
                "error": f"set_default_model: `{m}` is not one of this provider's models "
                f"{json.dumps(names, ensure_ascii=False)}",
                "reason": "bad_arg",
            }
        e = await _set_default(id, kernel, m)
        if e is not None:
            return {"error": f"set_default_model: {e}", "reason": "unhandled"}
        return {"default": m}
    return None


def chat_provider(id, kernel):
    """THE CHAT SIDE: the provider an AI chat's record names (`provider_id`)."""
    p = (kernel.get(id) or {}).get(PROVIDER_FIELD)
    if isinstance(p, str) and p:
        return RemoteProvider(kernel, id, p)
    raise ValueError(
        f"ai_chat: the record names no {PROVIDER_FIELD} "
        "(the provider agent this chat completes through)"
    )


def _token_of(item, rid) -> str | None:
    if (
        isinstance(item, dict)
        and item.get("type") == TOKEN_EVENT
        and item.get("rid") == rid
    ):
        t = item.get("text")
        return t if isinstance(t, str) else ""
    return None


class RemoteProvider:
    """A provider reached through the kernel: `complete` on the agent `provider`
    (duck-types `ai_core.provider.Provider`)."""

    last_prompt_tokens: int | None = None
    last_completion_tokens: int | None = None
    last_stop_reason: str | None = None

    def __init__(self, kernel, chat, provider):
        m = (kernel.get(chat) or {}).get("model")
        self._kernel = kernel
        self._chat = chat
        self._provider = provider
        self._chat_model = m if isinstance(m, str) and m else None
        self._model = self._chat_model or ""

    @property
    def model(self) -> str:
        return self._model

    def _settle(self, res) -> None:
        err = res.get("error") if isinstance(res, dict) else None
        if isinstance(err, str):
            if res.get("transport") is True:
                raise ProviderError(err, transport=True)
            if res.get("kind") == "ValueError":
                raise ValueError(err)
            raise ProviderError(err)

        def _int(v):
            return v if isinstance(v, int) and not isinstance(v, bool) else None

        self.last_prompt_tokens = _int(res.get("prompt_tokens"))
        self.last_completion_tokens = _int(res.get("completion_tokens"))
        sr = res.get("stop_reason")
        self.last_stop_reason = sr if isinstance(sr, str) else None
        m = res.get("model")
        if isinstance(m, str) and m:
            self._model = m

    async def chat(self, messages, max_tokens=None):
        kernel, chat, provider = self._kernel, self._chat, self._provider
        n = next(_counter)
        rid = f"{chat}~{n}"
        reader = f"_provider_reader_{chat}_{n}"
        q = kernel.ctx.ensure_inbox(reader)
        denial = kernel.watch(provider, reader)
        if denial:
            kernel.ctx.inboxes.pop(reader, None)
            err = denial.get("error")
            raise ProviderError(
                f"ai_chat: cannot watch provider `{provider}`: "
                f"{err if isinstance(err, str) else json.dumps(denial)}"
            )
        payload = {"type": "complete", "messages": list(messages), "rid": rid}
        if self._chat_model:
            payload["model"] = self._chat_model
        if max_tokens is not None:
            payload["max_tokens"] = max_tokens
        # created in the chat's context: the send is the chat's, judged by its grants
        call = asyncio.ensure_future(
            kernel.send([{"target": provider, "payload": payload}])
        )
        get = None
        try:
            while True:
                get = asyncio.ensure_future(q.get())
                await asyncio.wait({call, get}, return_when=asyncio.FIRST_COMPLETED)
                if get.done():
                    item = get.result()
                    get = None
                    tok = _token_of(item, rid)
                    if tok is not None:
                        yield tok
                if call.done():
                    break
            if get is not None:
                get.cancel()
                get = None
            while True:
                try:
                    item = q.get_nowait()
                except asyncio.QueueEmpty:
                    break
                tok = _token_of(item, rid)
                if tok is not None:
                    yield tok
            outs = call.result()
            if not outs:
                raise ProviderError("ai_chat: complete returned nothing")
            o = outs[0]
            res = o.get("result")
            if o.get("status") == "completed" or (
                isinstance(res, dict) and "rid" in res
            ):
                self._settle(res)
            else:
                raise ProviderError(
                    f"ai_chat: complete on `{provider}` ended {o.get('status')}: "
                    f"{json.dumps(res, default=str)}"
                )
        finally:
            if get is not None:
                get.cancel()
            if not call.done():
                call.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await call
            kernel.unwatch(provider, reader)
            kernel.ctx.inboxes.pop(reader, None)


async def pair_chat(kernel, provider) -> str:
    """TESTS AND TOOLS: an `ai_chat` agent completing through `provider` —
    `<provider>_ai`, under the same parent, on the provider's memory store, with both
    ends of the edge granted. Returns the chat's id."""
    chat = f"{provider}_ai"
    ctx = kernel.ctx
    if chat in ctx.agents:
        return chat
    pm = kernel.get(provider) or {}
    parent = pm.get("parent_id") or "kernel"
    can_call = dict(pm.get("can_call") or {})
    can_call[provider] = ["complete", "watch"]
    create = {
        "type": "create_agent",
        "parent_id": parent,
        "handler_module": CHAT_HANDLER,
        "id": chat,
        PROVIDER_FIELD: provider,
        "can_call": can_call,
        "callable_by": pm.get("callable_by") or {"$self": "*"},
    }
    m = pm.get("model")
    if isinstance(m, str) and m:
        create["model"] = m
    for k in (
        "memory_store_id",
        "file_bridge_id",
        "context_window",
        "context_strategy",
        "caching",
    ):
        if k in pm:
            create[k] = pm[k]
    if "context_window" not in create:
        outs = await kernel.send([{"target": provider, "payload": {"type": "reflect"}}])
        res = outs[0].get("result") if outs else None
        w = res.get("context_window") if isinstance(res, dict) else None
        if isinstance(w, (int, float)) and not isinstance(w, bool):
            create["context_window"] = w
    await kernel.send([{"target": "kernel", "payload": create}])

    verbs = ["complete", "watch"]

    def admit(cb: dict) -> bool:
        cb[chat] = list(verbs)
        return True

    ctx.edit_callable_by(provider, admit)
    for aid, agent in list(ctx.agents.items()):
        cb = agent._meta.get("callable_by")
        if isinstance(cb, dict) and provider in cb:
            v = cb[provider]
            ctx.edit_callable_by(
                aid,
                lambda c, v=v: c.__setitem__(chat, v) or True,
            )
    return chat
