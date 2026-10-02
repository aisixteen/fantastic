"""SECRET MARKS CROSS EVERY BRIDGE (owner-approved, 2026-10-02).

A batch item's `secret` paths (`kernel/_secret.py`) used to stop at the edge of
the kernel that held them: a call frame carried no marks, so the RECEIVING kernel
recorded the value verbatim (call/settle rows, ledger, state stream, memory
store), and a credential store on the far side refused the call outright. Now the
marks ride the call:

  * a ws / bridge `call` frame carries `secret: [paths]` beside `payload`, and
    every inbound leg (web_ws, the io_bridge engine serving ws_bridge peers) puts
    it on the dispatched item verbatim;
  * web_rest reads it from `X-Fantastic-Secret`, a JSON array;
  * outbound, a `<guid>|<agent>` send hands its marks to the peer route, and
    `forward` re-roots the marks on its own call (`payload.payload.<f>` →
    `payload.<f>`, `result[.<f>]` as is).

A RELAY leg is SEALED for secrets (owner, 2026-10-02): relayd records the frames
it routes unmasked, so `relay_connector` refuses any call carrying marks, outbound
(`forward`) and inbound (a tunneled `call` frame with `secret`) — section (g).

Two in-process kernels per test, joined by the real legs over MemoryTransport,
each with a memory store, so every record a kernel keeps is checked.
"""

from __future__ import annotations

import asyncio
import json
import logging
import sys
import types

import httpx
import pytest
from fastapi import FastAPI
from starlette.websockets import WebSocketState

from _testkit import boot_root
from _testkit_relay import _pair as relay_pair
from io_bridge import (
    ConnectionClosed,
    MemoryTransport,
    _bridges,
    _test_transport_inject,
    decode_frame,
    encode_frame,
    forwarded_secret,
)
from kernel._secret import PLACEHOLDER
from relay_connector._relay import SECRET_SEALED, SECRET_SEALED_HINT
from web_rest.tools import _make_post_endpoint
from web_ws._proxy import run as ws_run

KEY = "sk-cross-0123456789abcdef"
ISSUED = "tok-cross-issued-fedcba98"

# ─── the probe the far kernel serves ─────────────────────────────────────────

_got: list[dict] = []


async def _vault(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "cross-kernel secret probe", "verbs": {}}
    _got.append(payload)
    if verb == "set_key":
        return {"ok": True, "len": len(payload["key"])}
    if verb == "issue":
        return {"key": ISSUED, "note": "issued"}
    return {"error": f"unknown verb {verb!r}"}


_mod = types.ModuleType("xsecret_probe.tools")
_mod.handler = _vault
_mod.VERBS = {}
sys.modules["xsecret_probe.tools"] = _mod


# ─── two kernels, each keeping every record ──────────────────────────────────


async def _op(k, target, payload, **item):
    return (await k.send([{"target": target, "payload": payload, **item}]))[0]


async def _kernel(path, monkeypatch, guid: str, store: str):
    path.mkdir()
    monkeypatch.chdir(path)
    k = boot_root()
    k.update(k.id, guid=guid)
    made = await _op(
        k,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "ram_store.tools",
            "id": store,
            "operator": True,
            "admins": ["kernel"],
        },
    )
    assert made["status"] == "completed", made
    k.ctx.update(k.id, memory_store_id=store)
    out = await k.ctx.start(boot_all=False)
    assert out.get("started"), out
    k._tap = []  # every state event this kernel publishes
    k.ctx.add_state_subscriber(k._tap.append)
    k._store = store
    return k


async def _records(k) -> dict:
    """Everything the kernel KEEPS of its calls: the rows in RAM, the ledger
    projection, every state event, then — after a flush — the memory store."""
    rows = [dict(r) for r in k.ctx.rows.values()]
    ledger = await k.ctx.ledger_entries()
    await k.ctx.flush_executions()
    tables = (await _op(k, k._store, {"type": "dump"}))["result"]["tables"]
    return {"rows": rows, "ledger": ledger, "events": list(k._tap), "store": tables}


