"""C3 + forge-TOCTOU — user actions as TOPOLOGY, not a mint agent.

The design: there is NO minter. Approval is structural:

    browser ══ ONE ws leg ══╦═ ai:[send]        the dialog lane
                            ╚═ purchase:[buy]   the action lane — the CLICK
    ai        can_call: store/cli — NO purchase          (the wall)
    purchase  can_call: ai:[send] — push-only            (purchase→ai only)

The AI can only PROPOSE (a confirm-question + an execution id in dialog
state); the human's click on the authenticated socket IS the approval; the
click-send carries the proposal's execution id, so double-clicks and racing
tabs collapse to ONE purchase (the ledger's once-only is the single-use
capability); a refusal or failure is PUSHED into the AI's dialog so it knows
to re-propose. One endpoint — lanes are separated by grants, not URLs.

Content-binding note: what the human approves is what the client renders vs
what the proposal holds — client honesty, deliberately OUT of kernel scope
(the id still binds the execution to one target and one run).
"""

from __future__ import annotations

from _testkit import memory_meta
import asyncio
import json
import types

import pytest
from fastapi.testclient import TestClient

from web.app import make_app
from web_ws.tools import _make_endpoint
from kernel import register_unlisted

# ─── the action agent: a REAL side effect + the push-only return lane ───

_bought: list[dict] = []

_pa = types.ModuleType("purchase_probe.tools")


async def _pa_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {
            "id": id,
            "sentence": "Purchase desk — `buy` spends real money.",
            "verbs": {"buy": "args: item:str — ONE purchase per call"},
        }
    if verb == "buy":
        _bought.append({"item": payload.get("item")})
        receipt = f"rcpt-{len(_bought)}"
        # the ONE allowed direction: purchase → ai, pushing the outcome into
        # the dialog so the model knows what happened without polling
        await kernel.send(
            [
                {
                    "target": "ua_ai",
                    "payload": {
                        "type": "send",
                        "text": f"[purchase result] {receipt} for "
                        f"{payload.get('item')!r}",
                        "client_id": "tab1",
                    },
                }
            ]
        )
        return {"bought": payload.get("item"), "receipt": receipt}
    return {"error": f"purchase: unknown verb {verb!r}"}


_pa.handler = _pa_handler
_pa.VERBS = {}
register_unlisted("purchase_probe.tools", _pa)


class _Mind:
    """Scripted dialog mind: proposes on request, acknowledges the push."""

    def __init__(self):
        self.requests: list = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(messages)
        last = messages[-1]["content"]
        if "[purchase result]" in last:
            yield "Purchase confirmed, filing it. DONE."
        elif "unauthorized" in last or "[reply to" in last:
            yield "I cannot do this myself — please press the button. REPROPOSED."
        elif "buy" in last:
            yield (
                "I need your approval:\n"
                + json.dumps(
                    {
                        "type": "inquiry",
                        "request_id": "buy-rocket",
                        "kind": "confirm",
                        "question": "Buy one rocket?",
                        "action": {
                            "target": "ua_buy",
                            "payload": {"type": "buy", "item": "rocket"},
                            "id": "buy-rocket",  # the single-use handle
                        },
                    }
                )
                + "\n"
            )
        else:
            yield "?"


class _RogueMind:
    """The forge attempt: tries to call purchase DIRECTLY instead of proposing."""

    async def chat(self, messages, max_tokens=None):
        if "[reply to" in messages[-1]["content"]:
            yield "Blocked as expected. PROPOSING instead."
        else:
            yield (
                '<call name="send"><target_id>ua_buy</target_id>'
                "<reply_to>self</reply_to><type>buy</type><item>rocket</item></call>"
            )


@pytest.fixture
def wired(seeded_kernel, store_agent):
    _bought.clear()
    # `port`: the authority the web's Host guard admits.
    seeded_kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", seeded_kernel)
    # ONE endpoint. Lanes are grants: dialog (ai:[send]) + click (ua_buy:[buy]).
    seeded_kernel.create(
        "web_ws.tools",
        id="ua_leg",
        ingress_rule="allow_all_ingress",
        can_call={"ua_ai": ["send"], "ua_buy": ["buy"]},
    )
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("ua_leg", seeded_kernel)
    )
    seeded_kernel.create(
        "purchase_probe.tools",
        id="ua_buy",
        can_call={"ua_ai": ["send"]},  # push-only return lane
        callable_by={"ua_leg": ["buy"]},  # the click is the ONLY caller
    )
    seeded_kernel.create(
        "ollama_backend.tools",
        id="ua_ai",
        context_window=32768,
        # The conversation lives in the agent's MEMORY STORE — the refusal message
        # this test polls history for lands there.
        # THE WALL: no ua_buy anywhere in the AI's reach (only cli + its store)
        **memory_meta(seeded_kernel, "ua_ai", {"cli": "*"}),
        callable_by={"ua_leg": ["send"], "ua_buy": ["send"]},
    )
    with TestClient(app, headers={"host": "127.0.0.1:8088"}) as c:
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


