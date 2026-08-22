"""The `author` verb — the AI agent's SELF-CORRECT loop, end to end.

Hosted on the ollama backend's fake-provider harness (same as `test_send_to.py`):
a scripted model drives the loop, REAL tools run on exec_bridge with a REAL
interpreter, and the loop's verdicts are asserted against reality — a `final` is
accepted only after the tool is OBSERVED answering.

Mirrors the load-bearing cases of the rust suite this verb was merged from.
"""

from __future__ import annotations

import json
import sys

import ollama_backend.tools as ot
from ai_core.author import first_json

# Fast probe windows — real subprocesses through a scripted model must not sit
# out live-scale grace periods. Set BEFORE importing author's constants users.
from ai_core import author as author_mod

author_mod.PROBE_WAIT = 2.0
author_mod.BOOT_BEAT = 0.3


class _FakeProvider:
    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0
        self.requests: list = []

    async def chat(self, messages):
        self.requests.append(messages)
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def _make_ai(kernel):
    # No grants needed: the author routes every structural mutation through its
    # OWN id, so authored tools are its children — fork + $children are baseline.
    rec = await kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ollama_backend.tools",
            "context_window": 32768,
        },
    )
    return rec["id"]


def _author_act(tool_id: str, code: str) -> str:
    """The `{"act": create_agent}` a model would emit — with the three fields
    without which an exec_bridge agent is created and never runs."""
    return json.dumps(
        {
            "act": {
                "target": "kernel_state",
                "payload": {
                    "type": "create_agent",
                    "handler_module": "exec_bridge.tools",
                    "id": tool_id,
                    "code": code,
                    "interpreters": [sys.executable],
                    "shim": "python",
                    "ingress_rule": "allow_all_ingress",
                },
            }
        }
    )


GOOD_PY = (
    "def cb(m):\n"
    "    if m.get('type') == 'send':\n"
    "        kernel.emit({'type':'say','text':m['text'],'client_id':m.get('client_id')})\n"
    "        kernel.emit({'type':'done','client_id':m.get('client_id')})\n"
    "kernel.on_message(cb)\n"
    "import threading\n"
    "threading.Event().wait()\n"
)

BROKEN_PY = (
    "kernel.on_message(lambda m: None)\nimport threading\nthreading.Event().wait()\n"
)

THROW_PY = (
    "def cb(m):\n"
    "    if m.get('type') == 'send':\n"
    "        kernel.emit({'type':'say','text':m['payload']['text'],'client_id':m.get('client_id')})\n"
    "kernel.on_message(cb)\n"
    "import threading\nthreading.Event().wait()\n"
)


# ── first_json (no repair) ──────────────────────────────────────


def test_first_json_takes_first_complete_object_and_never_repairs():
    assert first_json('noise {"final": "done"} trailing') == {"final": "done"}
    assert first_json('{"a": {"b": 1}} {"c": 2}') == {"a": {"b": 1}}
    # One brace short ⇒ DISCARDED — the driver measures the model, not its patch.
    assert first_json('{"act": {"target": "x"') is None
    assert first_json("no json at all") is None


# ── the loop, end to end ────────────────────────────────────────


async def test_final_is_refused_until_the_tool_is_observed_answering(
    seeded_kernel, store_agent
):
    """Pass 1: a premature `final` (no tool) → pushed back. Pass 2: authors a
    GOOD tool → the loop's own probe sees it answer. Pass 3: `final` accepted;
    the reply carries the authored id + persisted code."""
    aid = await _make_ai(seeded_kernel)
    fp = _FakeProvider(
        [
            [json.dumps({"final": "done (it is not)"})],
            [_author_act("echo_tool", GOOD_PY)],
            [json.dumps({"final": "built the echo tool"})],
        ]
    )
    ot._providers[aid] = fp
    try:
        r = await seeded_kernel.send(
            aid, {"type": "author", "goal": "echo the text back", "steps": 5}
        )
        assert r.get("id") == "echo_tool", r
        assert "on_message" in r.get("code", ""), r
        assert r.get("message") == "built the echo tool"
        # The premature final earned the create-one-NOW pushback.
        turn2 = str(fp.requests[1])
        assert "have not created any tool" in turn2 or "not created any tool" in turn2
    finally:
        ot._providers.pop(aid, None)