def _assert_kept_out(k, recs: dict, *values: str) -> None:
    for where, data in recs.items():
        for v in values:
            assert v not in repr(data), (
                f"{k.id}@{k.ctx.root._meta.get('guid')}: {v!r} in {where}"
            )


def _assert_not_printed(capsys, caplog, *values: str) -> None:
    """(f) nothing on the way printed or logged a value."""
    printed = capsys.readouterr()
    for v in values:
        assert v not in printed.out and v not in printed.err, v
        assert v not in caplog.text, v


def _call_row(recs: dict, agent: str, verb: str) -> dict:
    rows = [
        r
        for r in recs["rows"]
        if r.get("kind") == "call" and r.get("agent") == agent and r.get("verb") == verb
    ]
    assert len(rows) == 1, rows
    return rows[0]


def _settle_of(recs: dict, call_id: int) -> dict:
    rows = [
        r
        for r in recs["rows"]
        if r.get("kind") == "settle" and r.get("parent") == call_id
    ]
    assert len(rows) == 1, rows
    return rows[0]


def _vault_on(k, *callers: str) -> None:
    k.create(
        "xsecret_probe.tools",
        id="vault",
        callable_by={c: "*" for c in callers},
    )


async def _bridge_pair(ka, kb, bundle: str) -> str:
    """A's leg dials, B's leg SERVES (pinned to A, open, reaching `vault`). Same
    engine for both bundles; the relay tunnels the frames through a fake hub."""
    extra_a: dict = {"peer_id": "B"}
    extra_b: dict = {"peer_id": "A"}
    if bundle == "relay_connector.tools":
        extra_a = {"relay_url": "ws://relay", "guid": "RA", "partner_guid": "RB"}
        extra_b = {"relay_url": "ws://relay", "guid": "RB", "partner_guid": "RA"}
    a = await _op(
        ka,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": ka.id,
            "handler_module": bundle,
            "transport": "memory",
            **extra_a,
        },
    )
    b = await _op(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": kb.id,
            "handler_module": bundle,
            "transport": "memory",
            "peer_guid": "A-g",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"vault": "*"},
            **extra_b,
        },
    )
    a_id, b_id = a["result"]["id"], b["result"]["id"]
    ta, tb = (
        relay_pair("RA", "RB")
        if bundle == "relay_connector.tools"
        else MemoryTransport.pair()
    )
    _test_transport_inject[a_id] = ta
    _test_transport_inject[b_id] = tb
    for k, i in ((ka, a_id), (kb, b_id)):
        assert (await _op(k, i, {"type": "boot"}))["result"].get("booted") is True
    return a_id


class _WsEnd:
    """kernel H's web_ws socket whose far end is a bridge leg's MemoryTransport
    half — the production topology (a bridge dialing a web_ws) minus the network.
    Every frame goes through the real wire codec; `wire` keeps what crossed (the
    TEST's capture, not anything the kernel keeps)."""

    def __init__(self, mt: MemoryTransport):
        self.application_state = WebSocketState.CONNECTED
        self.mt = mt
        self.wire: list[dict] = []

    async def receive(self):
        try:
            frame = await self.mt.recv()
        except ConnectionClosed:
            return {"type": "websocket.disconnect"}
        raw, is_bin = encode_frame(frame)
        if is_bin:
            return {"type": "websocket.receive", "bytes": raw}
        return {"type": "websocket.receive", "text": raw.decode("utf-8")}

    async def _out(self, raw):
        frame = decode_frame(raw)
        self.wire.append(frame)
        await self.mt.send(frame)

    async def send_text(self, text):
        await self._out(text)

    async def send_bytes(self, data):
        await self._out(data)


async def _until(pred, secs=3.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0.01)


@pytest.fixture
async def two(tmp_path, monkeypatch):
    _got.clear()
    ka = await _kernel(tmp_path / "a", monkeypatch, "A-g", "a_mem")
    kb = await _kernel(tmp_path / "b", monkeypatch, "B-g", "b_mem")
    monkeypatch.chdir(tmp_path)
    yield ka, kb
    _bridges.clear()
    _test_transport_inject.clear()


# ─── the forward mapping ─────────────────────────────────────────────────────


