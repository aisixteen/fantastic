"""kernel/kernel_bridges — two kernels wired kernel-to-kernel. NO MODEL.

The PACKAGED host of `RUNTIME` and a PEER (python; rust when RUNTIME=python), each
spawned and driven over `web_rest`. Each kernel holds a `ws_bridge` (`br`) dialing
the OTHER kernel's `web_ws` leg (`ws://127.0.0.1:<port>/ws/ws`) and a `file_bridge`
(`fb`, root `data/`). So both directions run: RUNTIME's bridge against the peer's
web_ws, and the peer's bridge against RUNTIME's web_ws.

Per direction (X's bridge → Y):
  1. the bridge is up on create; `reflect` says `connected`, transport `ws`;
  2. reflect of the remote: `forward kernel_state reflect` = Y's own root (its guid);
  3. send: `forward fb write` lands the file on Y's disk;
  4. bytes Y→X: `read_stream` pulled chunk by chunk to `eof` across the bridge,
     every byte equal (raw, the REST octet leg on X — no base64 anywhere);
  5. bytes X→Y: a BINARY ws frame `[len|header|body]` to X's `web_ws` calling
     `br forward fb write_stream` — the raw body lands on Y's disk byte-equal;
  6. stream: `forward fb watch_path`, `watch_remote fb`, a write on Y → a
     `changed` event re-emitted on X's bridge inbox with `src: fb`; `unwatch_remote`;
  7. the gate is Y's: `forward` to an agent Y's ws leg holds no grant for is
     refused `unauthorized` there, while Y's own operator reaches it.

Then, RUNTIME's bridge only:
  8. a bridge to a dead port: `boot` fails with an error, bounded (no hang);
     `forward` is refused `not_connected`;
  9. the peer dies: `connected:false` (bounded poll), `forward` is refused promptly;
     the peer comes back on the same port: still down (the readme: no
     auto-reconnect), `reconnect` → booted, `forward` reaches it again.

relay_connector, against a stub relay (`stub_relay.py`, the relay's routing):
 10. both kernels' connectors dial it with the password from their credential
     store (`X-Fantastic-Auth`) and the `fantastic.relay.v1` subprotocol;
 11. `forward` both ways through the relay; a `read_stream` chunk rides it as a
     BINARY relay frame, byte-equal;
 12. the serving connector's `ingress_rule: deny_ingress` refuses the tunneled call
     `unauthorized`, live; reopening it serves again.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import shutil
import socket
import struct
import sys
import tempfile
import threading
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import helpers.host.driver as drv
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.host.ws import WSWatcher
from helpers.verdict import verdict
from stub_relay import StubRelay

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
PEER = "rust" if RUNTIME == "python" else "python"
PW_VAR = "E2E_RELAY_PASSWORD"
PW = "relay-e2e-" + secrets.token_hex(10)
REACH = ["br", "fb", "dead", "rly", "creds"]
OPEN = {"$self": "*", "$parent": "*"}

bad: list[str] = []


def need(cond: bool, what: str) -> bool:
    print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
    if not cond:
        bad.append(what)
    return cond


def short(v, n: int = 240) -> str:
    if isinstance(v, dict):
        v = {k: (f"<{len(x)} bytes>" if isinstance(x, bytes) else x) for k, x in v.items()}
    return str(v)[:n]


def poll(fn, bound: float, step: float = 0.2):
    """fn() until truthy or `bound` seconds pass — the last value either way."""
    end = time.monotonic() + bound
    while True:
        v = fn()
        if v or time.monotonic() > end:
            return v
        time.sleep(step)


def port_open(port: int) -> bool:
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


# ─── a ws client on a kernel's web_ws leg (raw frames, binary included) ───


class WsCall(WSWatcher):
    """`call` frames over `ws://127.0.0.1:<port>/ws/ws` — no auto-watch."""

    async def connect(self) -> None:  # type: ignore[override]
        import base64

        self._r, self._w = await asyncio.open_connection("127.0.0.1", self.port)
        key = base64.b64encode(os.urandom(16)).decode()
        self._w.write(
            f"GET /{self.web_id}/ws HTTP/1.1\r\nHost: 127.0.0.1:{self.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
        await self._w.drain()
        head = await self._r.readuntil(b"\r\n\r\n")
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError(f"ws upgrade refused: {head[:120]!r}")

    async def call_binary(self, envelope: dict, path: list, body: bytes) -> dict:
        """One binary frame `[4B BE len | JSON header | raw body]`; the reply."""
        header = json.dumps({**envelope, "_binary_path": path}).encode()
        await self._send(struct.pack(">I", len(header)) + header + body, opcode=0x2)
        end = time.monotonic() + 60
        while time.monotonic() < end:
            try:
                raw = await asyncio.wait_for(self._frame(), max(0.1, end - time.monotonic()))
            except TimeoutError:
                break
            if raw is None:
                return {"error": "socket closed before the reply"}
            if raw:
                f = json.loads(raw)
                if f.get("type") == "error" and f.get("id") is None:
                    return f  # the frame itself was refused
                if f.get("id") == envelope["id"] and f.get("type") in ("reply", "error"):
                    return f
        return {"error": "no reply within 60s"}


def ws_binary_call(port: int, envelope: dict, path: list, body: bytes) -> dict:
    async def run():
        c = WsCall(port, "", web_id="ws")
        await c.connect()
        try:
            return await c.call_binary(envelope, path, body)
        finally:
            await c.close()

    return asyncio.run(run())


class EventTap:
    """Watch one agent's events on a kernel's ws leg from a thread; collect them."""

    def __init__(self, port: int, agent: str):
        self.events: list[dict] = []
        self.ready = threading.Event()
        self._w = WSWatcher(port, agent, web_id="ws")
        self._loop = asyncio.new_event_loop()
        self._t = threading.Thread(target=self._run, daemon=True)
        self._t.start()

    def _run(self) -> None:
        async def go():
            await self._w.connect()
            self.ready.set()
            async for p in self._w.events():
                self.events.append(p)

        try:
            self._loop.run_until_complete(go())
        except Exception:  # noqa: BLE001 — any end of the loop releases the waiter
            self.ready.set()

    def close(self) -> None:
        if self._w._w is not None:
            self._loop.call_soon_threadsafe(self._w._w.close)
        self._t.join(timeout=5)


# ─── one direction: X's bridge `br` → Y ─────────────────────────────


def bridge_checks(tag: str, X: HostKernel, Y: HostKernel, ywork: Path, yblob: bytes) -> None:
    print(f"\n── {tag} ──", flush=True)
    r = X.send("br", {"type": "reflect"})
    need(r.get("connected") is True and r.get("transport") == "ws",
         f"{tag} · bridge up on create: {short(r)}")
    b = X.send("br", {"type": "boot"})
    need(b.get("already") is True or b.get("booted") is True, f"{tag} · boot idempotent: {short(b)}")

    # 2 · reflect of the remote
    y_root = Y.send(Y.root_id, {"type": "reflect"})
    rr = X.send("br", {"type": "forward", "target": "kernel_state", "payload": {"type": "reflect"}})
    need(rr.get("id") == "kernel_state" and rr.get("guid") and rr.get("guid") == y_root.get("guid"),
         f"{tag} · forward reflect = the remote root (guid {rr.get('guid')} vs {y_root.get('guid')})")

    # 3 · send
    note = f"hello from {tag} {secrets.token_hex(4)}"
    w = X.send("br", {"type": "forward", "target": "fb",
                      "payload": {"type": "write", "path": "note.txt", "bytes": note}})
    landed = (ywork / "data/note.txt").read_text() if (ywork / "data/note.txt").exists() else None
    need(landed == note and not w.get("error"), f"{tag} · forward write lands on the remote disk: {short(w)}")

    # 4 · bytes Y→X, pulled as a stream to eof
    got, off, chunks, last = b"", 0, 0, {}
    for _ in range(32):
        last = X.send("br", {"type": "forward", "target": "fb",
                             "payload": {"type": "read_stream", "path": "blob.bin", "offset": off}})
        if not isinstance(last.get("bytes"), bytes):
            break
        got += last["bytes"]
        chunks += 1
        off = last.get("next_offset", off + len(last["bytes"]))
        if last.get("eof"):
            break
    need(got == yblob and chunks >= 3 and last.get("eof") is True,
         f"{tag} · read_stream across the bridge: {len(got)}/{len(yblob)} bytes in {chunks} chunks, "
         f"byte-equal={got == yblob}; last {short(last)}")

    # 5 · bytes X→Y in a binary ws frame
    up = bytes(range(256)) * 4 + b"\xff\xfe\x00\xc3(" + os.urandom(3000)
    env = {"type": "call", "id": f"up-{secrets.token_hex(4)}", "target": "br",
           "payload": {"type": "forward", "target": "fb",
                       "payload": {"type": "write_stream", "path": "up.bin", "bytes": None,
                                   "truncate": True}}}
    rep = ws_binary_call(X.port, env, ["payload", "payload", "bytes"], up)
    on_disk = (ywork / "data/up.bin").read_bytes() if (ywork / "data/up.bin").exists() else b""
    need(on_disk == up, f"{tag} · raw bytes X→Y (binary frame → forward write_stream): "
         f"{len(on_disk)}/{len(up)} byte-equal={on_disk == up}; reply {short(rep)}")

    # 6 · a stream of remote events: watch_remote → re-emitted on X's bridge inbox
    wp = X.send("br", {"type": "forward", "target": "fb",
                       "payload": {"type": "watch_path", "path": "."}})
    need(not wp.get("error"), f"{tag} · forward watch_path on the remote fb: {short(wp)}")
    tap = EventTap(X.port, "br")
    try:
        need(tap.ready.wait(10), f"{tag} · ws tap on X's bridge connected")
        wr = X.send("br", {"type": "watch_remote", "target": "fb"})
        need(wr.get("ok") is True and wr.get("watching") == "fb", f"{tag} · watch_remote: {short(wr)}")
        X.send("br", {"type": "forward", "target": "fb",
                      "payload": {"type": "write", "path": "w1.txt", "bytes": "x"}})

        def seen():
            return [e for e in tap.events
                    if e.get("src") == "fb" and e.get("type") == "changed"
                    and "w1.txt" in str(e.get("path"))]

        ev = poll(seen, 20)
        need(bool(ev), f"{tag} · remote `changed` event re-emitted on X's bridge with src fb: "
             f"{short(ev[0]) if ev else short(tap.events[-3:])}")
        uw = X.send("br", {"type": "unwatch_remote", "target": "fb"})
        need(uw.get("ok") is True, f"{tag} · unwatch_remote: {short(uw)}")
    finally:
        tap.close()

    # 7 · the gate is the remote's
    direct = Y.send("fb_closed", {"type": "reflect"})
    via = X.send("br", {"type": "forward", "target": "fb_closed", "payload": {"type": "reflect"}})
    need(direct.get("id") == "fb_closed" and via.get("reason") == "unauthorized",
         f"{tag} · an agent Y's ws leg holds no grant for: refused across the bridge "
         f"({short(via, 160)}), reachable by Y's operator ({direct.get('id')})")


# ─── the run ────────────────────────────────────────────────────────


def setup(k: HostKernel, peer_port: int) -> None:
    fb_cb = {**OPEN, "ws": "*", "rly": "*"}
    for aid, cb in (("fb", fb_cb), ("fb_closed", dict(OPEN))):
        rec = k.send(k.root_id, {"type": "create_agent", "parent_id": "rest",
                                 "handler_module": "file_bridge.tools", "id": aid,
                                 "root": "data", "allow": ["."],
                                 "ingress_rule": "allow_all_ingress", "callable_by": cb})
        need(rec.get("id") == aid, f":{k.port} create {aid}: {short(rec)}")
    rec = k.create(parent_id="rest", handler_module="ws_bridge.tools", id="br",
                   host="127.0.0.1", port=peer_port, peer_id="ws",
                   callable_by={**OPEN, "ws": "*"})
    need(rec.get("id") == "br", f":{k.port} create br → :{peer_port}: {short(rec)}")


def boot_kernel(spec: dict, work: Path, port: int | None = None) -> HostKernel:
    """Boot; with `port`, on THAT port (a peer coming back where it was)."""
    if port is None:
        return HostKernel(spec, work, reach=REACH).boot()
    keep = drv.free_port
    drv.free_port = lambda: port
    try:
        return HostKernel(spec, work, reach=REACH).boot()
    finally:
        drv.free_port = keep


def relay_checks(K: HostKernel, P: HostKernel, pblob: bytes) -> None:
    print("\n── relay_connector via a stub relay ──", flush=True)
    relay = StubRelay(PW)
    try:
        for k, me, other in ((K, "g-rt", "g-peer"), (P, "g-peer", "g-rt")):
            u = k.send(k.root_id, {"type": "update_agent", "id": "creds",
                                   "refs": {"relay": {"host": f"127.0.0.1:{relay.port}",
                                                      "env": {"password": PW_VAR}}},
                                   "callable_by": {**OPEN, "rly": ["get"]}})
            need(u.get("updated") is True, f":{k.port} relay ref on creds: {short(u)}")
            rec = k.create(parent_id="rest", handler_module="relay_connector.tools", id="rly",
                           relay_url=f"ws://127.0.0.1:{relay.port}", guid=me,
                           partner_guid=other, credentials={"store": "creds", "ref": "relay"},
                           heartbeat=0, reconnect=1, ingress_rule="allow_all_ingress",
                           can_call={"$self": "*", "creds": ["get"], "fb": "*"},
                           callable_by=dict(OPEN))
            need(rec.get("id") == "rly", f":{k.port} create rly ({me}): {short(rec)}")
            b = k.send("rly", {"type": "boot"})
            need(not b.get("error"), f":{k.port} rly boot: {short(b)}")
        for k, tag in ((K, RUNTIME), (P, PEER)):
            up = poll(lambda k=k: k.send("rly", {"type": "reflect"}).get("connected") is True, 15)
            need(bool(up), f"{tag} · relay connector connected: "
                 f"{short(k.send('rly', {'type': 'reflect'}))}")
        dials = relay.dials()
        for g in ("g-rt", "g-peer"):
            d = [x for x in dials if x["guid"] == g]
            need(bool(d) and all(x["auth_ok"] and x["subprotocol"] == "fantastic.relay.v1" for x in d),
                 f"relay saw {g} dial with the store's password + subprotocol: {d}")

        for X, tag in ((K, f"{RUNTIME}→{PEER}"), (P, f"{PEER}→{RUNTIME}")):
            r = X.send("rly", {"type": "forward", "target": "fb", "payload": {"type": "reflect"},
                               "forward_timeout": 20})
            need(r.get("id") == "fb", f"relay {tag} · forward reflect: {short(r)}")

        n0 = relay.binary_routed
        rs = K.send("rly", {"type": "forward", "target": "fb", "forward_timeout": 20,
                            "payload": {"type": "read_stream", "path": "blob.bin", "offset": 0}})
        chunk = rs.get("bytes")
        need(isinstance(chunk, bytes) and chunk == pblob[: len(chunk)] and len(chunk) > 0,
             f"relay {RUNTIME}→{PEER} · read_stream chunk byte-equal: {short(rs)}")
        need(relay.binary_routed > n0, f"relay · the chunk rode a BINARY relay frame "
             f"({relay.binary_routed - n0} routed)")

        u = P.send(P.root_id, {"type": "update_agent", "id": "rly", "ingress_rule": "deny_ingress"})
        need(u.get("updated") is True, f"{PEER} rly sealed: {short(u)}")
        r = K.send("rly", {"type": "forward", "target": "fb", "payload": {"type": "reflect"},
                           "forward_timeout": 20})
        need(r.get("reason") == "unauthorized",
             f"relay {RUNTIME}→{PEER} · a sealed serving leg refuses the tunneled call: {short(r)}")
        P.send(P.root_id, {"type": "update_agent", "id": "rly", "ingress_rule": "allow_all_ingress"})
        r = K.send("rly", {"type": "forward", "target": "fb", "payload": {"type": "reflect"},
                           "forward_timeout": 20})
        need(r.get("id") == "fb", f"relay {RUNTIME}→{PEER} · reopened live, served again: {short(r)}")
    finally:
        for k in (K, P):
            k.send(k.root_id, {"type": "delete_agent", "id": "rly"})
        relay.close()


def dead_peer_checks(K: HostKernel, P: HostKernel, pspec: dict, pwork: Path) -> HostKernel:
    print("\n── dead peer ──", flush=True)
    dead_port = drv.free_port()
    K.create(parent_id="rest", handler_module="ws_bridge.tools", id="dead",
             host="127.0.0.1", port=dead_port, peer_id="ws", callable_by=dict(OPEN))
    t = time.monotonic()
    b = K.send("dead", {"type": "boot"}, timeout=60)
    dt = time.monotonic() - t
    need(bool(b.get("error")) and dt < 15, f"boot to a dead port fails in {dt:.1f}s: {short(b)}")
    f = K.send("dead", {"type": "forward", "target": "kernel_state", "payload": {"type": "reflect"}})
    need(f.get("reason") == "not_connected", f"forward on a never-connected bridge: {short(f)}")

    port = P.port
    P.close()
    down = poll(lambda: K.send("br", {"type": "reflect"}).get("connected") is False, 15)
    need(bool(down), f"peer killed → {RUNTIME} bridge reflects connected:false")
    t = time.monotonic()
    f = K.send("br", {"type": "forward", "target": "kernel_state",
                      "payload": {"type": "reflect"}}, timeout=60)
    dt = time.monotonic() - t
    need(bool(f.get("error")) and dt < 5, f"forward to the dead peer refused in {dt:.1f}s: {short(f)}")

    P2 = boot_kernel(pspec, pwork, port)
    still = K.send("br", {"type": "reflect"})
    need(still.get("connected") is False,
         f"peer back on :{port} — no auto-reconnect (readme): connected={still.get('connected')}")
    rc = K.send("br", {"type": "reconnect"})
    need(rc.get("booted") is True, f"reconnect: {short(rc)}")
    g = P2.send(P2.root_id, {"type": "reflect"}).get("guid")
    rr = K.send("br", {"type": "forward", "target": "kernel_state", "payload": {"type": "reflect"}})
    need(rr.get("guid") == g and g, f"forward reaches the revived peer (guid {rr.get('guid')} vs {g})")
    return P2


def main() -> int:
    if PEER == RUNTIME:
        raise SystemExit("RUNTIME must be named")
    name, _ = runtime(REPO, RUNTIME)
    kspec, pspec = ensure_runtime(REPO, name), ensure_runtime(REPO, PEER)
    os.environ[PW_VAR] = PW  # the creds STORE reads it (env backend); before boot
    kwork = Path(tempfile.mkdtemp(prefix="e2e-bridges-rt-"))
    pwork = Path(tempfile.mkdtemp(prefix="e2e-bridges-peer-"))
    kblob, pblob = os.urandom(150_000), os.urandom(170_000)
    for w, blob in ((kwork, kblob), (pwork, pblob)):
        (w / "data").mkdir()
        (w / "data/blob.bin").write_bytes(blob)
    print(f"RUNTIME={RUNTIME} PEER={PEER}", flush=True)
    K = boot_kernel(kspec, kwork)
    P = boot_kernel(pspec, pwork)
    try:
        setup(K, P.port)
        setup(P, K.port)
        bridge_checks(f"{RUNTIME} bridge → {PEER} web_ws", K, P, pwork, pblob)
        bridge_checks(f"{PEER} bridge → {RUNTIME} web_ws", P, K, kwork, kblob)
        relay_checks(K, P, pblob)
        P = dead_peer_checks(K, P, pspec, pwork)
    finally:
        K.close()
        P.close()
        shutil.rmtree(kwork, ignore_errors=True)
        shutil.rmtree(pwork, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
