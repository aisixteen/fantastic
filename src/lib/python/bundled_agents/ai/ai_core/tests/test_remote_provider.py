"""The split: an AI chat and its provider are two agents (twin of the Rust
tests_remote_provider.rs and the chat/provider tests of tests.rs).

A provider answers `complete` (one stateless pass, tokens on its stream tagged with
the caller's `rid`); an `ai_chat.tools` agent holds the conversation and reaches its
provider only by id (`provider_id`), through `RemoteProvider`."""

from __future__ import annotations

import asyncio
import types

import pytest
from _testkit import as_agent, memory_meta

from ai_core import ProviderError, build
from ai_core.remote_provider import (
    CHAT_VERBS,
    COMPLETE_DOC,
    MODELS_DOC,
    PROVIDER_VERBS,
    SET_DEFAULT_DOC,
    RemoteProvider,
    provider_verbs,
)
from kernel import register_unlisted


class _Words:
    """A provider: streams the words it is given, reports a model."""

    def __init__(self, words, model="echo-1"):
        self.words = words
        self.model = model

    async def chat(self, messages, max_tokens=None):
        for w in self.words:
            yield w


def _echo(build_provider) -> types.ModuleType:
    async def handler(id, payload, kernel):
        return await provider_verbs(id, payload, kernel, build_provider)

    mod = types.ModuleType("echo_mod")
    mod.handler = handler
    mod.VERBS = {}
    return mod


async def _call(k, to, payload):
    return (await k.send([{"target": to, "payload": payload}]))[0]["result"]


async def test_a_chat_completes_through_its_provider_agent_by_id(seeded_kernel):
    k = seeded_kernel
    register_unlisted("echo.tools", _echo(lambda rec: _Words(["hel", "lo"])))
    k.create("echo.tools", id="p", callable_by={"chat": "*"})
    k.create("echo.tools", id="chat", can_call={"p": ["complete", "watch"]})
    rp = RemoteProvider(k, "chat", "p")
    with as_agent(k, "chat"):
        toks = [t async for t in rp.chat([{"role": "user", "content": "hi"}])]
    assert "".join(toks) == "hello", "the provider's tokens, in order"
    assert rp.model == "echo-1", "the model the provider reported"


async def test_a_chat_without_the_grant_is_refused_by_name(seeded_kernel):
    k = seeded_kernel
    register_unlisted("echo.tools", _echo(lambda rec: _Words(["x"])))
    k.create("echo.tools", id="p")
    k.create("echo.tools", id="chat")
    rp = RemoteProvider(k, "chat", "p")
    with as_agent(k, "chat"):
        with pytest.raises(ProviderError, match="cannot watch provider `p`"):
            async for _ in rp.chat([{"role": "user", "content": "hi"}]):
                pass


class _Echo:
    def __init__(self, rec):
        self.rec = rec
        self.model = rec.get("model") or "srv-1"

    async def chat(self, messages, max_tokens=None):
        yield "hel"
        yield "lo"
        yield f" from {self.rec.get('model') or 'srv-1'}"


def _provider(k, id, default=None, **meta):
    register_unlisted("echo_provider.tools", _echo(lambda rec: _Echo(rec)))
    rec = k.create(
        "echo_provider.tools", id=id, **meta, **memory_meta(k, id, {"cli": "*"})
    )
    assert rec.get("id") == id, rec
    return rec


async def test_an_ai_chat_holds_the_conversation_and_completes_through_its_provider(
    seeded_kernel,
):
    k = seeded_kernel
    _provider(
        k, "prov", callable_by={"chat1": "*", "$self": "*"}, models=["m-small", "m-big"]
    )
    r = await _call(k, "prov", {"type": "set_default_model", "model": "m-small"})
    assert r == {"default": "m-small"}, r
    rec = k.create(
        "ai_chat.tools",
        id="chat1",
        provider_id="prov",
        context_window=32768,
        **memory_meta(k, "chat1", {"cli": "*", "prov": ["complete", "watch"]}),
    )
    assert rec.get("id") == "chat1", rec
    r = await _call(k, "chat1", {"type": "send", "text": "hi"})
    assert r["response"] == "hello from m-small", (
        f"no model on the chat: the provider's default: {r}"
    )
    assert "model" not in k.get("chat1"), "the chat's record holds no provider field"
    rows = await _call(k, "chat1", {"type": "rows"})
    text = str(rows)
    assert "hi" in text and "hello" in text, f"the chat holds both sides: {rows}"
    refl = await _call(k, "chat1", {"type": "reflect"})
    assert refl["provider_id"] == "prov", refl

    # the chat names its own model: the next pass runs on it, the conversation stays
    k.update("chat1", model="m-big")
    r = await _call(k, "chat1", {"type": "send", "text": "again"})
    assert r["response"] == "hello from m-big", r
    assert "hi" in str(await _call(k, "chat1", {"type": "rows"}))


