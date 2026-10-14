"""C21 — one semantics, many transports (python seed).

The canonical assertion 2+ industry suites implement (a2a-tck `ALL_TRANSPORTS`,
openai-agents' runtime-symmetry contract) and the one this substrate claims to
beat: the SAME envelope through ANY transport yields the SAME reply. Five legs:

    in-proc        `root.send([...])`                      (the reference)
    ws bridge      io_bridge frames over MemoryTransport   (framing + correlation)
    web_rest       POST through the FastAPI surface        (HTTP)
    exec guest     the subprocess connector's `op:` wire   (guest shim)

Cases live in `c21_cases.yaml` as DATA — add one there and every transport is on
the hook; there is no per-transport test to forget. Replies are normalized
through a JSON round-trip (what every wire does anyway) and compared PAIRWISE —
per-case expectations catch a wrong answer, the pairwise diff catches a
transport quietly answering differently.

Attribution differs BY DESIGN — in-proc is the operator; the ws engine, the
relay, rest and exec each dispatch AS THEIR LEG AGENT (identity comes only from
dispatch: a leg's read loop, its inbound verbs and its job relay are its own).
So the fixture grants every leg the same reach, both halves (the probe's and the
root's `callable_by` name every leg), and cases avoid asserting on the gate
itself (the authz suites' job). What must be identical is the REPLY.

Rust and swift join by reading the same YAML once #29 lands there; this file is
then the contract the ports are graded against.
"""

from __future__ import annotations

import asyncio
import json
import types
from pathlib import Path

import pytest
from fastapi import FastAPI

from _testkit import boot_root
from _testkit_relay import _pair as relay_pair
from io_bridge import MemoryTransport
from io_bridge import _engine
from web_rest.tools import _make_post_endpoint
from kernel import register_unlisted

# THE SHARED CORPUS, at the repo root — rust, swift and ts drivers read this
# same file. It is JSON so every runtime parses it with what it already has (the
# ts kernel ships zero runtime deps); JSON is valid YAML, so nothing was lost in
# the move from `c21_cases.yaml`.
_CORPUS = (
    Path(__file__).resolve().parents[5] / "contract" / "conformance" / "cases.json"
)
_DOC = json.loads(_CORPUS.read_text())
# `$root` means THIS runtime's root id; each runner substitutes its own.
ROOT_ID = "kernel_state"


def _subst_root(v):
    if isinstance(v, dict):
        return {k: _subst_root(x) for k, x in v.items()}
    if isinstance(v, list):
        return [_subst_root(x) for x in v]
    return ROOT_ID if v == "$root" else v


CASES = [_subst_root(c) for c in _DOC["cases"]]

# ─── the probe bundle: deterministic, storeless ──────────────────

_echo = types.ModuleType("c21_echo.tools")


async def _echo_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {
            "id": id,
            "sentence": "C21 probe — answers `poke` identically on every transport.",
        }
    if verb == "poke":
        return {"poked": payload.get("value")}
    from kernel._refusal import UNHANDLED, refusal

    return refusal(UNHANDLED, f"c21_probe has no verb {verb!r}", verb=verb)


_echo.handler = _echo_handler
_echo.VERBS = {}
register_unlisted("c21_echo.tools", _echo)


def _norm(reply):
    """What every wire does anyway — a JSON round-trip — applied to ALL legs so
    in-proc python objects and parsed wire bodies compare on equal terms."""
    return json.loads(json.dumps(reply, default=str, sort_keys=True))


def _subset(expected, actual) -> bool:
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(
            k in actual and _subset(v, actual[k]) for k, v in expected.items()
        )
    return expected == actual


# ─── the fixture: ONE target kernel, every leg wired to it ───────


