"""STREAMS through the relay — raw bytes cross-kernel over cloud_bridge.

The matrix (test_relay_matrix) proves text `call`/`reply` round-trips through the
relay. This proves the BINARY half: a `write_stream`/`read_stream` chunk rides the
whole chain as RAW BYTES (never base64) —

    test ──binary WS frame──▶ A.web_ws ──▶ A.cloud_bridge ──tag-1 TLS record──▶
        relay (opaque ciphertext) ──▶ B.cloud_bridge ──▶ B.file_bridge (disk)

and back out for `read_stream`. A is always python (the canonical driver — the
test speaks the io_bridge binary-frame codec straight onto A's web_ws); B is
parametrized over every host runtime. Done-when: bytes written through the relay
land verbatim on B's disk AND read back byte-identical over the same chain.

The codec is reimplemented inline (≈20 lines) on purpose: this test asserts the
WIRE contract `[4B BE len | JSON header (+_binary_path) | raw body]`, not a
library import.
"""

from __future__ import annotations

import asyncio
import json
import os
import struct
import sys
import uuid
from pathlib import Path
from typing import Any, Callable

import pytest
import websockets

sys.path.insert(0, str(Path(__file__).resolve().parent))
from relay_harness import Relay, cloud_cert, new_id_key, require_relay  # noqa: E402

from helpers.kernel_proc import KernelProc  # noqa: E402
from helpers.seeding import seed_create, seed_web, seed_web_ws  # noqa: E402
from test_relay_matrix import CLOUD_BRIDGE_RUNTIMES, _HANDLER_MODULE, _cb_meta  # noqa: E402

# A python driver streams to each host runtime's fs through the relay.
TARGETS = [rt for rt in ("python", "rust", "swift") if rt in CLOUD_BRIDGE_RUNTIMES]

# Non-UTF-8 bytes — proves the channel is raw, not text/base64.
PAYLOAD = bytes([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80]) * 1024  # 8 KiB


# ── the io_bridge binary-frame wire codec (inline, spec-faithful) ──────────


def _encode_frame(envelope: dict[str, Any]) -> bytes:
    """`[4B BE len | header | body]` — the single bytes value nulled in the
    header, its dotted path in `_binary_path`, raw bytes trailing."""

    def find(obj: Any, prefix: str = "") -> tuple[str, bytes] | None:
        if isinstance(obj, (bytes, bytearray)):
            return (prefix, bytes(obj))
        if isinstance(obj, dict):
            for k, v in obj.items():
                r = find(v, f"{prefix}.{k}" if prefix else k)
                if r:
                    return r
        return None

    path, body = find(envelope) or (None, b"")
    assert path, "test frame must carry bytes"

    def nulled(obj: Any, at: str) -> Any:
        head, _, rest = at.partition(".")
        return {
            k: (nulled(v, rest) if k == head and rest else (None if k == head else v))
            for k, v in obj.items()
        }

    header = dict(nulled(envelope, path))
    header["_binary_path"] = path
    h = json.dumps(header).encode()
    return struct.pack(">I", len(h)) + h + body


def _decode_frame(wire: bytes) -> dict[str, Any]:
    """Inverse: parse the header, restore the trailing body at `_binary_path`."""
    (hlen,) = struct.unpack(">I", wire[:4])
    header = json.loads(wire[4 : 4 + hlen])
    body = wire[4 + hlen :]
    path = header.pop("_binary_path", None)
    if path:
        obj = header
        parts = path.split(".")
        for p in parts[:-1]:
            obj = obj[p]
        obj[parts[-1]] = body
    return header


async def _binary_call(ws, target: str, payload: dict[str, Any], timeout: float = 30.0) -> dict:
    """Send one binary call frame on an open web_ws socket; await the matching
    reply (text OR binary), skipping unrelated event/boot frames."""
    corr = f"st_{uuid.uuid4().hex[:8]}"
    await ws.send(_encode_frame({"type": "call", "target": target, "id": corr, "payload": payload}))
    deadline = asyncio.get_event_loop().time() + timeout
    while True:
        raw = await asyncio.wait_for(
            ws.recv(), timeout=max(0.1, deadline - asyncio.get_event_loop().time())
        )
        msg = _decode_frame(raw) if isinstance(raw, (bytes, bytearray)) else json.loads(raw)
        if msg.get("type") == "reply" and msg.get("id") == corr:
            return msg.get("data", {})