def test_forward_re_roots_the_marks_at_the_forwarded_item():
    assert forwarded_secret(
        (
            "payload.payload.key",
            "payload.payload.fields.api_key",
            "result",
            "result.key",
            "payload.target",
            "payload.timeout",
            "payload.payload",
        )
    ) == ["payload.key", "payload.fields.api_key", "result", "result.key", "payload"]
    assert forwarded_secret(()) == []


# ─── (a) a payload mark across a bridge ──────────────────────────────────────


# relay_connector is SEALED for secrets — see (g).
@pytest.mark.parametrize("bundle", ["ws_bridge.tools"])
async def test_a_marked_payload_crosses_a_bridge_and_both_kernels_mask_it(two, bundle):
    ka, kb = two
    _vault_on(kb, "A-g|*")
    a_leg = await _bridge_pair(ka, kb, bundle)
    out = await _op(
        ka,
        a_leg,
        {
            "type": "forward",
            "target": "vault",
            "payload": {"type": "set_key", "ref": "openai", "key": KEY},
        },
        secret=["payload.payload.key"],
    )
    # the far TARGET got the value; the CALLER the real reply
    assert _got == [{"type": "set_key", "ref": "openai", "key": KEY}], _got
    assert out["status"] == "completed" and out["result"] == {
        "ok": True,
        "len": len(KEY),
    }, out
    # the RECEIVING kernel: its call row is masked and names the paths
    rb = await _records(kb)
    row = _call_row(rb, "vault", "set_key")
    assert row["payload"] == {"type": "set_key", "ref": "openai", "key": PLACEHOLDER}
    assert row["secret"] == ["payload.key"], row
    _assert_kept_out(kb, rb, KEY)
    # the calling kernel masks its own record of the forward
    ra = await _records(ka)
    fwd = _call_row(ra, a_leg, "forward")
    assert fwd["payload"]["payload"]["key"] == PLACEHOLDER, fwd
    _assert_kept_out(ka, ra, KEY)


async def test_a_bridge_dialing_web_ws_carries_the_marks(two, capsys, caplog):
    """The production ws topology: A's ws_bridge dials B's web_ws leg."""
    ka, kb = two
    caplog.set_level(logging.DEBUG)
    kb.create(
        "web_ws.tools",
        id="b_ws",
        ingress_rule="allow_all_ingress",
        can_call={"vault": "*"},
    )
    _vault_on(kb, "b_ws")
    a = await _op(
        ka,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": ka.id,
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": kb.id,
        },
    )
    a_leg = a["result"]["id"]
    mt_a, mt_b = MemoryTransport.pair()
    _test_transport_inject[a_leg] = mt_a
    sock = _WsEnd(mt_b)
    serving = asyncio.create_task(ws_run(sock, kb, kb.id, "b_ws"))
    try:
        assert (await _op(ka, a_leg, {"type": "boot"}))["result"].get("booted")
        out = await _op(
            ka,
            a_leg,
            {
                "type": "forward",
                "target": "vault",
                "payload": {"type": "issue"},
            },
            secret=["result.key"],
        )
        assert out["result"] == {"key": ISSUED, "note": "issued"}, out
        # the frame web_ws received named the call — the settle keeps its whole
        # result — and carried the mark, so B records the placeholder
        rb = await _records(kb)
        row = _call_row(rb, "vault", "issue")
        assert row["secret"] == ["result.key"] and row["named"] is True, row
        assert _settle_of(rb, row["id"])["outcome"]["result"] == {
            "key": PLACEHOLDER,
            "note": "issued",
        }
        _assert_kept_out(kb, rb, ISSUED)
        _assert_kept_out(ka, await _records(ka), ISSUED)
        _assert_not_printed(capsys, caplog, ISSUED)
    finally:
        await mt_a.close()
        await asyncio.wait_for(serving, 3)


# ─── (b) a result mark masks the RECEIVING settle row ────────────────────────