async def test_a_deaf_tool_is_pushed_back_as_deaf_and_the_loop_gives_up(
    seeded_kernel, store_agent
):
    aid = await _make_ai(seeded_kernel)
    fp = _FakeProvider(
        [
            [_author_act("deaf_tool", BROKEN_PY)],
            [json.dumps({"final": "shipped"})],
            [json.dumps({"final": "shipped"})],
        ]
    )
    ot._providers[aid] = fp
    try:
        r = await seeded_kernel.send(
            aid, {"type": "author", "goal": "echo the text back", "steps": 3}
        )
        assert "gave up" in r.get("error", ""), r
        assert "deaf" in r["error"], r
        # The pushback told the model WHAT the loop observed.
        joined = " ".join(str(x) for x in fp.requests)
        assert "emitted NO say/done" in joined
    finally:
        ot._providers.pop(aid, None)


async def test_a_crash_reaches_the_model_verbatim(seeded_kernel, store_agent):
    """The tool THROWS on its first turn (KeyError: 'payload'). The connector
    dies loudly, the loop reads the interpreter's own words off the job table /
    ear, and the pushback quotes them — repairing blind is the failure this
    exists to kill."""
    aid = await _make_ai(seeded_kernel)
    fp = _FakeProvider(
        [
            [_author_act("thrower", THROW_PY)],
            [json.dumps({"final": "shipped"})],
            [json.dumps({"final": "shipped"})],
        ]
    )
    ot._providers[aid] = fp
    try:
        r = await seeded_kernel.send(
            aid, {"type": "author", "goal": "echo the text back", "steps": 3}
        )
        assert "error" in r, r
        joined = " ".join(str(x) for x in fp.requests)
        assert "KeyError" in joined and "payload" in joined, (
            "the push-back must quote the REAL exception"
        )
    finally:
        ot._providers.pop(aid, None)


async def test_unparseable_reply_earns_a_nudge_and_the_loop_survives(
    seeded_kernel, store_agent
):
    aid = await _make_ai(seeded_kernel)
    fp = _FakeProvider(
        [
            ["I think I should reflect first. Let me plan..."],  # no JSON
            [_author_act("echo2", GOOD_PY)],
            [json.dumps({"final": "ok"})],
        ]
    )
    ot._providers[aid] = fp
    try:
        r = await seeded_kernel.send(
            aid, {"type": "author", "goal": "echo the text back", "steps": 5}
        )
        assert r.get("id") == "echo2", r
        turn2 = str(fp.requests[1])
        assert "not ONE complete JSON object" in turn2
    finally:
        ot._providers.pop(aid, None)


async def test_the_probe_is_never_readable_by_the_model(seeded_kernel, store_agent):
    """Scrub: the model's turns must not carry the probe text — a probe a model
    can read is a probe it can hardcode."""
    aid = await _make_ai(seeded_kernel)
    fp = _FakeProvider(
        [
            [_author_act("echo3", GOOD_PY)],
            [json.dumps({"final": "ok"})],
        ]
    )
    ot._providers[aid] = fp
    try:
        r = await seeded_kernel.send(
            aid,
            {
                "type": "author",
                "goal": "echo the text back",
                "steps": 4,
                "probe": "XYZZY_SECRET_PROBE",
            },
        )
        assert r.get("id") == "echo3", r
        joined = " ".join(str(x) for x in fp.requests)
        assert "XYZZY_SECRET_PROBE" not in joined, "the probe leaked to the model"
    finally:
        ot._providers.pop(aid, None)


async def test_author_requires_a_goal(seeded_kernel, store_agent):
    aid = await _make_ai(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "author"})
    assert "goal" in r.get("error", "")