@pytest.fixture
async def wired(tmp_path, monkeypatch):
    a_dir, b_dir = tmp_path / "a", tmp_path / "b"
    a_dir.mkdir(), b_dir.mkdir()
    monkeypatch.chdir(a_dir)
    ka = boot_root()  # scratch kernel: the ws bridge's CALLING side
    monkeypatch.chdir(b_dir)
    kb = boot_root()  # THE target kernel — every transport dispatches here
    ka.update(ka.id, guid="c21-a")
    kb.update(kb.id, guid="c21-b")

    async def send1(k, t, p):
        return (await k.send([{"target": t, "payload": p}]))[0]["result"]

    # ws bridge pair over MemoryTransport (the io_bridge frame path — the same
    # engine the relay and real WS ride).
    rec_a = await send1(
        ka,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "PLACEHOLDER",
        },
    )
    rec_b = await send1(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": rec_a["id"],
            "ingress_rule": "allow_all_ingress",
            # The serving leg DISPATCHES AS ITSELF — same reach as every other leg.
            "can_call": {"c21_probe": "*", "kernel_state": "*"},
        },
    )
    ka.update(rec_a["id"], peer_id=rec_b["id"])
    mt_a, mt_b = MemoryTransport.pair()
    _engine._test_transport_inject[rec_a["id"]] = mt_a
    _engine._test_transport_inject[rec_b["id"]] = mt_b
    assert (await send1(ka, rec_a["id"], {"type": "boot"})).get("booted") is True
    assert (await send1(kb, rec_b["id"], {"type": "boot"})).get("booted") is True

    # rest leg on the target kernel.
    rest = await send1(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "web_rest.tools",
            "id": "c21_rest",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"c21_probe": "*", "kernel_state": "*"},
        },
    )

    # exec guest leg on the target kernel.
    xrec = await send1(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "exec_bridge.tools",
            "id": "c21_exec",
            "ingress_rule": "allow_all_ingress",
            **__import__("_testkit").py_runner_meta(),
            "can_call": {"c21_probe": "*", "kernel_state": "*"},
        },
    )

    # relay tunnel pair (the io_bridge frames over the relay transport).
    rel_a = await send1(
        ka,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "relay_connector.tools",
            "transport": "memory",
            "relay_url": "ws://relay",
            "guid": "A",
            "partner_guid": "B",
        },
    )
    rel_b = await send1(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "relay_connector.tools",
            "transport": "memory",
            "relay_url": "ws://relay",
            "guid": "B",
            "partner_guid": "A",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"c21_probe": "*", "kernel_state": "*"},
        },
    )
    rt_a, rt_b = relay_pair("A", "B")
    _engine._test_transport_inject[rel_a["id"]] = rt_a
    _engine._test_transport_inject[rel_b["id"]] = rt_b
    assert (await send1(ka, rel_a["id"], {"type": "boot"})).get("booted") is True
    assert (await send1(kb, rel_b["id"], {"type": "boot"})).get("booted") is True

    # THE probe — created LAST so its ingress can name every attributed leg.
    await send1(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "c21_echo.tools",
            "id": "c21_probe",
            "callable_by": {
                rest["id"]: "*",
                xrec["id"]: "*",
                # the bridge legs serve A's calls AS THEMSELVES
                rec_b["id"]: "*",
                rel_b["id"]: "*",
            },
        },
    )

    # B's root accepts every attributed leg (the kernel verbs take the two-sided
    # check against the root, as any call does)
    kb.update(
        kb.id,
        callable_by={
            **kb.ctx.agents[kb.id]._meta["callable_by"],
            rest["id"]: "*",
            xrec["id"]: "*",
            rec_b["id"]: "*",
            rel_b["id"]: "*",
        },
    )
    return ka, kb, rec_a["id"], rel_a["id"]


# ─── the four legs, one signature each: cases -> [normalized reply] ───


async def _leg_inproc(ka, kb, bridge_a, relay_a):
    out = []
    for c in CASES:
        item = {"target": c["target"], "payload": c["payload"]}
        o = (await kb.send([item]))[0]
        out.append(_norm(o["result"]))
    return out


async def _forward_leg(ka, leg_id):
    out = []
    for c in CASES:
        item = {
            "target": leg_id,
            "payload": {
                "type": "forward",
                "target": c["target"],
                "payload": c["payload"],
            },
        }
        r = (await ka.send([item]))[0]["result"]
        out.append(_norm(r))
    return out