# relay_connector is SEALED for secrets — see (g).
@pytest.mark.parametrize("bundle", ["ws_bridge.tools"])
async def test_a_result_mark_masks_the_receiving_settle_row(two, bundle):
    ka, kb = two
    _vault_on(kb, "A-g|*")
    a_leg = await _bridge_pair(ka, kb, bundle)
    whole = await _op(
        ka,
        a_leg,
        {"type": "forward", "target": "vault", "payload": {"type": "issue"}},
        secret=["result"],
    )
    assert whole["result"] == {"key": ISSUED, "note": "issued"}, whole
    rb = await _records(kb)
    row = _call_row(rb, "vault", "issue")
    assert row["secret"] == ["result"], row
    # the frame id NAMES the far execution, so its settle keeps the whole
    # result — the one place the value would have landed
    assert _settle_of(rb, row["id"])["outcome"]["result"] == PLACEHOLDER
    _assert_kept_out(kb, rb, ISSUED)
    _assert_kept_out(ka, await _records(ka), ISSUED)


async def test_without_a_mark_the_receiving_settle_keeps_the_value(two):
    """The control: what the marks prevent is real."""
    ka, kb = two
    _vault_on(kb, "A-g|*")
    a_leg = await _bridge_pair(ka, kb, "ws_bridge.tools")
    await _op(
        ka, a_leg, {"type": "forward", "target": "vault", "payload": {"type": "issue"}}
    )
    rb = await _records(kb)
    row = _call_row(rb, "vault", "issue")
    assert "secret" not in row
    assert _settle_of(rb, row["id"])["outcome"]["result"]["key"] == ISSUED


# ─── (c) a malformed mark is refused before dispatch ─────────────────────────

_BAD = [
    ("payload.key", "must be a non-empty list of paths"),
    ([], "must be a non-empty list of paths"),
    (["payload.nope"], "names nothing in the payload"),
    (["payload.type"], "cannot hide payload.type"),
    (["payload"], "must start with 'payload.'"),
]


async def test_a_malformed_mark_on_a_bridge_frame_is_refused_before_dispatch(two):
    ka, kb = two
    _vault_on(kb, "A-g|*")
    b = await _op(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": kb.id,
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "A",
            "peer_guid": "A-g",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"vault": "*"},
        },
    )
    mt_peer, mt_b = MemoryTransport.pair()
    _test_transport_inject[b["result"]["id"]] = mt_b
    assert (await _op(kb, b["result"]["id"], {"type": "boot"}))["result"]["booted"]
    for n, (secret, why) in enumerate(_BAD):
        await mt_peer.send(
            {
                "type": "call",
                "id": f"bad-{n}",
                "target": "vault",
                "payload": {"type": "set_key", "key": KEY},
                "caller": "A-g|kernel",
                "secret": secret,
            }
        )
        data = (await asyncio.wait_for(mt_peer.recv(), 3))["data"]
        assert (data["reason"], data["arg"]) == ("bad_arg", "secret"), (secret, data)
        assert why in data["error"] and KEY not in data["error"], (secret, data)
    assert _got == [], "a refused item must not reach the handler"
    rb = await _records(kb)
    assert not [r for r in rb["rows"] if r.get("agent") == "vault"], rb["rows"]
    _assert_kept_out(kb, rb, KEY)


async def test_a_malformed_mark_on_a_web_ws_frame_is_refused_before_dispatch(two):
    _, kb = two
    kb.create(
        "web_ws.tools",
        id="b_ws",
        ingress_rule="allow_all_ingress",
        can_call={"vault": "*"},
    )
    _vault_on(kb, "b_ws")
    mt_peer, mt_b = MemoryTransport.pair()
    serving = asyncio.create_task(ws_run(_WsEnd(mt_b), kb, kb.id, "b_ws"))
    try:
        for n, (secret, why) in enumerate(_BAD):
            await mt_peer.send(
                {
                    "type": "call",
                    "id": f"bad-{n}",
                    "target": "vault",
                    "payload": {"type": "set_key", "key": KEY},
                    "secret": secret,
                }
            )
            while True:
                f = await asyncio.wait_for(mt_peer.recv(), 3)
                if f.get("type") == "reply":
                    break
            data = f["data"]
            assert (data["reason"], data["arg"]) == ("bad_arg", "secret"), data
            assert why in data["error"], (secret, data)
        assert _got == []
    finally:
        await mt_peer.close()
        await asyncio.wait_for(serving, 3)


