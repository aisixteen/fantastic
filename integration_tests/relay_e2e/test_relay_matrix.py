"""Kernels-through-relay e2e — the any-to-any + same-kind matrix (relay_connector).

Topology per pair: two host kernels each running a `relay_connector` agent, BOTH
dialling the same relay-KERNEL router (`ws://host/<guid>`, group password in
`X-Fantastic-Auth`). The relay routes by `target`; the connectors TUNNEL
`kernel.send` `call`/`reply` frames to each other by GUID. Done-when: a reflect
forwarded from A round-trips B's root reply through the relay.

The matrix is the 3 host runtimes × 2 (any-to-any + same-kind). Heavy + opt-in;
skips cleanly when `relayd` isn't built.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path
from typing import Callable

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from relay_harness import Relay, require_relay  # noqa: E402

from helpers.kernel_proc import KernelProc  # noqa: E402
from helpers.seeding import seed_web, seed_web_ws  # noqa: E402

# Runtimes that ship a relay_connector transport today.
RELAY_RUNTIMES = {"python", "rust", "swift"}

# `handler_module` for the relay_connector agent — same name across all three host
# runtimes (the relay-kernel-router derivation of io_bridge).
_HANDLER_MODULE = {
    "python": "relay_connector.tools",
    "rust": "relay_connector.tools",
    "swift": "relay_connector.tools",
}

# 3 host canvases × 2 → all unordered pairs (any-to-any) PLUS same-kind.
PAIRS = [
    ("python", "python"),
    ("python", "rust"),
    ("python", "swift"),
    ("rust", "rust"),
    ("rust", "swift"),
    ("swift", "swift"),
]


def _pair_skip_reason(a: str, b: str) -> str | None:
    for rt in (a, b):
        if rt not in RELAY_RUNTIMES:
            return f"relay_connector transport not yet in e2e for {rt}"
    return None


def _relay_meta(
    *,
    handler_module: str,
    guid: str,
    partner_guid: str,
    relay_url: str,
    relay_token: str,
    auth: str | None = None,
) -> dict:
    """The relay_connector agent record. `relay_token` is the relay CONNECTION
    password (X-Fantastic-Auth); the per-leg `auth`/`ingress_rule`/`egress_rule`
    gate the tunneled bridge calls and are INDEPENDENT of it."""
    meta = {
        "handler_module": handler_module,
        "id": "cb",
        "transport": "relay",
        "relay_url": relay_url,
        "guid": guid,
        "partner_guid": partner_guid,
        "relay_token": relay_token,
        "heartbeat": 0,
    }
    # IO legs SEAL by default (absent rule ⇒ deny_inbound); a leg that must answer
    # inbound forwards sets this explicitly (allow_all, or password for the group tests).
    if auth is not None:
        meta["auth"] = auth
    return meta


async def _await_connected(kp: KernelProc, created: list) -> dict:
    """Poll the leg's connectivity flag (some runtimes boot the transport async)."""
    rcb = await kp.call("cb", "reflect")
    for _ in range(50):
        if rcb.get("connected"):
            break
        await asyncio.sleep(0.1)
        rcb = await kp.call("cb", "reflect")
    assert rcb.get("connected") is True, (
        f"A relay_connector not connected after 5s.\n  create: {created}\n  reflect: {rcb}"
    )
    return rcb


async def _spawn_pair(
    rt_a: str,
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
    tag: str,
    *,
    env_a: dict | None = None,
    env_b: dict | None = None,
) -> tuple[KernelProc, KernelProc, tuple, tuple, list[KernelProc]]:
    bin_a = request.getfixturevalue(f"{rt_a}_binary")
    bin_b = request.getfixturevalue(f"{rt_b}_binary")
    base = parity_tmp(tag)
    wd_a, wd_b = base / "A", base / "B"
    wd_a.mkdir(parents=True)
    wd_b.mkdir(parents=True)
    port_a, port_b = free_port(), free_port()
    # web + web_ws so each daemon is drivable over WS (no relay leg persisted →
    # no blocking auto-boot at startup; we create+boot the legs concurrently below).
    for binary, wd, port in [(bin_a, wd_a, port_a), (bin_b, wd_b, port_b)]:
        seed_web(binary, wd, port)
        seed_web_ws(binary, wd)
    spawned: list[KernelProc] = []
    kp_a = bin_a.start_daemon(wd_a, port_a, label=rt_a, extra_env=env_a)
    spawned.append(kp_a)
    await kp_a.wait_ready()
    kp_b = bin_b.start_daemon(wd_b, port_b, label=rt_b, extra_env=env_b)
    spawned.append(kp_b)
    await kp_b.wait_ready()
    return kp_a, kp_b, (bin_a, wd_a), (bin_b, wd_b), spawned


