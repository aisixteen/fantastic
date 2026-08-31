"""ADV-4 — provenance cannot be laundered: no wire hop becomes the operator.

LangGraph's resume channel is an untyped value: `Command(resume=v)` carries no
provenance, so if the channel is LLM-reachable the AI supplies its own "yes."
The broader class: in ambient-authority frameworks ANY hop that reaches the
runtime reaches it with the runtime's full authority.

Here `sender=None` (operator) exists ONLY in-process. A wire frame dispatches
AS ITS LEG AGENT (pinned by C21), the leg is a gated agent like any other, and
privileged verbs (create/update/delete of OTHERS) demand explicit naming that
a wildcard grant deliberately does not cover. So the same envelope that the
operator runs freely dies twice over the wire — once as an over-broad wildcard,
once as a sealed default. C21 transport symmetry extends this pin to every leg.
"""

from __future__ import annotations

import json

import pytest
from fastapi.testclient import TestClient

from web.app import make_app
from web_ws.tools import _make_endpoint


@pytest.fixture
def wired(seeded_kernel):
    app = make_app("adv4_web", seeded_kernel)
    # a DELIBERATELY over-granted leg: wildcard on the kernel itself
    seeded_kernel.create(
        "web_ws.tools",
        id="adv4_leg",
        ingress_rule="allow_all_ingress",
        can_call={"kernel_state": "*"},
    )
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("adv4_leg", seeded_kernel)
    )
    with TestClient(app) as c:
        yield seeded_kernel, c


def _ws_call(ws, cid, target, payload):
    ws.send_text(
        json.dumps({"type": "call", "id": cid, "target": target, "payload": payload})
    )
    for _ in range(50):
        msg = json.loads(ws.receive_text())
        if msg.get("type") == "reply" and msg.get("id") == cid:
            return msg["data"]
    raise AssertionError(f"no reply for {cid}")


_MINT = {
    "type": "create_agent",
    "handler_module": "text_file.tools",
    "id": "adv4_minted",
}


def test_a_wire_frame_cannot_mint_agents_even_over_a_wildcard_leg(wired):
    kernel, client = wired
    with client.websocket_connect("/kernel_state/ws") as ws:
        data = _ws_call(ws, "m1", "kernel_state", dict(_MINT, parent_id="kernel_state"))
    assert data.get("reason") == "unauthorized", (
        f"a wire hop minted an agent — provenance laundered: {data!r}"
    )
    # the refusal names the LEG as the sender: the frame ran AS adv4_leg,
    # never as the operator
    assert "adv4_leg" in data.get("error", ""), data


async def test_the_same_envelope_in_process_is_operator_and_succeeds(seeded_kernel):
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": dict(_MINT, parent_id=seeded_kernel.id),
                }
            ]
        )
    )[0]
    assert out["status"] == "completed", out
    assert out["result"]["id"] == "adv4_minted"


def test_update_and_delete_are_walled_the_same_way(wired):
    kernel, client = wired
    with client.websocket_connect("/kernel_state/ws") as ws:
        upd = _ws_call(
            ws,
            "u1",
            "kernel_state",
            {"type": "update_agent", "id": "cli", "can_call": {"kernel_state": "*"}},
        )
        dele = _ws_call(ws, "d1", "kernel_state", {"type": "delete_agent", "id": "cli"})
    assert upd.get("reason") == "unauthorized", upd
    assert dele.get("reason") == "unauthorized", dele
