"""The in-process io LEG GATE, pinned across every leg that copies it.

file_bridge and exec_bridge each carry their own handler gate, their own
write-time refusals and their own Decision→refusal conversion; web_rest and
web_ws convert a Decision by hand too; rule-name scoping is ONE function
(`io_bridge._base.describe`) for legs and bridges alike. Sharing one gate (reduction
candidate G) must keep, per leg: the sealed-by-default refusal (text, hint, key
order), which verbs stay UNGATED, the `kind` each leg asks its rule about, the
write-time refusal texts, and the reflected rule names.
"""

from __future__ import annotations

import asyncio
import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from io_bridge import ingress_rules
from io_bridge._base import ALLOW, describe

SEALED_HINT = (
    "this edge is sealed — open it: update_agent <id> ingress_rule=allow_all_ingress"
    " (or password); reflect readme=true on this agent for the channel model"
)
SEALED = {
    "error": "edge sealed by policy",
    "reason": "unauthorized",
    "hint": SEALED_HINT,
}


def _unknown(name: str) -> dict:
    return {
        "error": f"edge sealed — unknown ingress rule {name!r}",
        "reason": "unauthorized",
        "hint": SEALED_HINT
        + (
            f". {name!r} is not an INGRESS rule name — the legal values are "
            "allow_all_ingress, deny_ingress, password_ingress (names are "
            "manifold-scoped: an egress name here is a typo, not a policy)"
        ),
    }


async def _call(k, target, payload) -> dict:
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _create(k, **meta) -> dict:
    return await _call(k, "kernel", {"type": "create_agent", "parent_id": k.id, **meta})


# ─── the sealed-by-default refusal, per leg ──────────────────────────────────


async def test_file_bridge_sealed_refusal_is_exact(seeded_kernel):
    k = seeded_kernel
    k.create("file_bridge.tools", id="fb", allow=["."])
    r = await _call(k, "fb", {"type": "list", "path": ""})
    assert r == SEALED and list(r) == ["error", "reason", "hint"]
    # reflect stays UNGATED on a sealed leg
    refl = await _call(k, "fb", {"type": "reflect"})
    assert refl["sealed"] is True and refl["ingress_rule"] == "deny_ingress"
    # an unknown verb is refused BEFORE the gate, with its own code
    assert await _call(k, "fb", {"type": "frobnicate"}) == {
        "error": "file_bridge: unknown type 'frobnicate'",
        "reason": "unhandled",
        "verb": "frobnicate",
    }


async def test_exec_bridge_sealed_refusal_is_exact(seeded_kernel):
    k = seeded_kernel
    k.create("exec_bridge.tools", id="ex", allowed=["x"], cwd=".")
    r = await _call(k, "ex", {"type": "start", "command": "x"})
    assert r == SEALED and list(r) == ["error", "reason", "hint"]
    # reflect AND connector stay UNGATED on a sealed leg
    assert (await _call(k, "ex", {"type": "reflect"}))["sealed"] is True
    assert "howto" in await _call(k, "ex", {"type": "connector"})
    # a verb the bundle does not own is mail — gated like every other verb
    assert await _call(k, "ex", {"type": "mail"}) == SEALED


async def test_an_unknown_auth_stem_seals_and_names_itself(seeded_kernel):
    k = seeded_kernel
    k.create("file_bridge.tools", id="fbu", allow=["."], auth="bogus")
    k.create("exec_bridge.tools", id="exu", allowed=["x"], cwd=".", auth="bogus")
    assert await _call(k, "fbu", {"type": "list", "path": ""}) == _unknown(
        "bogus_ingress"
    )
    assert await _call(k, "exu", {"type": "status"}) == _unknown("bogus_ingress")


def test_web_rest_sealed_body_is_exact(seeded_kernel):
    from web_rest.tools import _make_post_endpoint

    seeded_kernel.create("web_rest.tools", id="rs")
    app = FastAPI()
    app.add_api_route(
        "/rs/{target_id}", _make_post_endpoint("rs", seeded_kernel), methods=["POST"]
    )
    with TestClient(app) as c:
        r = c.post("/rs/kernel_state", json={"type": "reflect"})
    assert r.status_code == 403
    assert r.json() == SEALED and list(r.json()) == ["error", "reason", "hint"]


def test_web_ws_sealed_reply_is_exact(seeded_kernel):
    from web.app import make_app
    from web_ws.tools import _make_endpoint

    seeded_kernel.create("web_ws.tools", id="ws_sealed")
    # `port`: the authority the web's Host guard admits.
    seeded_kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", seeded_kernel)
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("ws_sealed", seeded_kernel)
    )
    with (
        TestClient(app, headers={"host": "127.0.0.1:8088"}) as c,
        c.websocket_connect("/kernel_state/ws") as ws,
    ):
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "id": "1",
                    "payload": {"type": "reflect"},
                }
            )
        )
        msg = json.loads(ws.receive_text())
    assert msg == {"type": "reply", "id": "1", "data": SEALED}