# ── the test ───────────────────────────────────────────────────────────────


@pytest.mark.parametrize("rt_b", TARGETS)
@pytest.mark.asyncio
async def test_relay_streams_raw_bytes(
    rt_b: str,
    request: pytest.FixtureRequest,
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    bin_a = request.getfixturevalue("python_binary")
    bin_b = request.getfixturevalue(f"{rt_b}_binary")

    base = parity_tmp(f"relay_stream_{rt_b}")
    wd_a, wd_b = base / "A", base / "B"
    wd_a.mkdir(parents=True)
    wd_b.mkdir(parents=True)
    port_a, port_b = free_port(), free_port()

    for binary, wd, port in [(bin_a, wd_a, port_a), (bin_b, wd_b, port_b)]:
        seed_web(binary, wd, port)
        seed_web_ws(binary, wd)
    # B's fs edge — an OPEN file_bridge rooted INSIDE B's workdir (the clamp).
    (wd_b / "streams").mkdir()
    seed_create(
        bin_b,
        wd_b,
        handler_module="file_bridge.tools",
        agent_id="fs",
        root="streams",
        ingress_rule="allow_all",
    )

    relay = Relay(*require_relay(), free_port()).start()
    spawned: list[KernelProc] = []
    try:
        kp_a = bin_a.start_daemon(wd_a, port_a, label="python")
        spawned.append(kp_a)
        await kp_a.wait_ready()
        kp_b = bin_b.start_daemon(wd_b, port_b, label=rt_b)
        spawned.append(kp_b)
        await kp_b.wait_ready()

        idk_a, idk_b = new_id_key(), new_id_key()
        cert_a = cloud_cert("python", idk_a, bin_a, wd_a)
        cert_b = cloud_cert(rt_b, idk_b, bin_b, wd_b)
        rv = "rv-" + os.urandom(4).hex()
        meta_a = _cb_meta(
            handler_module=_HANDLER_MODULE["python"],
            peer="A",
            partner="B",
            role="client",
            relay_url=relay.url,
            rendezvous=rv,
            id_key=idk_a,
            peer_cert_pem=cert_b,
            issue_url=relay.issue_url,
            auth=None,
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
            # B answers inbound forwards — its leg must be explicitly OPEN.
            auth="allow_all",
        )
        await asyncio.gather(
            kp_a.call("kernel", "create_agent", **meta_a),
            kp_b.call("kernel", "create_agent", **meta_b),
        )
        rcb = await kp_a.call("cb", "reflect")
        for _ in range(50):
            if rcb.get("connected"):
                break
            await asyncio.sleep(0.1)
            rcb = await kp_a.call("cb", "reflect")
        assert rcb.get("connected") is True, f"A leg never connected: {rcb}"

        url = f"ws://127.0.0.1:{port_a}/cb/ws"
        async with websockets.connect(url, open_timeout=10, max_size=None) as ws:
            # write_stream: raw bytes A → relay → B → disk.
            w = await _binary_call(
                ws,
                "cb",
                {
                    "type": "forward",
                    "target": "fs",
                    "payload": {
                        "type": "write_stream",
                        "path": "blob.bin",
                        "truncate": True,
                        "bytes": PAYLOAD,
                    },
                },
            )
            assert w.get("written") == len(PAYLOAD), f"write through relay failed: {w}"
            on_disk = (wd_b / "streams" / "blob.bin").read_bytes()
            assert on_disk == PAYLOAD, "bytes must land verbatim on B's disk"

            # read_stream: raw bytes B → relay → A → this socket.
            r = await _binary_call(
                ws,
                "cb",
                {
                    "type": "forward",
                    "target": "fs",
                    "payload": {
                        "type": "read_stream",
                        "path": "blob.bin",
                        "length": len(PAYLOAD),
                        "bytes": b"",
                    },
                },
            )
            got = r.get("bytes")
            assert isinstance(got, (bytes, bytearray)) and bytes(got) == PAYLOAD, (
                f"raw bytes must round-trip through the relay "
                f"(got {type(got).__name__}, {len(got) if got else 0}B)"
            )
    finally:
        for kp in spawned:
            kp.terminate()
        relay.stop()