async def test_a_provider_completes_on_its_default_model_field_and_lists_and_sets_it(
    seeded_kernel,
):
    k = seeded_kernel
    _provider(k, "prov", models=["m-small", "m-big"])
    await _call(k, "prov", {"type": "set_default_model", "model": "m-small"})
    _provider(k, "bare")
    msg = [{"role": "user", "content": "hi"}]
    r = await _call(k, "prov", {"type": "complete", "messages": msg, "rid": "r1"})
    assert r["model"] == "m-small", f"no model named: the record's default: {r}"
    r = await _call(
        k, "prov", {"type": "complete", "messages": msg, "rid": "r2", "model": "m-big"}
    )
    assert r["model"] == "m-big", r
    r = await _call(k, "bare", {"type": "complete", "messages": msg, "rid": "r3"})
    assert r["model"] == "srv-1", f"no user model, no default: the server's: {r}"
    assert (await _call(k, "bare", {"type": "models"}))["default"] == "srv-1", (
        "which is the default from now on"
    )
    assert await _call(k, "prov", {"type": "models"}) == {
        "models": ["m-small", "m-big"],
        "default": "m-small",
    }
    assert await _call(k, "prov", {"type": "set_default_model", "model": "m-big"}) == {
        "default": "m-big"
    }
    assert (await _call(k, "prov", {"type": "models"}))["default"] == "m-big"
    no = await _call(k, "prov", {"type": "set_default_model", "model": "m-huge"})
    assert no["reason"] == "bad_arg", f"not one of its models: {no}"
    r = await _call(k, "prov", {"type": "complete", "messages": msg, "rid": "r4"})
    assert r["model"] == "m-big", f"the new default: {r}"


async def test_a_provider_refuses_a_chat_verb_naming_ai_chat(seeded_kernel):
    k = seeded_kernel
    k.create(
        "ollama_backend.tools",
        id="olla",
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        **memory_meta(k, "olla", {"cli": "*"}),
    )
    r = await _call(k, "olla", {"type": "send", "text": "hi"})
    assert r["reason"] == "unhandled" and r["verb"] == "send", r
    assert "ai_chat.tools" in r["error"] and "provider_id" in r["error"], r
    assert "error" not in await _call(k, "olla", {"type": "reflect"})


@pytest.mark.parametrize("verb", PROVIDER_VERBS)
async def test_ai_chat_refuses_a_provider_verb(seeded_kernel, verb):
    k = seeded_kernel
    k.create(
        "ai_chat.tools",
        id="c",
        provider_id="nowhere",
        **memory_meta(k, "c", {"cli": "*"}),
    )
    r = await _call(k, "c", {"type": verb})
    assert r["reason"] == "unhandled", r
    assert r["error"].startswith(f"ai_chat: `{verb}` is a provider's verb; "), r


async def test_the_docs_split(seeded_kernel):
    k = seeded_kernel
    chat = await _call(k, "kernel", {"type": "docs", "of": "ai_chat.tools"})
    verbs = chat["verbs"]
    assert "send" in verbs and not set(PROVIDER_VERBS) & set(verbs), list(verbs)
    prov = (
        await _call(
            k,
            "kernel",
            {"type": "docs", "of": "ollama_backend.tools", "tier": "detailed"},
        )
    )["verbs"]
    assert list(prov)[:4] == ["reflect", "complete", "models", "set_default_model"]
    for verb, doc in zip(
        PROVIDER_VERBS, (COMPLETE_DOC, MODELS_DOC, SET_DEFAULT_DOC), strict=True
    ):
        assert doc in str(prov[verb]), (verb, prov[verb])


class _Hang:
    model = "hang-1"

    async def chat(self, messages, max_tokens=None):
        yield "a"
        await asyncio.sleep(30)


async def test_a_cancelled_pass_releases_its_reader(seeded_kernel):
    k = seeded_kernel
    register_unlisted("echo.tools", _echo(lambda rec: _Hang()))
    k.create("echo.tools", id="p", callable_by={"chat": "*"})
    k.create("echo.tools", id="chat", can_call={"p": ["complete", "watch"]})
    rp = RemoteProvider(k, "chat", "p")
    with as_agent(k, "chat"):
        gen = rp.chat([{"role": "user", "content": "hi"}])
        assert await gen.__anext__() == "a"
        await gen.aclose()
    assert not [i for i in k.ctx.inboxes if i.startswith("_provider_reader_chat_")]
    assert not k.ctx.agents["p"]._watcher_ids
    for _ in range(100):  # the provider's dispatch ends, not left for 30 s
        if not k.ctx.agents["p"]._in_flight:
            break
        await asyncio.sleep(0.02)
    assert not k.ctx.agents["p"]._in_flight


def test_chat_verbs_are_the_core_table_minus_the_providers_own():
    table = build(sentence="s", make_provider=lambda *a: None, name="x")[0]
    assert set(table) - {"reflect", "boot", *PROVIDER_VERBS} == CHAT_VERBS