# ─── the KIND each leg asks its rule about ───────────────────────────────────


class _Recorder:
    seen: list = []

    def authorize(self, action):
        _Recorder.seen.append((action.kind, action.target, action.verb, action.token))
        return ALLOW


async def test_each_leg_names_its_own_action_kind(seeded_kernel, monkeypatch):
    monkeypatch.setitem(ingress_rules.REGISTRY, "probe_ingress", _Recorder)
    _Recorder.seen = []
    k = seeded_kernel
    k.create("file_bridge.tools", id="fbk", allow=["."], ingress_rule="probe_ingress")
    k.create(
        "exec_bridge.tools",
        id="exk",
        allowed=["x"],
        cwd=".",
        ingress_rule="probe_ingress",
    )
    await _call(k, "fbk", {"type": "list", "path": ""})
    await _call(k, "exk", {"type": "status"})
    await _call(k, "fbk", {"type": "reflect"})  # ungated: the rule is not asked
    assert _Recorder.seen == [
        ("call", "fbk", "list", None),
        ("spawn", "exk", "spawn", None),
    ]


# ─── write-time refusals ─────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "module, edge, extra",
    [
        ("file_bridge.tools", "fs edge", {"allow": ["."]}),
        ("exec_bridge.tools", "process edge", {"allowed": ["x"], "cwd": "."}),
    ],
)
async def test_password_is_refused_on_an_in_process_leg(
    seeded_kernel, module, edge, extra
):
    for field, value in (("ingress_rule", "password_ingress"), ("auth", "password")):
        r = await _create(
            seeded_kernel, handler_module=module, **extra, **{field: value}
        )
        assert r["reason"] == "invalid_meta", r
        assert r["error"].endswith(
            f"{field}={value!r} is not applicable to the {edge}: a leg with no "
            "wire envelope can never present a token — legal values: "
            "allow_all_ingress, deny_ingress"
        ), r


_IN_PROCESS = "allow_all_ingress, deny_ingress"
_WIRE = "allow_all_ingress, deny_ingress, password_ingress"


@pytest.mark.parametrize(
    "module, extra, legal",
    [
        ("file_bridge.tools", {"allow": ["."]}, _IN_PROCESS),
        ("exec_bridge.tools", {"allowed": ["x"], "cwd": "."}, _IN_PROCESS),
        ("ws_bridge.tools", {"transport": "memory"}, _WIRE),
        ("relay_connector.tools", {"transport": "memory"}, _WIRE),
        ("web_rest.tools", {}, _WIRE),
        ("web_ws.tools", {}, _WIRE),
    ],
)
@pytest.mark.parametrize(
    "spec",
    [
        "open_sesame",
        7,
        ["allow_all_ingress"],
        {"type": ["x"]},
        {"type": "password_ingress", "env": 7},
    ],
)
async def test_every_leg_refuses_an_unknown_or_malformed_rule_at_the_write(
    seeded_kernel, module, extra, legal, spec
):
    """ONE check (`unknown_ingress_refusal`) on every leg. The wire legs had
    none: `ingress_rule=7` was accepted with a record, and every later frame
    raised (web_rest 500, a bridge call with no reply)."""
    r = await _create(seeded_kernel, handler_module=module, ingress_rule=spec, **extra)
    assert r["reason"] == "invalid_meta", r
    assert r["error"].endswith(
        f"ingress_rule={spec!r} is not a rule this bundle implements — "
        f"legal values: {legal}"
    ), r


@pytest.mark.parametrize(
    "module, extra",
    [
        ("ws_bridge.tools", {"transport": "memory"}),
        ("relay_connector.tools", {"transport": "memory"}),
        ("web_rest.tools", {}),
        ("web_ws.tools", {}),
    ],
)
async def test_a_wire_leg_takes_every_rule_it_implements(seeded_kernel, module, extra):
    for spec in (
        "allow_all_ingress",
        "deny_ingress",
        "password_ingress",
        {"type": "password_ingress", "env": "GROUP_TOKEN"},
    ):
        r = await _create(
            seeded_kernel, handler_module=module, ingress_rule=spec, **extra
        )
        assert "error" not in r, (spec, r)


