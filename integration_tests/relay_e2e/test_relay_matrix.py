"""Kernels-through-relay e2e — the any-to-any + same-kind matrix.

Topology per pair: two host kernels each running a `cloud_bridge` agent, BOTH
dialling OUT to one relay; the relay pairs them by (tenant, rendezvous) and
forwards OPAQUE frames; the two cloud_bridges run end-to-end TLS 1.3 mutual auth
and tunnel `kernel.send` `call`/`reply` through it. Done-when: a reflect forwarded
from A round-trips B's root reply through the relay (`tmp/relay_e2e_setup.md`).

The matrix is the 3 host runtimes x 2 (any-to-any + same-kind). A leg runs once
its runtime ships a cloud_bridge transport (CLOUD_BRIDGE_RUNTIMES). Cross-runtime
pinning works because the harness derives each leg's ACTUAL cert the way its own
runtime does -- python via its `_tls` builder, rust/swift via the kernel binary's
`__cloud-cert` subcommand -- and cross-feeds the exact cert to the validating peer.

Heavy + opt-in; skips cleanly when the relay binaries aren't built.
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path
from typing import Callable

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from relay_harness import PASSWORD, Relay, cloud_cert, new_id_key, require_relay  # noqa: E402

from helpers.kernel_proc import KernelProc  # noqa: E402
from helpers.seeding import seed_web, seed_web_ws  # noqa: E402

# Runtimes that ship a cloud_bridge transport today.
CLOUD_BRIDGE_RUNTIMES = {"python", "rust", "swift"}

# `handler_module` for the cloud_bridge agent per runtime: all three host
# runtimes now ship it as the standalone `cloud_bridge` derivation (split out
# of the combined kernel_bridge).
_HANDLER_MODULE = {
    "python": "cloud_bridge.tools",
    "rust": "cloud_bridge.tools",
    "swift": "cloud_bridge.tools",
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
    """Why a runtime pair can't run yet (None = runs). Cross-runtime pinning works
    via per-runtime cert derivation, so the only gate is transport availability."""
    for rt in (a, b):
        if rt not in CLOUD_BRIDGE_RUNTIMES:
            return f"cloud_bridge transport not yet in e2e for {rt}"
    return None


def _cb_meta(
    *,
    handler_module: str,
    peer: str,
    partner: str,
    role: str,
    relay_url: str,
    rendezvous: str,
    id_key: str,
    peer_cert_pem: bytes,
    issue_url: str,
    auth: str | None = None,
) -> dict:
    meta = {
        "handler_module": handler_module,
        "id": "cb",
        "transport": "cloud_bridge",
        "relay_url": relay_url,
        "tenant_id": "t1",
        "peer_id": peer,
        "partner_peer_id": partner,
        "rendezvous": rendezvous,
        "id_key": id_key,
        "approved_peer_certs": [peer_cert_pem.decode("ascii")],
        # TokenSource = the production HTTP path: the kernel POSTs the relay's
        # /issue with our credential to obtain a token (vs a pre-minted literal).
        "issue_url": issue_url,
        "password": PASSWORD,
        "provider": "password",
        "tls_role": role,
        "heartbeat": 0,
    }
    # The per-leg auth policy. IO legs now SEAL by default (absent rule ⇒
    # deny_inbound), so any leg that must answer inbound forwards sets this
    # explicitly to `allow_all` (or `password` for the group-secret tests).
    if auth is not None:
        meta["auth"] = auth
    return meta


async def _pair_round_trip(
    rt_a: str,
    rt_b: str,
    relay: Relay,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
    tag: str,
) -> None:
    # Per-runtime LAUNCHER fixtures (python_binary / rust_binary …) — sync, so
    # they resolve mid-test. The KERNEL fixtures are async and can't be pulled
    # from inside a running loop, so we spawn via the launcher directly + manage
    # teardown ourselves (mirrors the `{rt}_kernel` fixture body).
    bin_a = request.getfixturevalue(f"{rt_a}_binary")
    bin_b = request.getfixturevalue(f"{rt_b}_binary")

    base = parity_tmp(tag)
    wd_a, wd_b = base / "A", base / "B"
    wd_a.mkdir(parents=True)
    wd_b.mkdir(parents=True)
    port_a, port_b = free_port(), free_port()

    # web + web_ws so each daemon is drivable over WS (no cloud_bridge persisted →
    # no blocking auto-boot at startup; we create+boot the legs concurrently below).
    for binary, wd, port in [(bin_a, wd_a, port_a), (bin_b, wd_b, port_b)]:
        seed_web(binary, wd, port)
        seed_web_ws(binary, wd)

    spawned: list[KernelProc] = []
    try:
        kp_a = bin_a.start_daemon(wd_a, port_a, label=rt_a)
        spawned.append(kp_a)
        await kp_a.wait_ready()
        kp_b = bin_b.start_daemon(wd_b, port_b, label=rt_b)
        spawned.append(kp_b)
        await kp_b.wait_ready()

        await _drive_pair(rt_a, rt_b, relay, kp_a, kp_b, (bin_a, wd_a), (bin_b, wd_b))
    finally:
        for kp in spawned:
            kp.terminate()


async def _drive_pair(
    rt_a: str,
    rt_b: str,
    relay: Relay,
    kp_a: KernelProc,
    kp_b: KernelProc,
    leg_a: tuple,
    leg_b: tuple,
) -> None:
    bin_a, wd_a = leg_a
    bin_b, wd_b = leg_b
    idk_a, idk_b = new_id_key(), new_id_key()
    # Each leg's cert carries its durable identity = its Ed25519 pubkey. All
    # runtimes pin by PUBKEY (not cert bytes), so the carrier here need only hold
    # the right key — we use each runtime's own cert builder (realistic), and the
    # peer matches the pubkey even though the presented cert may differ (swift's
    # is non-deterministic). That's exactly what cross-runtime pinning must do.
    cert_a = cloud_cert(rt_a, idk_a, bin_a, wd_a)
    cert_b = cloud_cert(rt_b, idk_b, bin_b, wd_b)
    rv = "rv-" + os.urandom(4).hex()
    # No pre-minted token: each leg's kernel POSTs the relay's /issue endpoint
    # (issue_url + password/provider) to obtain its own token — the production path.

    meta_a = _cb_meta(
        handler_module=_HANDLER_MODULE[rt_a],
        peer="A",
        partner="B",
        role="client",
        relay_url=relay.url,
        rendezvous=rv,
        id_key=idk_a,
        peer_cert_pem=cert_b,
        issue_url=relay.issue_url,
        # Directional topology: A refuses INBOUND calls (hub→spoke push). A→B
        # forwards still succeed (B is allow_all); B→A reverse calls are denied
        # on arrival at A. The cross-runtime wire-shape guard for `deny_inbound`.
        auth="deny_inbound",
    )
    meta_b = _cb_meta(
        handler_module=_HANDLER_MODULE[rt_b],
        peer="B",
        partner="A",
        role="server",
        relay_url=relay.url,
        rendezvous=rv,
        id_key=idk_b,
        peer_cert_pem=cert_a,
        issue_url=relay.issue_url,
        # B is EXPLICITLY OPEN — IO legs now seal by default (absent rule ⇒
        # deny_inbound), so without this the A→B forward below would be refused.
        auth="allow_all",
    )

    # create_agent auto-boots (awaited) → the cloud_bridge dials the relay + runs
    # the TLS handshake. Run BOTH concurrently so the relay can pair them (each
    # blocks in the handshake until its partner connects).
    created = await asyncio.gather(
        kp_a.call("kernel", "create_agent", **meta_a),
        kp_b.call("kernel", "create_agent", **meta_b),
    )

    # Both legs paired + handshook through the relay. Some runtimes boot the
    # transport asynchronously (create_agent returns before the handshake
    # completes), so poll the connectivity flag rather than asserting instantly.
    rcb = await kp_a.call("cb", "reflect")
    for _ in range(50):
        if rcb.get("connected"):
            break
        await asyncio.sleep(0.1)
        rcb = await kp_a.call("cb", "reflect")
    assert rcb.get("connected") is True, (
        f"A cloud_bridge not connected after 5s.\n  create replies: {created}\n  reflect: {rcb}"
    )

    # A's leg surfaces its policy back through reflect.
    assert rcb.get("ingress_rule") == "deny_inbound", f"A reflect missing auth policy: {rcb}"

    # The payload: forward a reflect to B's root over the relay; reply round-trips.
    # A→B is allowed (B's leg is EXPLICITLY opened with `auth=allow_all` above —
    # IO legs seal by default now) — the open-leg guard.
    reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict), f"non-dict reply: {reply!r}"
    assert "error" not in reply, f"forward errored through the relay: {reply}"
    assert "id" in reply and "tree" in reply, (
        f"reply lacks B's uniform reflect (round-trip failed): {list(reply.keys())}"
    )

    # Reverse direction (B → A's deny_inbound leg) is refused on arrival at A —
    # the same `{reason:"unauthorized"}` wire shape across every runtime pair.
    reverse = await kp_b.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reverse, dict), f"non-dict reverse reply: {reverse!r}"
    assert reverse.get("reason") == "unauthorized", (
        f"B→A reverse call should be denied by A's deny_inbound policy, got: {reverse}"
    )


async def _drive_password_pair(
    rt_a: str,
    rt_b: str,
    relay: Relay,
    kp_a: KernelProc,
    kp_b: KernelProc,
    leg_a: tuple,
    leg_b: tuple,
    *,
    expect_denied: bool,
) -> None:
    """Both legs are kernel-GROUP members (`auth="password"` ⇒ the default
    `FANTASTIC_GROUP_TOKEN`, injected into each daemon's env at spawn). When the two
    daemons share the token, A→B and B→A both round-trip (the token is presented on
    each call envelope, survives the relay+TLS, and is checked on arrival). When the
    tokens differ (`expect_denied`), B refuses A's call `unauthorized`."""
    bin_a, wd_a = leg_a
    bin_b, wd_b = leg_b
    idk_a, idk_b = new_id_key(), new_id_key()
    cert_a = cloud_cert(rt_a, idk_a, bin_a, wd_a)
    cert_b = cloud_cert(rt_b, idk_b, bin_b, wd_b)
    rv = "rv-" + os.urandom(4).hex()

    meta_a = _cb_meta(
        handler_module=_HANDLER_MODULE[rt_a],
        peer="A",
        partner="B",
        role="client",
        relay_url=relay.url,
        rendezvous=rv,
        id_key=idk_a,
        peer_cert_pem=cert_b,
        issue_url=relay.issue_url,
        auth="password",  # string form ⇒ default token_env FANTASTIC_GROUP_TOKEN
    )
    meta_b = _cb_meta(
        handler_module=_HANDLER_MODULE[rt_b],
        peer="B",
        partner="A",
        role="server",
        relay_url=relay.url,
        rendezvous=rv,
        id_key=idk_b,
        peer_cert_pem=cert_a,
        issue_url=relay.issue_url,
        auth="password",
    )

    created = await asyncio.gather(
        kp_a.call("kernel", "create_agent", **meta_a),
        kp_b.call("kernel", "create_agent", **meta_b),
    )
    rcb = await kp_a.call("cb", "reflect")
    for _ in range(50):
        if rcb.get("connected"):
            break
        await asyncio.sleep(0.1)
        rcb = await kp_a.call("cb", "reflect")
    assert rcb.get("connected") is True, (
        f"A cloud_bridge not connected after 5s.\n  create replies: {created}\n  reflect: {rcb}"
    )
    # reflect surfaces only the policy NAME (never the env-var config).
    assert rcb.get("ingress_rule") == "password", f"A reflect missing auth policy: {rcb}"

    reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict), f"non-dict reply: {reply!r}"
    if expect_denied:
        # A presents a DIFFERENT group token than B expects → B refuses on arrival.
        assert reply.get("reason") == "unauthorized", (
            f"outsider (wrong group token) should be denied by B, got: {reply}"
        )
    else:
        # Same group: A→B forward round-trips (token presented + accepted)…
        assert "id" in reply and "tree" in reply, (
            f"group member A→B forward failed: {list(reply.keys())}"
        )
        # …and symmetrically B→A (each runtime accepts the peer's group token).
        reverse = await kp_b.call("cb", "forward", target="kernel", payload={"type": "reflect"})
        assert isinstance(reverse, dict) and "id" in reverse and "tree" in reverse, (
            f"group member B→A forward failed: {reverse}"
        )


