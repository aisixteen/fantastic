"""The READ side of the gate: `watch` and the state stream are authorized.

A watcher receives its target's raw inbound payloads VERBATIM — the model
prompt, the document body, whatever was sent. So an ungated `watch` is a read
side-channel around `callable_by`: an agent that may not CALL an agent could
still read everything sent TO it.

Found 2026-08-27 in a dataroom audit: a browser leg granted exactly
`assistant:[send]` could mirror ANY agent's inbox in its session and read the
packed confidential context out of the model's prompt. The `call` path went
through the two-sided gate; `watch` and `state_subscribe` never did.

Same class as the ledger's origin scoping — a verb that returns other agents'
data has to be gated on the data, not merely on reachability.
"""

from __future__ import annotations

import sys
import types

import pytest

from kernel import sender_context

_mod = types.ModuleType("watch_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "watch probe", "verbs": {}}
    return {"ok": True}


_mod.handler = _handler
_mod.VERBS = {}
sys.modules["watch_probe.tools"] = _mod


async def _mk(kernel, agent_id, **meta):
    out = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "watch_probe.tools",
                        "id": agent_id,
                        **meta,
                    },
                }
            ]
        )
    )[0]
    assert out["status"] == "completed", out
    return agent_id


@pytest.fixture
async def wired(seeded_kernel):
    await _mk(seeded_kernel, "secret")  # holds the confidential traffic
    await _mk(seeded_kernel, "nosy")  # a leg with NO grant on `secret`
    return seeded_kernel


async def test_an_ungranted_agent_cannot_watch_another(wired):
    with sender_context("nosy"):
        wired.ctx.ensure_inbox("nosy")  # a watcher registers first
        denial = wired.watch("secret", "nosy")
    assert denial is not None, "an ungranted watch was allowed — read side-channel"
    assert denial["reason"] == "unauthorized"
    # ...and nothing was registered, so no payload can ever reach it.
    assert "nosy" not in wired.ctx.agents["secret"]._watcher_ids


async def test_an_explicit_watch_grant_is_honoured(wired):
    # The edge is granted like any other: two-sided, named by verb.
    await wired.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": "secret",
                    "callable_by": {
                        "$self": "*",
                        "$parent": "*",
                        "$children": "*",
                        "nosy": ["watch"],
                    },
                },
            }
        ]
    )
    await wired.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": "nosy",
                    "can_call": {
                        "$self": "*",
                        "$parent": "*",
                        "$children": "*",
                        "secret": ["watch"],
                    },
                },
            }
        ]
    )
    with sender_context("nosy"):
        wired.ctx.ensure_inbox("nosy")  # a watcher registers first
        denial = wired.watch("secret", "nosy")
    assert denial is None, denial
    assert "nosy" in wired.ctx.agents["secret"]._watcher_ids


async def test_an_operator_watch_stays_ungated(wired):
    # `sender=None` is the origin root of trust — the bootstrap wiring a
    # renderer, a test driver. Same rule as send.
    wired.ctx.ensure_inbox("cli")
    assert wired.watch("secret", "cli") is None
    assert "cli" in wired.ctx.agents["secret"]._watcher_ids


async def test_the_state_stream_needs_an_explicit_edge_on_the_root(wired):
    # The broadest read the kernel offers: every send's sender, target, verb and
    # payload summary, for every agent. An open ingress rule says a client may
    # REACH the leg; it says nothing about what it may then READ.
    with sender_context("nosy"):
        denial = wired.authorize_state_subscribe("nosy")
    assert denial is not None, "the whole state stream was readable ungranted"
    assert denial["reason"] == "unauthorized"

    await wired.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": "nosy",
                    "can_call": {
                        "$self": "*",
                        "$parent": "*",
                        "$children": "*",
                        wired.id: ["state_subscribe"],
                    },
                },
            }
        ]
    )
    with sender_context("nosy"):
        assert wired.authorize_state_subscribe("nosy") is None


async def test_an_operator_may_tap_the_state_stream(wired):
    assert wired.authorize_state_subscribe("cli") is None


# ─── the ROW EVENT: an AI agent announces its own rows ────────────────────
#
# The kernel holds no conversation, so there is no row event on the state stream.
# An AI agent announces every row it appends to whoever `watch`es IT — gated by that
# agent's own `callable_by` — as a content-free digest; a `body:true` watcher gets
# the content only for rows a live grant from the agent gives it.


async def _ai_with_watcher(kernel):
    from _testkit import memory_meta

    import ollama_backend.tools  # noqa: F401 — the bundle the AI runs

    kernel.create(
        "ollama_backend.tools",
        id="talker",
        callable_by={"viewer": ["watch", "unwatch"]},
        **memory_meta(kernel, "talker", {"cli": "*"}),
    )
    kernel.create(
        None, id="viewer", can_call={"talker": ["watch", "unwatch"], "memory": "*"}
    )
    cb = dict(kernel.ctx.agents["memory"]._meta["callable_by"])
    kernel.ctx.update("memory", callable_by={**cb, "viewer": "*"})
    return kernel.ctx.ensure_inbox("viewer")


async def test_an_ai_agent_announces_its_rows_to_its_watchers_as_digests(wired):
    from ai_core import core

    inbox = await _ai_with_watcher(wired)
    with sender_context("viewer"):
        out = (await wired.send([{"target": "talker", "payload": {"type": "watch"}}]))[
            0
        ]
    assert out["result"] == {"watching": True, "watcher": "viewer", "body": False}
    await core._save_history(
        "talker", wired, [{"role": "user", "content": "SECRET line one\nline two"}]
    )
    ev = inbox.get_nowait()
    assert ev["type"] == "row" and ev["source"] == "talker"
    assert ev["row"]["bytes"] == 24 and "content" not in ev["row"], "a digest only"


async def test_row_bodies_reach_only_a_watcher_the_agent_granted(wired):
    from ai_core import core

    inbox = await _ai_with_watcher(wired)
    with sender_context("viewer"):
        await wired.send(
            [{"target": "talker", "payload": {"type": "watch", "body": True}}]
        )
    await core._save_history("talker", wired, [{"role": "user", "content": "first"}])
    assert "content" not in inbox.get_nowait()["row"], "no grant, no body"
    # the agent shares its head with the viewer (a live grant) — bodies flow
    with sender_context("talker"):
        g = (
            await wired.send(
                [
                    {
                        "target": "memory",
                        "payload": {
                            "type": "share",
                            "with": "viewer",
                            "scope": {"head_until": 10**9},
                            "mode": "live",
                        },
                    }
                ]
            )
        )[0]["result"]
    assert g.get("grant"), g
    core._mem(wired)["grants"].pop("talker", None)
    await core._my_grants("talker", wired)
    await core._save_history("talker", wired, [{"role": "user", "content": "second"}])
    assert inbox.get_nowait()["row"]["content"] == "second"
    # an ungated watcher is refused by the agent's own gate
    wired.create(None, id="stranger", can_call={"talker": "*"})
    wired.ctx.ensure_inbox("stranger")
    with sender_context("stranger"):
        out = (await wired.send([{"target": "talker", "payload": {"type": "watch"}}]))[
            0
        ]
    assert out["result"]["reason"] == "unauthorized"