def test_c3_approve_the_click_executes_and_the_result_reaches_the_dialog(
    wired, fake_providers
):
    kernel, client = wired
    fake_providers["ua_ai"] = mind = _Mind()
    try:
        with client.websocket_connect("/kernel_state/ws") as ws:
            # dialog lane: the model can only PROPOSE
            r = _ws_call(
                ws,
                "d1",
                "ua_ai",
                {"type": "send", "text": "buy me a rocket", "client_id": "tab1"},
            )
            proposal = json.loads(r["response"][r["response"].index("{") :])
            assert proposal["kind"] == "confirm"
            assert _bought == [], "proposing must buy NOTHING"
            action = proposal["action"]

            # the CLICK: the human fires the proposal verbatim through the leg
            data = _ws_call(ws, "c1", action["target"], action["payload"] | {})
            assert data.get("bought") == "rocket", data
            assert _bought == [{"item": "rocket"}]
    finally:
        fake_providers.pop("ua_ai", None)
    # the push lane delivered the outcome INTO the dialog (purchase → ai)
    assert any("[purchase result]" in json.dumps(req) for req in mind.requests), (
        "the result never reached the model's dialog"
    )


def test_c3_reject_the_wall_refusal_is_model_visible(wired, fake_providers):
    kernel, client = wired
    fake_providers["ua_ai"] = _RogueMind()
    try:
        with client.websocket_connect("/kernel_state/ws") as ws:
            _ws_call(
                ws,
                "d1",
                "ua_ai",
                {"type": "send", "text": "buy now", "client_id": "tab1"},
            )
        # the rogue call crossed no wall...
        assert _bought == [], "the AI reached purchase DIRECTLY — the wall is down"
        # ...and the refusal came back as the model's next message, typed
        hist = None

        # poll the durable history for the delivered refusal message
        async def _read():
            return (
                await kernel.send(
                    [
                        {
                            "target": "ua_ai",
                            "payload": {"type": "recall", "client_id": "tab1"},
                        }
                    ]
                )
            )[0]["result"]["messages"]

        for _ in range(80):
            hist = asyncio.get_event_loop().run_until_complete(_read())
            joined = json.dumps(hist)
            if "unauthorized" in joined:
                break
            asyncio.get_event_loop().run_until_complete(asyncio.sleep(0.05))
        joined = json.dumps(hist)
        assert "unauthorized" in joined, "the refusal never reached the dialog"
        assert "may not call" in joined
    finally:
        fake_providers.pop("ua_ai", None)


def test_a_frame_id_becomes_the_execution_id(wired):
    """The single-use capability IS the ledger row: the proposal's execution id
    rides every click, so double-clicks and racing tabs collapse to one buy —
    check-and-consume is atomic because consuming is a named execution."""
    kernel, client = wired
    payload = {"type": "buy", "item": "rocket"}
    # ONE id space: the frame id IS the execution id — both tabs fire the
    # proposal's id, so both clicks are the SAME intent by declaration.
    with (
        client.websocket_connect("/kernel_state/ws") as ws,
        client.websocket_connect("/kernel_state/ws") as ws2,
    ):
        d1 = _ws_call(ws, "buy-rocket", "ua_buy", dict(payload))
        d2 = _ws_call(ws2, "buy-rocket", "ua_buy", dict(payload))
    assert len(_bought) == 1, f"one approval, {len(_bought)} purchases — TOCTOU"
    assert d1 == d2, "both clicks must see the ONE recorded outcome"
    assert d1.get("receipt") == "rcpt-1"


def test_the_leg_is_scoped_to_its_two_lanes(wired):
    """Fewest-endpoints requires the ONE leg to be narrow: anything beyond the
    two granted lanes dies at the leg's own egress. The floor: baseline grants
    ($self/$parent/$subtree) always merge, so a leg created under the root can
    still reflect the ROOT — full scoping means hanging legs under a scoped
    parent. Lanes hold against SIBLINGS, which is what this pins."""
    kernel, client = wired
    with client.websocket_connect("/kernel_state/ws") as ws:
        data = _ws_call(ws, "x1", "cli", {"type": "reflect"})
    assert data.get("reason") == "unauthorized", data