async def test_a_malformed_rule_on_a_bridge_record_denies_the_call(seeded_kernel):
    """A record can still carry one (written below the verbs, or by hand on
    disk): the inbound call is ANSWERED with the seal's denial — never a raise in
    the call task that leaves the peer waiting out its timeout."""
    from io_bridge import MemoryTransport, _test_transport_inject

    k = seeded_kernel
    k.update(k.id, guid="guid-a")
    k.create("ws_bridge.tools", id="br", transport="memory")
    k.update("br", ingress_rule=7)
    mine, peer = MemoryTransport.pair()
    _test_transport_inject["br"] = mine
    assert (await _call(k, "br", {"type": "boot"})).get("booted") is True
    await peer.send(
        {
            "type": "call",
            "id": "in-1",
            "target": k.id,
            "payload": {"type": "reflect"},
        }
    )
    reply = await asyncio.wait_for(peer.recv(), 2.0)
    assert reply == {"type": "reply", "id": "in-1", "data": _unknown("7")}
    await _call(k, "kernel", {"type": "delete_agent", "id": "br"})


# ─── reflected rule names ────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "record, ingress, egress, sealed",
    [
        ({}, "deny_ingress", "silent_egress", True),
        ({"auth": "allow_all"}, "allow_all_ingress", "silent_egress", False),
        ({"auth": "deny"}, "deny_ingress", "silent_egress", True),
        ({"auth": "password"}, "password_ingress", "password_egress", True),
        (
            {"auth": {"type": "password", "env": "X"}},
            "password_ingress",
            "password_egress",
            True,
        ),
        # a per-direction field is reported AS WRITTEN and wins over `auth`
        (
            {"auth": "password", "ingress_rule": "allow_all_ingress"},
            "allow_all_ingress",
            "password_egress",
            False,
        ),
        ({"egress_rule": "password_egress"}, "deny_ingress", "password_egress", True),
        ({"ingress_rule": "typo"}, "typo", "silent_egress", True),
        # an already-suffixed `auth` stem is scoped AGAIN — exactly what the gate
        # resolves (`allow_all_ingress_ingress`: unknown, sealed)
        (
            {"auth": "allow_all_ingress"},
            "allow_all_ingress_ingress",
            "allow_all_ingress_egress",
            True,
        ),
    ],
)
def test_describe_reports_what_the_gate_resolves(record, ingress, egress, sealed):
    assert describe(record) == {
        "ingress_rule": ingress,
        "egress_rule": egress,
        "sealed": sealed,
    }


@pytest.mark.parametrize(
    "record, ingress, egress, sealed",
    [
        ({}, "deny_ingress", "silent_egress", True),
        ({"auth": "allow_all"}, "allow_all_ingress", "silent_egress", False),
        ({"auth": "deny"}, "deny_ingress", "silent_egress", True),
        ({"auth": "password"}, "password_ingress", "password_egress", True),
        (
            {"auth": "password", "ingress_rule": "allow_all_ingress"},
            "allow_all_ingress",
            "password_egress",
            False,
        ),
        ({"egress_rule": "password_egress"}, "deny_ingress", "password_egress", True),
    ],
)
async def test_a_bridge_leg_reflects_the_same_names(
    seeded_kernel, record, ingress, egress, sealed
):
    k = seeded_kernel
    k.create("ws_bridge.tools", id="br", transport="memory", **record)
    r = await _call(k, "br", {"type": "reflect"})
    assert (r["ingress_rule"], r["egress_rule"], r["sealed"]) == (
        ingress,
        egress,
        sealed,
    )
    assert describe(k.ctx.get("br")) == {
        "ingress_rule": ingress,
        "egress_rule": egress,
        "sealed": sealed,
    }


async def test_a_bridge_reflects_exactly_what_its_gate_enforces(seeded_kernel):
    """`auth` carries a bare STEM and the gate appends `_ingress` to it, so
    `auth: allow_all_ingress` resolves `allow_all_ingress_ingress` — unknown, and
    it denies. The bridge reflect must not skip an already-suffixed stem and report
    `allow_all_ingress` / sealed:false, an open door its gate never opened."""
    k = seeded_kernel
    k.create("ws_bridge.tools", id="br", transport="memory", auth="allow_all_ingress")
    rule = ingress_rules.resolve_ingress(k.ctx.get("br"))
    assert isinstance(rule, ingress_rules.DenyInbound)
    assert rule.unknown == "allow_all_ingress_ingress"
    r = await _call(k, "br", {"type": "reflect"})
    assert (r["ingress_rule"], r["egress_rule"], r["sealed"]) == (
        "allow_all_ingress_ingress",
        "allow_all_ingress_egress",
        True,
    )


# ─── web_rest: every route reads the same gate ───────────────────────────────

_COUNT: dict = {}