@pytest.mark.parametrize(
    "secret, why",
    [
        (["payload.payload"], "must start with 'payload.'"),
        (["payload.payload.type"], "cannot hide payload.type"),
    ],
)
async def test_a_forward_mark_that_maps_to_nothing_valid_is_the_far_kernels_refusal(
    two, secret, why
):
    ka, kb = two
    _vault_on(kb, "A-g|*")
    a_leg = await _bridge_pair(ka, kb, "ws_bridge.tools")
    out = await _op(
        ka,
        a_leg,
        {
            "type": "forward",
            "target": "vault",
            "payload": {"type": "set_key", "key": KEY},
        },
        secret=secret,
    )
    r = out["result"]
    assert (r["reason"], r["arg"]) == ("bad_arg", "secret") and why in r["error"], r
    assert _got == []


# ─── (d) web_rest: the X-Fantastic-Secret header ─────────────────────────────


async def test_web_rest_carries_the_marks_in_a_header(two, capsys, caplog):
    _, kb = two
    caplog.set_level(logging.DEBUG)
    kb.create(
        "web_rest.tools",
        id="b_rest",
        ingress_rule="allow_all_ingress",
        can_call={"vault": "*"},
    )
    _vault_on(kb, "b_rest")
    app = FastAPI()
    app.add_api_route(
        "/b_rest/{target_id}", _make_post_endpoint("b_rest", kb), methods=["POST"]
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://h"
    ) as c:
        r = await c.post(
            "/b_rest/vault",
            json={"type": "set_key", "key": KEY},
            headers={"X-Fantastic-Secret": json.dumps(["payload.key"])},
        )
        assert r.status_code == 200 and r.json() == {"ok": True, "len": len(KEY)}
        r = await c.post(
            "/b_rest/vault",
            json={"type": "issue"},
            headers={
                "X-Fantastic-Secret": '["result.key"]',
                "X-Fantastic-Execution-Id": "issue-rest-1",
            },
        )
        assert r.json() == {"key": ISSUED, "note": "issued"}, r.text
        # a header that is not JSON is the leg's own 400 — nothing dispatched
        r = await c.post(
            "/b_rest/vault",
            json={"type": "set_key", "key": KEY},
            headers={"X-Fantastic-Secret": "payload.key"},
        )
        assert r.status_code == 400, r.text
        assert r.json() == {"error": "web_rest: X-Fantastic-Secret is not valid JSON"}
        # JSON of the wrong shape is the kernel's own refusal
        r = await c.post(
            "/b_rest/vault",
            json={"type": "set_key", "key": KEY},
            headers={"X-Fantastic-Secret": '"payload.key"'},
        )
        body = r.json()
        assert (body["reason"], body["arg"]) == ("bad_arg", "secret"), body
    assert _got == [{"type": "set_key", "key": KEY}, {"type": "issue"}], _got
    rb = await _records(kb)
    assert _call_row(rb, "vault", "set_key")["payload"]["key"] == PLACEHOLDER
    issue = _call_row(rb, "vault", "issue")
    assert _settle_of(rb, issue["id"])["outcome"]["result"]["key"] == PLACEHOLDER
    _assert_kept_out(kb, rb, KEY, ISSUED)
    _assert_not_printed(capsys, caplog, KEY, ISSUED)


# ─── (e) <guid>|creds from another kernel ────────────────────────────────────


async def _page_peer(kh, kp, creds_file):
    """H serves a web_ws leg; P is a kernel connected to it (its bridge leg is
    the socket's far end) and says `hello` — from then on H reaches P's agents as
    `P-g|<agent>`. P serves a credential_store that H may create refs in."""
    kh.create("web_ws.tools", id="h_ws", ingress_rule="allow_all_ingress")
    p = await _op(
        kp,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": kp.id,
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": kh.id,
            "peer_guid": "H-g",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"p_creds": "*"},
        },
    )
    made = kp.create(
        "credential_store.tools",
        id="p_creds",
        file=str(creds_file),
        creators=["H-g|kernel"],
        callable_by={"H-g|kernel": "*"},
    )
    assert made.get("id") == "p_creds", made
    mt_h, mt_p = MemoryTransport.pair()
    _test_transport_inject[p["result"]["id"]] = mt_p
    sock = _WsEnd(mt_h)
    serving = asyncio.create_task(ws_run(sock, kh, kh.id, "h_ws"))
    assert (await _op(kp, p["result"]["id"], {"type": "boot"}))["result"]["booted"]
    await mt_p.send({"type": "hello", "guid": "P-g"})
    await _until(lambda: "P-g" in kh.ctx.peers)
    return sock, serving, mt_p