async def _password_round_trip(
    rt_a: str,
    rt_b: str,
    relay: Relay,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
    tag: str,
    *,
    token_a: str,
    token_b: str,
    expect_denied: bool,
) -> None:
    bin_a = request.getfixturevalue(f"{rt_a}_binary")
    bin_b = request.getfixturevalue(f"{rt_b}_binary")

    base = parity_tmp(tag)
    wd_a, wd_b = base / "A", base / "B"
    wd_a.mkdir(parents=True)
    wd_b.mkdir(parents=True)
    port_a, port_b = free_port(), free_port()

    for binary, wd, port in [(bin_a, wd_a, port_a), (bin_b, wd_b, port_b)]:
        seed_web(binary, wd, port)
        seed_web_ws(binary, wd)

    spawned: list[KernelProc] = []
    try:
        # Inject each daemon's group token via env — the production posture (the
        # secret never touches the portable `.fantastic` workdir / agent records).
        kp_a = bin_a.start_daemon(
            wd_a, port_a, label=rt_a, extra_env={"FANTASTIC_GROUP_TOKEN": token_a}
        )
        spawned.append(kp_a)
        await kp_a.wait_ready()
        kp_b = bin_b.start_daemon(
            wd_b, port_b, label=rt_b, extra_env={"FANTASTIC_GROUP_TOKEN": token_b}
        )
        spawned.append(kp_b)
        await kp_b.wait_ready()

        await _drive_password_pair(
            rt_a,
            rt_b,
            relay,
            kp_a,
            kp_b,
            (bin_a, wd_a),
            (bin_b, wd_b),
            expect_denied=expect_denied,
        )
    finally:
        for kp in spawned:
            kp.terminate()


