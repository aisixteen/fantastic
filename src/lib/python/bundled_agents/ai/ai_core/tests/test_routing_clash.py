"""A ROUTING NAME A VERB ALSO TAKES. `title`, `timeout`, `reply_to` … on a call are
routing — lifted out, never sent to the target. A verb whose `docs` names an argument
the same way could never receive it, and the model would blame the target. The call is
refused, loud, naming the clash; a verb without such an argument is untouched."""

from __future__ import annotations

import asyncio
import types

from _testkit import memory_meta

from ai_core.tool_parse import render_call
from kernel import register_unlisted

DONE = '<call name="done"></call>'
_GOT: list[dict] = []


def _store_module() -> types.ModuleType:
    async def _create(id, payload, kernel):
        """args: title:str (req), body:str (req). A page → {created}."""
        _GOT.append(payload)
        return {"created": payload.get("title")}

    async def _ping(id, payload, kernel):
        """No args. → {pong}."""
        _GOT.append(payload)
        return {"pong": True}

    mod = types.ModuleType("clash_store")
    mod.VERBS = {"create": _create, "ping": _ping}

    async def handler(id, payload, kernel):
        fn = mod.VERBS.get(payload.get("type"))
        if fn is None:
            return None
        return await fn(id, payload, kernel)

    mod.handler = handler
    return mod


class _Fake:
    def __init__(self, scripts):
        self._scripts = list(scripts)
        self.requests: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(list(messages))
        yield self._scripts.pop(0) if self._scripts else DONE


async def test_a_routing_name_the_verb_documents_is_refused_loud(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    register_unlisted("clash_store", _store_module())
    k.create("clash_store", id="kb", callable_by={"rc_ai": "*"})
    meta = memory_meta(k, "rc_ai", {"cli": "*", "kb": "*", "kernel_state": ["docs"]})
    k.create(
        "ollama_backend.tools",
        id="rc_ai",
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        context_window=32768,
        **meta,
    )
    create = render_call(
        "send",
        {
            "target_id": "kb",
            "payload": {"type": "create", "body": "b"},
            "title": "Notes",
        },
    )
    ping = render_call(
        "send", {"target_id": "kb", "payload": {"type": "ping"}, "title": "check kb"}
    )
    fp = fake_providers["rc_ai"] = _Fake([create + ping, DONE])
    _GOT.clear()
    await k.send([{"target": "rc_ai", "payload": {"type": "send", "text": "go"}}])
    for _ in range(100):
        if len(fp.requests) >= 2:
            break
        await asyncio.sleep(0.02)
    turn2 = fp.requests[1][-1]["content"]
    assert "title on this call is call routing" in turn2, turn2
    assert "documents an argument named title" in turn2, turn2
    assert [p.get("type") for p in _GOT] == ["ping"], "create must not run; ping must"