@pytest.fixture
async def host_and_page(tmp_path, monkeypatch):
    _got.clear()
    kh = await _kernel(tmp_path / "h", monkeypatch, "H-g", "h_mem")
    kp = await _kernel(tmp_path / "p", monkeypatch, "P-g", "p_mem")
    monkeypatch.chdir(tmp_path)
    yield kh, kp, tmp_path / "creds.yaml"
    _bridges.clear()
    _test_transport_inject.clear()


async def test_a_guid_qualified_credential_set_and_get_cross_with_their_marks(
    host_and_page, capsys, caplog
):
    kh, kp, creds_file = host_and_page
    caplog.set_level(logging.DEBUG)
    sock, serving, mt_p = await _page_peer(kh, kp, creds_file)
    try:
        put = await _op(
            kh,
            "P-g|p_creds",
            {
                "type": "set",
                "ref": "openai/main",
                "host": "https://api.openai.com/v1",
                "fields": {"api_key": KEY},
            },
            secret=["payload.fields"],
        )
        assert put["status"] == "completed", put
        assert put["result"] == {
            "ref": "openai/main",
            "host": "api.openai.com",
            "fields": ["api_key"],
            "stored": True,
        }, put
        got = await _op(
            kh, "P-g|p_creds", {"type": "get", "ref": "openai/main"}, secret=["result"]
        )
        assert got["result"]["fields"] == {"api_key": KEY}, got
        # the frames H sent carried the item's marks verbatim
        calls = [f for f in sock.wire if f.get("type") == "call"]
        assert [f.get("secret") for f in calls] == [["payload.fields"], ["result"]]
        # the value lives in P's backend, and in NO record on either side
        assert KEY in creds_file.read_text()
        rp = await _records(kp)
        assert _call_row(rp, "p_creds", "set")["payload"]["fields"] == PLACEHOLDER
        get_row = _call_row(rp, "p_creds", "get")
        assert _settle_of(rp, get_row["id"])["outcome"]["result"] == PLACEHOLDER
        _assert_kept_out(kp, rp, KEY)
        rh = await _records(kh)
        assert _call_row(rh, "P-g|p_creds", "set")["payload"]["fields"] == PLACEHOLDER
        _assert_kept_out(kh, rh, KEY)
        # an UNMARKED set from H is refused by P's store, as for a local caller
        bare = await _op(
            kh,
            "P-g|p_creds",
            {
                "type": "set",
                "ref": "openai/main",
                "host": "api.openai.com",
                "fields": {"api_key": "sk-unmarked-x"},
            },
        )
        assert bare["result"]["arg"] == "secret", bare
        # (f) and nothing on the way printed or logged it
        _assert_not_printed(capsys, caplog, KEY)
    finally:
        await mt_p.close()
        await asyncio.wait_for(serving, 3)


# ─── (f) no leg, transport or bridge keeps or prints the unmasked frame ──────