# ── any-to-any + directional deny ───────────────────────────────


@pytest.mark.parametrize("rt_a,rt_b", PAIRS, ids=[f"{a}-{b}" for a, b in PAIRS])
@pytest.mark.asyncio
async def test_relay_any_to_any(
    rt_a: str,
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """Each host-runtime pair round-trips a kernel.send through the relay over
    relay_connector. A is directional (deny_inbound): A→B succeeds (B is
    allow_all); B→A is refused on arrival at A — the cross-runtime wire-shape guard."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)
    relay = Relay(require_relay(), free_port()).start()
    try:
        kp_a, kp_b, _, _, spawned = await _spawn_pair(
            rt_a, rt_b, request, parity_tmp, free_port, f"relay_{rt_a}_{rt_b}"
        )
        try:
            created = await asyncio.gather(
                kp_a.call(
                    "kernel",
                    "create_agent",
                    **_relay_meta(
                        handler_module=_HANDLER_MODULE[rt_a],
                        guid="A",
                        partner_guid="B",
                        relay_url=relay.url,
                        relay_token=relay.password,
                        auth="deny_inbound",
                    ),
                ),
                kp_b.call(
                    "kernel",
                    "create_agent",
                    **_relay_meta(
                        handler_module=_HANDLER_MODULE[rt_b],
                        guid="B",
                        partner_guid="A",
                        relay_url=relay.url,
                        relay_token=relay.password,
                        auth="allow_all",
                    ),
                ),
            )
            rcb = await _await_connected(kp_a, created)
            assert rcb.get("ingress_rule") == "deny_inbound", f"A reflect: {rcb}"

            # A→B: forward a reflect to B's root over the relay; reply round-trips.
            reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
            assert isinstance(reply, dict), f"non-dict reply: {reply!r}"
            assert "error" not in reply, f"forward errored through the relay: {reply}"
            assert "id" in reply and "tree" in reply, (
                f"reply lacks B's reflect: {list(reply.keys())}"
            )

            # B→A: refused on arrival at A's deny_inbound leg (same wire shape everywhere).
            reverse = await kp_b.call("cb", "forward", target="kernel", payload={"type": "reflect"})
            assert reverse.get("reason") == "unauthorized", f"B→A should be denied: {reverse}"
        finally:
            for kp in spawned:
                kp.terminate()
    finally:
        relay.stop()


# ── password group membership ───────────────────────────────────


async def _drive_password(
    rt_a: str, rt_b: str, relay: Relay, kp_a: KernelProc, kp_b: KernelProc, *, expect_denied: bool
) -> None:
    created = await asyncio.gather(
        kp_a.call(
            "kernel",
            "create_agent",
            **_relay_meta(
                handler_module=_HANDLER_MODULE[rt_a],
                guid="A",
                partner_guid="B",
                relay_url=relay.url,
                relay_token=relay.password,
                auth="password",
            ),
        ),
        kp_b.call(
            "kernel",
            "create_agent",
            **_relay_meta(
                handler_module=_HANDLER_MODULE[rt_b],
                guid="B",
                partner_guid="A",
                relay_url=relay.url,
                relay_token=relay.password,
                auth="password",
            ),
        ),
    )
    rcb = await _await_connected(kp_a, created)
    assert rcb.get("ingress_rule") == "password", f"A reflect: {rcb}"

    reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict), f"non-dict reply: {reply!r}"
    if expect_denied:
        assert reply.get("reason") == "unauthorized", f"outsider should be denied: {reply}"
    else:
        assert "id" in reply and "tree" in reply, f"group member A→B failed: {list(reply.keys())}"
        reverse = await kp_b.call("cb", "forward", target="kernel", payload={"type": "reflect"})
        assert isinstance(reverse, dict) and "id" in reverse and "tree" in reverse, (
            f"group member B→A failed: {reverse}"
        )


@pytest.mark.parametrize("rt_a,rt_b", PAIRS, ids=[f"{a}-{b}" for a, b in PAIRS])
@pytest.mark.asyncio
async def test_relay_password_group_member(
    rt_a: str, rt_b: str, request, parity_tmp, free_port
) -> None:
    """Both legs are kernel-group members (`auth="password"`, SAME group token in
    env): the token is presented on each call envelope, survives the relay, and is
    checked on arrival — A→B and B→A both round-trip across every runtime pair."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)
    relay = Relay(require_relay(), free_port()).start()
    try:
        env = {"FANTASTIC_GROUP_TOKEN": "grp-secret"}
        kp_a, kp_b, _, _, spawned = await _spawn_pair(
            rt_a,
            rt_b,
            request,
            parity_tmp,
            free_port,
            f"relaypw_{rt_a}_{rt_b}",
            env_a=env,
            env_b=env,
        )
        try:
            await _drive_password(rt_a, rt_b, relay, kp_a, kp_b, expect_denied=False)
        finally:
            for kp in spawned:
                kp.terminate()
    finally:
        relay.stop()


# B = each runtime in turn (the enforcing receiver); A = python presents a token.
_REJECT_PAIRS = [("python", "python"), ("python", "rust"), ("python", "swift")]


@pytest.mark.parametrize("rt_a,rt_b", _REJECT_PAIRS, ids=[f"{a}-{b}" for a, b in _REJECT_PAIRS])
@pytest.mark.asyncio
async def test_relay_password_rejects_outsider(
    rt_a: str, rt_b: str, request, parity_tmp, free_port
) -> None:
    """A presents a DIFFERENT group token than B expects → B's `password` gate
    refuses A's inbound call `unauthorized`. Each runtime as the enforcing receiver."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)
    relay = Relay(require_relay(), free_port()).start()
    try:
        kp_a, kp_b, _, _, spawned = await _spawn_pair(
            rt_a,
            rt_b,
            request,
            parity_tmp,
            free_port,
            f"relaypwx_{rt_a}_{rt_b}",
            env_a={"FANTASTIC_GROUP_TOKEN": "grp-A"},
            env_b={"FANTASTIC_GROUP_TOKEN": "grp-B"},
        )
        try:
            await _drive_password(rt_a, rt_b, relay, kp_a, kp_b, expect_denied=True)
        finally:
            for kp in spawned:
                kp.terminate()
    finally:
        relay.stop()


# ── asymmetric per-direction rules ──────────────────────────────


@pytest.mark.parametrize("rt_a,rt_b", PAIRS, ids=[f"{a}-{b}" for a, b in PAIRS])
@pytest.mark.asyncio
async def test_relay_asymmetric_rules(rt_a: str, rt_b: str, request, parity_tmp, free_port) -> None:
    """The symmetric SPLIT through the relay: A is a hub (`ingress_rule="deny_inbound"`
    + `egress_rule="password"`), B a group member. A→B round-trips (egress presents,
    ingress checks); B→A is denied (A's ingress). Independent per-direction rules,
    identical wire shape across every runtime pair."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)
    relay = Relay(require_relay(), free_port()).start()
    try:
        env = {"FANTASTIC_GROUP_TOKEN": "fleet"}
        kp_a, kp_b, _, _, spawned = await _spawn_pair(
            rt_a,
            rt_b,
            request,
            parity_tmp,
            free_port,
            f"relayasym_{rt_a}_{rt_b}",
            env_a=env,
            env_b=env,
        )
        try:
            meta_a = _relay_meta(
                handler_module=_HANDLER_MODULE[rt_a],
                guid="A",
                partner_guid="B",
                relay_url=relay.url,
                relay_token=relay.password,
            )
            meta_a["ingress_rule"] = "deny_inbound"
            meta_a["egress_rule"] = "password"
            created = await asyncio.gather(
                kp_a.call("kernel", "create_agent", **meta_a),
                kp_b.call(
                    "kernel",
                    "create_agent",
                    **_relay_meta(
                        handler_module=_HANDLER_MODULE[rt_b],
                        guid="B",
                        partner_guid="A",
                        relay_url=relay.url,
                        relay_token=relay.password,
                        auth="password",
                    ),
                ),
            )
            rcb = await _await_connected(kp_a, created)
            assert rcb.get("ingress_rule") == "deny_inbound", f"A ingress: {rcb}"
            assert rcb.get("egress_rule") == "password", f"A egress: {rcb}"

            # A→B: A's egress presents the fleet token, B's ingress accepts → round-trip.
            reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
            assert isinstance(reply, dict) and "id" in reply and "tree" in reply, (
                f"A→B (egress presents, B checks) should round-trip: {reply}"
            )
            # B→A: A's ingress is deny_inbound → refused regardless of B's token.
            reverse = await kp_b.call("cb", "forward", target="kernel", payload={"type": "reflect"})
            assert reverse.get("reason") == "unauthorized", f"B→A should be denied: {reverse}"
        finally:
            for kp in spawned:
                kp.terminate()
    finally:
        relay.stop()


# ── directory surface (list_peers + watch) ──────────────────────


@pytest.mark.parametrize("rt", ["python", "rust", "swift"])
@pytest.mark.asyncio
async def test_relay_directory(rt, request, parity_tmp, free_port) -> None:
    """A connector reads the LIVE relay directory: `list_peers` returns the
    connected peers with green health, and `watch_directory` subscribes (acks).
    Two connectors of runtime `rt` both connect → both appear. The event-surfacing
    onto the connector inbox is unit-tested per runtime."""
    if _pair_skip_reason(rt, rt):
        pytest.skip("relay_connector not available")
    relay = Relay(require_relay(), free_port()).start()
    try:
        kp_a, kp_b, _, _, spawned = await _spawn_pair(
            rt, rt, request, parity_tmp, free_port, f"relay_dir_{rt}"
        )
        try:
            created = await asyncio.gather(
                kp_a.call(
                    "kernel",
                    "create_agent",
                    **_relay_meta(
                        handler_module=_HANDLER_MODULE[rt],
                        guid="A",
                        partner_guid="B",
                        relay_url=relay.url,
                        relay_token=relay.password,
                        auth="allow_all",
                    ),
                ),
                kp_b.call(
                    "kernel",
                    "create_agent",
                    **_relay_meta(
                        handler_module=_HANDLER_MODULE[rt],
                        guid="B",
                        partner_guid="A",
                        relay_url=relay.url,
                        relay_token=relay.password,
                        auth="allow_all",
                    ),
                ),
            )
            await _await_connected(kp_a, created)
            await _await_connected(kp_b, created)

            # list_peers → both peers, green (addresses the relay directory).
            snap = await kp_a.call("cb", "list_peers")
            guids = {p.get("guid") for p in snap.get("peers", [])}
            assert {"A", "B"} <= guids, f"directory should list both peers: {snap}"
            assert all(p.get("status") == "green" for p in snap["peers"]), snap

            # watch_directory → ack (the live event feed is unit-tested per runtime).
            ack = await kp_a.call("cb", "watch_directory")
            assert ack.get("ok") is True, f"watch_directory should ack: {ack}"
        finally:
            for kp in spawned:
                kp.terminate()
    finally:
        relay.stop()


# ── auto-reconnect (relay restart) ──────────────────────────────


@pytest.mark.asyncio
async def test_relay_reconnects_after_relay_restart(request, parity_tmp, free_port) -> None:
    """A leg with auto-reconnect re-establishes after the RELAY restarts on the
    same port: a forward round-trips, the relay is killed + restarted, the leg
    re-dials with its `reconnect` backoff, and a forward round-trips again.
    Python↔python (fastest); the reconnect logic is shared/mirrored across runtimes."""
    if _pair_skip_reason("python", "python"):
        pytest.skip("relay_connector not available")
    relay_bin = require_relay()
    port = free_port()
    relay = Relay(relay_bin, port).start()
    spawned: list[KernelProc] = []
    try:
        kp_a, kp_b, _, _, spawned = await _spawn_pair(
            "python", "python", request, parity_tmp, free_port, "relay_reconnect"
        )

        def meta(guid, partner, auth):
            m = _relay_meta(
                handler_module=_HANDLER_MODULE["python"],
                guid=guid,
                partner_guid=partner,
                relay_url=relay.url,
                relay_token=relay.password,
                auth=auth,
            )
            m["reconnect"] = 1  # fast backoff for the test
            return m

        created = await asyncio.gather(
            kp_a.call("kernel", "create_agent", **meta("A", "B", "deny_inbound")),
            kp_b.call("kernel", "create_agent", **meta("B", "A", "allow_all")),
        )
        await _await_connected(kp_a, created)
        r1 = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
        assert "tree" in r1, f"pre-restart forward failed: {r1}"

        # Kill the relay (drops both legs), then restart it on the SAME port.
        relay.stop()
        await asyncio.sleep(0.5)
        relay = Relay(relay_bin, port).start()

        # The legs re-dial (backoff 1s); poll until A reports connected again.
        rc = await kp_a.call("cb", "reflect")
        for _ in range(40):  # up to ~10s
            if rc.get("connected"):
                break
            await asyncio.sleep(0.25)
            rc = await kp_a.call("cb", "reflect")
        assert rc.get("connected") is True, f"A did not reconnect after relay restart: {rc}"

        # A forward round-trips again over the healed connection.
        r2 = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
        assert "tree" in r2, f"post-reconnect forward failed: {r2}"
    finally:
        for kp in spawned:
            kp.terminate()
        relay.stop()
