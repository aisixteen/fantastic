"""C21 — ONE semantics, MANY execution modes.

The canonical form of this test elsewhere is narrower than ours. `openai-agents`
runs one scenario streamed and non-streamed and asserts the projections match
(`tests/test_runtime_symmetry_contract.py`); `a2a-tck` parametrizes every core
test over `ALL_TRANSPORTS` and specifies `BIND-EQUIV-001..004` — a multi-transport
server must expose the same operations with semantically equivalent results.

Here the same envelope rides FOUR ways into one kernel:

    in_process   kernel.send                      — the primitive
    web_rest     POST /<leg>/<target>             — http INBOUND derivation
    web_ws       a `call` frame over a websocket  — ws INBOUND derivation

The INBOUND derivations are the scope here, because they all terminate in the
same kernel and so are directly comparable. `ws_bridge` / `relay_connector` are
CROSS-KERNEL derivations — they need a counterpart kernel to forward to, and
"bridge to yourself" is not the thing they do. Their equivalence is asserted
where it means something, against a second live kernel, in
`integration_tests/bridge/`.
    ws_bridge    forward over MemoryTransport     — cross-kernel derivation

They are not four implementations of a protocol: they are four EXTRACTORS in
front of one `kernel.send`. This test is what keeps that true, because a
transport that quietly reshaped a reply — dropping a field, unwrapping an error,
serialising a denial differently — would make the same agent behave differently
depending on who dialled it, and nothing else in the suite would notice.

All in-process: TestClient for the web legs, MemoryTransport for the bridge. No
server, no ports, no sleeps.
"""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from web.app import make_app
from web_rest.tools import _make_post_endpoint
from web_ws.tools import _make_endpoint

# The scenarios every mode must agree on. Two shapes that matter: a normal
# reply, and a DENIAL — a transport is most likely to reshape the error path,
# because that is the one it is tempted to "handle".
CASES = [
    ("reflect the root", "kernel_state", {"type": "reflect", "tree": "none"}),
    ("read an agent record", "kernel_state", {"type": "get", "id": "kernel_state"}),
    ("an unknown verb", "kernel_state", {"type": "no_such_verb"}),
    ("an unknown target", "nobody_home", {"type": "reflect"}),
    ("a gate denial", "sealed", {"type": "list", "path": ""}),
]

# `reflect` carries run-scoped fields (a fresh guid per kernel, timings) that are
# not part of the semantics under test.
VOLATILE = {"guid", "started_at", "ts", "port", "running"}


def _project(reply):
    """Reduce a reply to what every mode MUST agree on, recursively."""
    if isinstance(reply, dict):
        return {k: _project(v) for k, v in sorted(reply.items()) if k not in VOLATILE}
    if isinstance(reply, list):
        return [_project(v) for v in reply]
    return reply


async def _seed(kernel) -> None:
    """Build the WHOLE tree up front — every leg every mode will use, plus one
    sealed agent so each mode also has a DENIAL to agree about.

    All of it before any mode runs, because the reply to a `reflect` names the
    roster: a driver that created its own leg lazily would change the tree it
    was measuring and the modes would disagree about `agent_count` for a reason
    that has nothing to do with transport.
    """
    # The legs are granted the edge they will exercise, so that every mode ends
    # at the SAME gate. Without this the comparison is meaningless rather than
    # failing: `in_process` runs as OPERATOR (`_current_sender` is None) and
    # skips the call gate entirely, while a web leg dispatches under its own id
    # and is an ordinary agent send — so a sealed target answers "edge sealed by
    # policy" to one and an egress denial to the other. That difference is the
    # PRINCIPAL, not the carrier, and it is correct behaviour.
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "sealed",
                        "callable_by": {"rest_leg": "*", "ws_leg": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    for leg, mod in (
        ("rest_leg", "web_rest.tools"),
        ("ws_leg", "web_ws.tools"),
    ):
        kernel.create(
            mod,
            id=leg,
            ingress_rule="allow_all_ingress",
            can_call={"sealed": "*", "kernel_state": "*"},
        )


# ─── the four modes ─────────────────────────────────────────────


async def _via_in_process(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _via_web_rest(kernel, target, payload):
    app = FastAPI()
    app.add_api_route(
        "/rest_leg/{target_id}",
        _make_post_endpoint("rest_leg", kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        return c.post(f"/rest_leg/{target}", json=payload).json()


async def _via_web_ws(kernel, target, payload):
    app = make_app("test_web", kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint("ws_leg", kernel))
    with TestClient(app) as c, c.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(
            json.dumps(
                {"type": "call", "target": target, "payload": payload, "id": "1"}
            )
        )
        for _ in range(10):
            msg = json.loads(ws.receive_text())
            if msg.get("type") == "reply" and msg.get("id") == "1":
                return msg["data"]
    raise AssertionError("no reply frame")


MODES = {
    "in_process": _via_in_process,
    "web_rest": _via_web_rest,
    "web_ws": _via_web_ws,
}


# ─── the contract ───────────────────────────────────────────────


@pytest.mark.parametrize("name,target,payload", CASES, ids=[c[0] for c in CASES])
async def test_every_mode_projects_the_same_reply(kernel, name, target, payload):
    """BIND-EQUIV: one envelope, many carriers, one answer. Asserted on a
    PROJECTION rather than raw equality, because run-scoped fields (a per-kernel
    guid, timings) legitimately differ and pinning them would make this test
    flap instead of catching drift."""
    await _seed(kernel)
    got = {}
    for mode, drive in MODES.items():
        got[mode] = _project(await drive(kernel, target, payload))

    first = next(iter(MODES))
    for mode in list(MODES)[1:]:
        assert got[mode] == got[first], (
            f"{name}: {mode} and {first} disagree.\n"
            f"  {first}: {got[first]}\n  {mode}: {got[mode]}"
        )


async def test_a_denial_survives_every_carrier_intact(kernel):
    """The error path is where a transport is most tempted to "help" — to
    unwrap, to translate a refusal into an HTTP status and drop the body, to
    swallow the hint. A denial that arrives without its `reason` is
    indistinguishable from an empty success."""
    await _seed(kernel)
    for mode, drive in MODES.items():
        r = await drive(kernel, "sealed", {"type": "list", "path": ""})
        assert r.get("reason") == "unauthorized", f"{mode} lost the reason: {r}"
        assert r.get("hint"), f"{mode} lost the teaching hint: {r}"


async def test_an_unknown_target_reads_the_same_way_everywhere(kernel):
    """Unknown-id is the one error a transport can plausibly answer BY ITSELF —
    a 404 before the kernel is ever asked. If one carrier does that, callers get
    two different truths about whether an agent exists."""
    await _seed(kernel)
    replies = {
        m: await d(kernel, "nobody_home", {"type": "reflect"}) for m, d in MODES.items()
    }
    for mode, r in replies.items():
        assert "error" in r, f"{mode} did not report the unknown target: {r}"
        assert "nobody_home" in r["error"], f"{mode} lost the id: {r}"


def test_the_mode_list_is_not_silently_empty():
    """A parametrized suite that discovers zero modes passes loudly-green while
    testing nothing. Pin the count so deleting a mode is a decision."""
    assert len(MODES) >= 3, "cross-mode with fewer than three carriers proves little"
