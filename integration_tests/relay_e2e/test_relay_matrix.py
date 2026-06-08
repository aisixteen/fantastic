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

# `handler_module` for the cloud_bridge agent per runtime: python ships it as a
# standalone bundle; rust/swift fold it into kernel_bridge as a transport kind.
_HANDLER_MODULE = {
    "python": "cloud_bridge.tools",
    "rust": "kernel_bridge.tools",
    "swift": "kernel_bridge.tools",
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
) -> dict:
    return {
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

    # The payload: forward a reflect to B's root over the relay; reply round-trips.
    reply = await kp_a.call("cb", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict), f"non-dict reply: {reply!r}"
    assert "error" not in reply, f"forward errored through the relay: {reply}"
    assert "id" in reply and "tree" in reply, (
        f"reply lacks B's uniform reflect (round-trip failed): {list(reply.keys())}"
    )


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