# relay_connector is SEALED for secrets — see (g).
@pytest.mark.parametrize("bundle", ["ws_bridge.tools"])
async def test_no_leg_or_bridge_prints_logs_or_mirrors_the_value(
    two, bundle, capsys, caplog
):
    ka, kb = two
    caplog.set_level(logging.DEBUG)
    _vault_on(kb, "A-g|*")
    a_leg = await _bridge_pair(ka, kb, bundle)
    b_leg = next(a for a in _bridges if a != a_leg)
    # watchers on every agent the call passes: both legs and the far target
    for k, aid in ((ka, a_leg), (kb, b_leg), (kb, "vault")):
        k.ctx.ensure_inbox(f"spy_{aid}")
        assert k.watch(aid, f"spy_{aid}") is None
    out = await _op(
        ka,
        a_leg,
        {
            "type": "forward",
            "target": "vault",
            "payload": {"type": "set_key", "key": KEY},
        },
        secret=["payload.payload.key", "result"],
    )
    assert out["result"] == {"ok": True, "len": len(KEY)}, out
    for k, aid in ((ka, a_leg), (kb, b_leg), (kb, "vault")):
        q = k.ctx.inboxes[f"spy_{aid}"]
        mirrored = [q.get_nowait() for _ in range(q.qsize())]
        assert KEY not in repr(mirrored), (aid, mirrored)
    # the bridge state holds no frame once the call settled
    for st in _bridges.values():
        assert not st.pending and KEY not in repr(st.extra)
    _assert_kept_out(ka, await _records(ka), KEY)
    _assert_kept_out(kb, await _records(kb), KEY)
    _assert_not_printed(capsys, caplog, KEY)


# ─── (g) the relay seal ──────────────────────────────────────────────────────

_SEALED = {"error": SECRET_SEALED, "reason": "unauthorized", "hint": SECRET_SEALED_HINT}


@pytest.mark.parametrize(
    "secret", [["payload.payload.key"], ["result"], ["payload.timeout"]]
)
async def test_the_relay_refuses_a_marked_forward_before_a_frame_leaves(two, secret):
    """Outbound: ANY mark on the forward call — one that would cross, a result
    mark, even one on forward's own field — is refused; nothing reaches the far
    kernel, and the caller's own records stay masked."""
    ka, kb = two
    _vault_on(kb, "A-g|*")
    a_leg = await _bridge_pair(ka, kb, "relay_connector.tools")
    out = await _op(
        ka,
        a_leg,
        {
            "type": "forward",
            "target": "vault",
            "payload": {"type": "set_key", "key": KEY},
            "timeout": 5,
        },
        secret=secret,
    )
    assert out["status"] == "errored" and out["result"] == _SEALED, out
    assert _got == [], "nothing crossed the relay"
    rb = await _records(kb)
    assert not [r for r in rb["rows"] if r.get("agent") == "vault"], rb["rows"]
    if secret != ["result"]:
        _assert_kept_out(ka, await _records(ka), KEY)


async def test_the_relay_refuses_an_inbound_marked_frame_and_dispatches_nothing(two):
    """Inbound: a tunneled `call` frame that names `secret` is answered with the
    seal's refusal; the far target never runs and the serving kernel records no
    call to it."""
    _, kb = two
    _vault_on(kb, "A-g|*")
    b = await _op(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": kb.id,
            "handler_module": "relay_connector.tools",
            "transport": "memory",
            "relay_url": "ws://relay",
            "guid": "RB",
            "partner_guid": "RA",
            "peer_guid": "A-g",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"vault": "*"},
        },
    )
    ta, tb = relay_pair("RA", "RB")
    _test_transport_inject[b["result"]["id"]] = tb
    assert (await _op(kb, b["result"]["id"], {"type": "boot"}))["result"]["booted"]
    await ta.send(
        {
            "type": "call",
            "id": "sealed-1",
            "target": "vault",
            "payload": {"type": "set_key", "key": KEY},
            "caller": "A-g|kernel",
            "secret": ["payload.key"],
        }
    )
    reply = await asyncio.wait_for(ta.recv(), 3)
    assert reply == {"type": "reply", "id": "sealed-1", "data": _SEALED}, reply
    # the same leg still serves an UNMARKED call — the seal is about marks only
    await ta.send(
        {
            "type": "call",
            "id": "plain-1",
            "target": "vault",
            "payload": {"type": "issue"},
            "caller": "A-g|kernel",
        }
    )
    reply = await asyncio.wait_for(ta.recv(), 3)
    assert reply["data"] == {"key": ISSUED, "note": "issued"}, reply
    assert _got == [{"type": "issue"}], _got
    rb = await _records(kb)
    assert [r.get("verb") for r in rb["rows"] if r.get("agent") == "vault"] == ["issue"]
    _assert_kept_out(kb, rb, KEY)
    await ta.close()