async def _leg_ws(ka, kb, bridge_a, relay_a):
    return await _forward_leg(ka, bridge_a)


async def _leg_rest(ka, kb, bridge_a, relay_a):
    import httpx

    app = FastAPI()
    app.add_api_route(
        "/c21_rest/{target_id}",
        _make_post_endpoint("c21_rest", kb),
        methods=["POST"],
    )
    out = []
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://c21"
    ) as client:
        for c in CASES:
            r = await client.post(f"/c21_rest/{c['target']}", json=c["payload"])
            out.append(_norm(r.json()))
    return out


async def _leg_exec(ka, kb, bridge_a, relay_a):
    lines = "\n".join(
        f"print(__import__('json').dumps(kernel.send("
        f"{c['target']!r}, {c['payload']!r}), default=str), flush=True)"
        for c in CASES
    )
    r = (
        await kb.send(
            [
                {
                    "target": "c21_exec",
                    "payload": __import__("_testkit").py_start_payload(lines),
                }
            ]
        )
    )[0]["result"]
    assert r.get("job_id"), f"exec leg refused: {r!r}"
    for _ in range(40):
        await asyncio.sleep(0.25)
        js = (
            await kb.send(
                [
                    {
                        "target": "c21_exec",
                        "payload": {"type": "status", "job_id": r["job_id"]},
                    }
                ]
            )
        )[0]["result"]
        row = (js.get("jobs") or [{}])[0]
        if row.get("status") == "done":
            break
    assert row.get("status") == "done", f"exec job never finished: {row!r}"
    tail = (
        await kb.send(
            [
                {
                    "target": "c21_exec",
                    "payload": {
                        "type": "tail",
                        "job_id": r["job_id"],
                        "lines": 500,
                        "stream": "stdout",
                    },
                }
            ]
        )
    )[0]["result"]
    stdout = [ln["line"] for ln in tail.get("lines") or [] if ln["line"].strip()]
    assert len(stdout) == len(CASES), f"exec emitted {len(stdout)} lines: {stdout!r}"
    return [_norm(json.loads(ln)) for ln in stdout]


async def _leg_relay(ka, kb, bridge_a, relay_a):
    return await _forward_leg(ka, relay_a)


LEGS = {
    "in-proc": _leg_inproc,
    "ws-bridge": _leg_ws,
    "web-rest": _leg_rest,
    "exec-guest": _leg_exec,
    "relay-tunnel": _leg_relay,
}


async def test_every_transport_gives_the_same_reply(wired):
    ka, kb, bridge_a, relay_a = wired
    replies = {name: await leg(ka, kb, bridge_a, relay_a) for name, leg in LEGS.items()}

    failures = []
    ref_name = "in-proc"
    for i, case in enumerate(CASES):
        ref = replies[ref_name][i]
        legs = list(LEGS)
        # per-case expectations — a wrong answer fails even if every transport
        # agrees on it
        for name in legs:
            got = replies[name][i]
            for key in case.get("expect_keys", []):
                if not (isinstance(got, dict) and key in got):
                    failures.append(
                        f"{case['name']}: {name} missing key {key!r}: {got}"
                    )
            if "expect" in case and not _subset(case["expect"], got):
                failures.append(
                    f"{case['name']}: {name}: expect {case['expect']} ⊄ {got}"
                )
        # the C21 core: pairwise identity against the in-proc reference —
        # except cases marked `pairwise: false` (a viewer-DEPENDENT reply, like
        # the origin-scoped ledger, is checked for shape on every leg instead)
        if case.get("pairwise") is False:
            continue
        for name in legs:
            if name == ref_name:
                continue
            got = replies[name][i]
            if got != ref:
                failures.append(
                    f"{case['name']}: {name} diverges from {ref_name}:\n"
                    f"    {ref_name}: {json.dumps(ref)[:300]}\n"
                    f"    {name}: {json.dumps(got)[:300]}"
                )
    assert not failures, "\n".join(failures)