def _count_mod():
    import types
    from kernel import register_unlisted

    mod = types.ModuleType("leg_count_probe.tools")

    async def handler(id, payload, kernel):
        verb = payload.get("type")
        if verb in ("boot", "shutdown"):
            return None
        if verb == "reflect":
            return {"id": id, "sentence": "counter", "verbs": {}}
        if verb == "nothing":
            return None
        _COUNT[id] = _COUNT.get(id, 0) + 1
        return {"n": _COUNT[id]}

    mod.handler = handler
    mod.VERBS = {}
    register_unlisted("leg_count_probe.tools", mod)


def _rest_app(kernel, leg: str):
    from web_rest.tools import (
        _make_post_endpoint,
        _make_reflect_get,
        _make_reflect_root,
    )

    app = FastAPI()
    app.add_api_route(
        f"/{leg}/_reflect", _make_reflect_root(leg, kernel), methods=["GET"]
    )
    app.add_api_route(
        f"/{leg}/_reflect/{{target_id}}",
        _make_reflect_get(leg, kernel),
        methods=["GET"],
    )
    app.add_api_route(
        f"/{leg}/{{target_id}}", _make_post_endpoint(leg, kernel), methods=["POST"]
    )
    return app


def test_web_rest_reflect_gets_are_gated_too(seeded_kernel):
    seeded_kernel.create("web_rest.tools", id="rg")
    with TestClient(_rest_app(seeded_kernel, "rg")) as c:
        for path in ("/rg/_reflect", "/rg/_reflect/kernel_state"):
            r = c.get(path)
            assert r.status_code == 403 and r.json() == SEALED, path


def test_web_rest_execution_id_header_names_one_execution(seeded_kernel):
    _count_mod()
    _COUNT.clear()
    k = seeded_kernel
    k.create("leg_count_probe.tools", id="ctr", callable_by={"ro": "*"})
    k.create(
        "web_rest.tools",
        id="ro",
        ingress_rule="allow_all_ingress",
        can_call={"ctr": "*"},
    )
    with TestClient(_rest_app(k, "ro")) as c:
        h = {"X-Fantastic-Execution-Id": "once-1"}
        a = c.post("/ro/ctr", json={"type": "count"}, headers=h)
        b = c.post("/ro/ctr", json={"type": "count"}, headers=h)
        plain = c.post("/ro/ctr", json={"type": "count"})
        none = c.post("/ro/ctr", json={"type": "nothing"})
        refl = c.get("/ro/_reflect/ctr?tree=none")
    assert a.status_code == 200 and a.json() == {"n": 1}
    assert b.json() == {"n": 1}, "a named repeat returns the recorded outcome"
    assert plain.json() == {"n": 2} and _COUNT["ctr"] == 2
    assert none.status_code == 204 and none.content == b""
    assert refl.status_code == 200 and refl.json()["id"] == "ctr"
    assert "tree" not in refl.json()


def test_web_rest_binary_leg_header_refusals(seeded_kernel):
    seeded_kernel.create("web_rest.tools", id="rb", ingress_rule="allow_all_ingress")
    octet = {"content-type": "application/octet-stream"}
    with TestClient(_rest_app(seeded_kernel, "rb")) as c:
        r = c.post(
            "/rb/kernel_state",
            content=b"raw",
            headers={**octet, "X-Fantastic-Payload": "{not json"},
        )
        assert r.status_code == 400
        assert r.json() == {"error": "web_rest: X-Fantastic-Payload is not valid JSON"}
        r = c.post(
            "/rb/kernel_state",
            content=b"raw",
            headers={**octet, "X-Fantastic-Payload": "[1]"},
        )
        assert r.status_code == 400
        assert r.json() == {
            "error": "web_rest: X-Fantastic-Payload must be a JSON object"
        }


# ─── a bridge leg's verbs refuse a missing target in their own words ─────────


async def test_bridge_verbs_name_their_missing_target(seeded_kernel):
    k = seeded_kernel
    k.create("ws_bridge.tools", id="brv", transport="memory")
    assert await _call(k, "brv", {"type": "forward", "payload": {"type": "x"}}) == {
        "error": "bridge.forward: target (str) + payload (dict) required"
    }
    assert await _call(k, "brv", {"type": "forward", "target": "t"}) == {
        "error": "bridge.forward: target (str) + payload (dict) required"
    }
    assert await _call(k, "brv", {"type": "watch_remote"}) == {
        "error": "bridge.watch_remote: target (str) required"
    }
    assert await _call(k, "brv", {"type": "unwatch_remote", "target": 3}) == {
        "error": "bridge.unwatch_remote: target (str) required"
    }