@pytest.mark.parametrize("rt_a,rt_b", PAIRS, ids=[f"{a}-{b}" for a, b in PAIRS])
@pytest.mark.asyncio
async def test_relay_any_to_any(
    rt_a: str,
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """Each host-runtime pair (any-to-any + same-kind) round-trips a kernel.send
    through the relay over cloud_bridge. Legs gated by transport availability +
    cert-pinning compatibility skip with a clear reason (see _pair_skip_reason)."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)

    relay = Relay(*require_relay(), free_port()).start()  # require_relay skips if not built
    try:
        await _pair_round_trip(
            rt_a, rt_b, relay, request, parity_tmp, free_port, f"relay_{rt_a}_{rt_b}"
        )
    finally:
        relay.stop()


@pytest.mark.parametrize("rt_a,rt_b", PAIRS, ids=[f"{a}-{b}" for a, b in PAIRS])
@pytest.mark.asyncio
async def test_relay_password_group_member(
    rt_a: str,
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """Both legs are kernel-group members (`auth="password"`, SAME group token in
    env): the token is presented on each call envelope, survives the relay+TLS, and
    is checked on arrival — A→B and B→A both round-trip across every runtime pair."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)

    relay = Relay(*require_relay(), free_port()).start()
    try:
        await _password_round_trip(
            rt_a,
            rt_b,
            relay,
            request,
            parity_tmp,
            free_port,
            f"relaypw_{rt_a}_{rt_b}",
            token_a="grp-secret",
            token_b="grp-secret",
            expect_denied=False,
        )
    finally:
        relay.stop()


# B = each runtime in turn (the enforcing receiver); A = python presents a token.
_REJECT_PAIRS = [("python", "python"), ("python", "rust"), ("python", "swift")]


@pytest.mark.parametrize("rt_a,rt_b", _REJECT_PAIRS, ids=[f"{a}-{b}" for a, b in _REJECT_PAIRS])
@pytest.mark.asyncio
async def test_relay_password_rejects_outsider(
    rt_a: str,
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """A presents a DIFFERENT group token than B expects (different kernel groups):
    B's `password` gate refuses A's inbound call `unauthorized` on arrival. Covers
    each runtime as the enforcing receiver (B)."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)

    relay = Relay(*require_relay(), free_port()).start()
    try:
        await _password_round_trip(
            rt_a,
            rt_b,
            relay,
            request,
            parity_tmp,
            free_port,
            f"relaypwx_{rt_a}_{rt_b}",
            token_a="grp-A",
            token_b="grp-B",
            expect_denied=True,
        )
    finally:
        relay.stop()


async def _drive_asymmetric_pair(
    rt_a: str,
    rt_b: str,
    relay: Relay,
    kp_a: KernelProc,
    kp_b: KernelProc,
    leg_a: tuple,
    leg_b: tuple,
) -> None:
    """A is a HUB: `ingress_rule="deny_inbound"` (refuse inbound) + `egress_rule=
    "password"` (still present the fleet token). B is a group member (`auth=
    "password"`). Proves the symmetric SPLIT through the relay: A→B succeeds (A's
    egress presents, B's ingress checks) while B→A is refused (A's ingress denies) —
    independent per-direction rules, identical wire shape across runtimes."""
    bin_a, wd_a = leg_a
    bin_b, wd_b = leg_b
    idk_a, idk_b = new_id_key(), new_id_key()
    cert_a = cloud_cert(rt_a, idk_a, bin_a, wd_a)
    cert_b = cloud_cert(rt_b, idk_b, bin_b, wd_b)
    rv = "rv-" + os.urandom(4).hex()

    meta_a = _cb_meta(
        handler_module=_HANDLER_MODULE[rt_a],
        peer="A",
        partner="B",
        role="client",
        relay_url=relay.url,
        rendezvous=rv,
        id_key=idk_a,
        peer_cert_pem=cert_b,
        issue_url=relay.issue_url,
    )
    # The split fields (string form ⇒ default token_env FANTASTIC_GROUP_TOKEN).
    meta_a["ingress_rule"] = "deny_inbound"
    meta_a["egress_rule"] = "password"
    meta_b = _cb_meta(
        handler_module=_HANDLER_MODULE[rt_b],
        peer="B",
        partner="A",
        role="server",
        relay_url=relay.url,
        rendezvous=rv,
        id_key=idk_b,
        peer_cert_pem=cert_a,
        issue_url=relay.issue_url,
        auth="password",
    )

    created = await asyncio.gather(
        kp_a.call("kernel", "create_agent", **meta_a),
        kp_b.call("kernel", "create_agent", **meta_b),
    )
    rcb = await kp_a.call("cb", "reflect")
    for _ in range(50):
        if rcb.get("connected"):
            break
        await asyncio.sleep(0.1)
        rcb = await kp_a.call("cb", "reflect")
    assert rcb.get("connected") is True, (
        f"A cloud_bridge not connected after 5s.\n  create replies: {created}\n  reflect: {rcb}"
    )
    # reflect surfaces both directions independently (auth alias = ingress).
    assert rcb.get("ingress_rule") == "deny_inbound", f"A ingress: {rcb}"
    assert rcb.get("egress_rule") == "password", f"A egress: {rcb}"

    # A→B: A's egress presents the fleet token, B's ingress accepts → round-trip.
    reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict) and "id" in reply and "tree" in reply, (
        f"A→B (egress presents, B checks) should round-trip: {reply}"
    )
    # B→A: A's ingress is deny_inbound → refused regardless of B's token.
    reverse = await kp_b.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert reverse.get("reason") == "unauthorized", (
        f"B→A should be denied by A's ingress deny_inbound: {reverse}"
    )


async def _asymmetric_round_trip(
    rt_a: str,
    rt_b: str,
    relay: Relay,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
    tag: str,
) -> None:
    bin_a = request.getfixturevalue(f"{rt_a}_binary")
    bin_b = request.getfixturevalue(f"{rt_b}_binary")
    base = parity_tmp(tag)
    wd_a, wd_b = base / "A", base / "B"
    wd_a.mkdir(parents=True)
    wd_b.mkdir(parents=True)
    port_a, port_b = free_port(), free_port()
    for binary, wd, port in [(bin_a, wd_a, port_a), (bin_b, wd_b, port_b)]:
        seed_web(binary, wd, port)
        seed_web_ws(binary, wd)

    spawned: list[KernelProc] = []
    try:
        # Both daemons share the fleet token in env (A presents it, B checks it).
        env = {"FANTASTIC_GROUP_TOKEN": "fleet"}
        kp_a = bin_a.start_daemon(wd_a, port_a, label=rt_a, extra_env=env)
        spawned.append(kp_a)
        await kp_a.wait_ready()
        kp_b = bin_b.start_daemon(wd_b, port_b, label=rt_b, extra_env=env)
        spawned.append(kp_b)
        await kp_b.wait_ready()
        await _drive_asymmetric_pair(rt_a, rt_b, relay, kp_a, kp_b, (bin_a, wd_a), (bin_b, wd_b))
    finally:
        for kp in spawned:
            kp.terminate()


@pytest.mark.parametrize("rt_a,rt_b", PAIRS, ids=[f"{a}-{b}" for a, b in PAIRS])
@pytest.mark.asyncio
async def test_relay_asymmetric_rules(
    rt_a: str,
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """The symmetric SPLIT through the relay: A is a hub (`ingress_rule="deny_inbound"`
    + `egress_rule="password"`), B a group member. A→B round-trips (egress presents,
    ingress checks); B→A is denied (A's ingress). Independent per-direction rules,
    identical wire shape across every runtime pair."""
    reason = _pair_skip_reason(rt_a, rt_b)
    if reason:
        pytest.skip(reason)

    relay = Relay(*require_relay(), free_port()).start()
    try:
        await _asymmetric_round_trip(
            rt_a, rt_b, relay, request, parity_tmp, free_port, f"relayasym_{rt_a}_{rt_b}"
        )
    finally:
        relay.stop()
