"""kernel/secret_marks — SECRET MARKS on a send item, over the wire. NO MODEL.

A send item may carry `secret: ["<dotted path>", …]` (paths into its payload).
The HANDLER gets the payload as sent; everything the kernel keeps or shows of the
call carries it AS SHOWN — each marked value `[secret]`. On the PACKAGED host of
`RUNTIME` (python | rust | swift), every call through the wire faces:

  1. REST `X-Fantastic-Secret: bytes` on a `file_bridge` write → the file on disk
     and a read-back hold the CLEAR value (the handler got it);
  2. a ws `call` frame's `secret` → the same, through the other face;
  3. a NESTED mark (`cfg.x`) on `update_agent` → the record holds `cfg.x` clear,
     its sibling `cfg.y` untouched;
  4. the CALL ROWS in the memory store show `[secret]` at each marked path and
     record the marks (`row.secret`); the leg's own `inbound_call` row is marked
     under `item.payload.`; the clear value appears NOWHERE in the store;
  5. the STATE STREAM (`payload`, `summary`) and the INBOX MIRROR to a watcher of
     the target show `[secret]`, never the clear value;
  6. a mark naming nothing is skipped (the call runs);
  7. a malformed field is `bad_arg` `secret` (header `a..b`, a ws frame whose
     `secret` is not a list); marking `type` is refused — and the refused write
     never ran.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import shutil
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import memory
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.host.ws import StateWatcher, WSWatcher
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
SHOWN = "[secret]"
#: One clear value per channel, so a hit names the channel that leaked it.
S_REST = "rest-" + secrets.token_hex(10)
S_WS = "ws-" + secrets.token_hex(10)
S_NEST = "nest-" + secrets.token_hex(10)
S_BAD = "bad-" + secrets.token_hex(10)
FB = "fb"


# ─── wire helpers (local: the driver's `send` carries no secret header) ──


def post(port: int, target: str, payload: dict, headers: dict | None = None) -> dict:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/rest/{target}",
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json", **(headers or {})},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read() or b"null") or {}
    except urllib.error.HTTPError as e:
        return {"error": f"HTTP {e.code}", "body": e.read().decode()[:300]}


class WSCaller(WSWatcher):
    """One ws connection that sends `call` frames and reads their `reply`."""

    async def connect(self) -> None:
        saved, self.agent_id = self.agent_id, None
        # the handshake of WSWatcher, minus its `watch` frame
        import base64

        self._r, self._w = await asyncio.open_connection("127.0.0.1", self.port)
        key = base64.b64encode(os.urandom(16)).decode()
        self._w.write(
            f"GET /{self.web_id}/ws HTTP/1.1\r\nHost: 127.0.0.1:{self.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        await self._w.drain()
        head = await self._r.readuntil(b"\r\n\r\n")
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError(f"ws upgrade refused: {head[:120]!r}")
        self.agent_id = saved

    async def call(self, frame: dict, timeout: float = 30) -> dict:
        await self._send(json.dumps(frame))

        async def _reply():
            while True:
                raw = await self._frame()
                if raw is None:
                    return {"error": "ws closed before the reply"}
                if not raw:
                    continue
                try:
                    f = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                if isinstance(f, dict) and f.get("type") == "reply" and f.get("id") == frame["id"]:
                    return f.get("data") or {}

        return await asyncio.wait_for(_reply(), timeout)


class Taps:
    """The state stream and an inbox watch of `FB`, collected on a loop thread."""

    def __init__(self, port: int):
        self.port = port
        self.state: list[dict] = []
        self.inbox: list[dict] = []
        self.ready = threading.Event()
        self.errors: list[str] = []
        self.loop = asyncio.new_event_loop()
        self.th = threading.Thread(target=self.loop.run_forever, daemon=True)
        self.th.start()
        self.sw = StateWatcher(port, "state")
        self.ww = WSWatcher(port, FB)
        self.tasks = [
            asyncio.run_coroutine_threadsafe(self._state(), self.loop),
            asyncio.run_coroutine_threadsafe(self._inbox(), self.loop),
        ]

    async def _state(self):
        try:
            await self.sw.connect()
            async for f in self.sw.state_events():
                self.state.append(f)
        except Exception as e:  # noqa: BLE001 — reported as a check
            self.errors.append(f"state: {e}")

    async def _inbox(self):
        try:
            await self.ww.connect()
            async for p in self.ww.events():
                self.inbox.append(p)
        except Exception as e:  # noqa: BLE001
            self.errors.append(f"inbox: {e}")

    def ws_call(self, frame: dict) -> dict:
        async def go():
            c = WSCaller(self.port, "")
            await c.connect()
            try:
                return await c.call(frame)
            finally:
                await c.close()

        return asyncio.run_coroutine_threadsafe(go(), self.loop).result(60)

    def close(self):
        for w in (self.sw, self.ww):
            asyncio.run_coroutine_threadsafe(w.close(), self.loop).result(10)
        for t in self.tasks:
            t.cancel()
        self.loop.call_soon_threadsafe(self.loop.stop)
        self.th.join(5)


def poll(fn, what: str, bound: float = 15.0):
    """`fn()` until truthy, bounded — never a sleep as sync."""
    end = time.monotonic() + bound
    while True:
        v = fn()
        if v or time.monotonic() > end:
            return v
        time.sleep(0.1)


def dig(d, path: str):
    for k in path.split("."):
        if not isinstance(d, dict) or k not in d:
            return None
        d = d[k]
    return d


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-secret-marks-"))
    served = work / "served"
    served.mkdir()
    hk = HostKernel(spec, work, reach=[FB]).boot()
    root = hk.root_id
    port = hk.port
    bad: list[str] = []
    taps: Taps | None = None

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def store_rows() -> list[dict]:
        d = hk.send(memory.MEMORY, {"type": "dump"})
        return [r.get("row") or {} for r in (d.get("tables") or {}).get("rows") or []]

    def call_rows(verb: str, agent: str, caller: str) -> list[dict]:
        return [
            r for r in store_rows()
            if r.get("kind") == "call" and r.get("verb") == verb
            and r.get("agent") == agent and r.get("caller") == caller
        ]

    try:
        rec = hk.create(
            parent_id="rest",
            handler_module="file_bridge.tools",
            id=FB,
            root=str(served),
            allow=["."],
            ingress_rule="allow_all_ingress",
            callable_by={"$self": "*", "$parent": "*", "ws": "*"},
        )
        need(rec.get("id") == FB, f"create {FB}: {str(rec)[:200]}")
        taps = Taps(port)
        # Both taps are live once a probe call shows up on each (no sleep-as-sync).
        warm = poll(
            lambda: (hk.send(FB, {"type": "list", "path": ""}) is not None)
            and any((f.get("payload") or {}).get("agent_id") == FB or f.get("agent_id") == FB
                    for f in taps.state)
            and any(p.get("type") == "list" for p in taps.inbox),
            "taps", bound=20,
        )
        need(bool(warm), f"state stream + inbox watch of {FB} are live (errors: {taps.errors})")

        # 1 · REST header — the handler gets the clear value
        r = post(port, FB, {"type": "write", "path": "rest.txt", "bytes": S_REST},
                 {"X-Fantastic-Secret": "bytes"})
        need("error" not in r, f"REST marked write accepted: {str(r)[:200]}")
        need((served / "rest.txt").exists() and (served / "rest.txt").read_text() == S_REST,
             "the file on disk holds the CLEAR value (REST)")

        # 2 · ws call frame — the same through the other face
        r = taps.ws_call({"type": "call", "id": "e113-ws-1", "target": FB,
                          "payload": {"type": "write", "path": "ws.txt", "bytes": S_WS},
                          "secret": ["bytes"]})
        need("error" not in r, f"ws marked write accepted: {str(r)[:200]}")
        need((served / "ws.txt").exists() and (served / "ws.txt").read_text() == S_WS,
             "the file on disk holds the CLEAR value (ws)")

        # 3 · a NESTED mark on update_agent — stored clear, sibling untouched
        r = post(port, root, {"type": "update_agent", "id": FB,
                              "cfg": {"x": S_NEST, "y": "plain"}},
                 {"X-Fantastic-Secret": "cfg.x"})
        need("error" not in r, f"marked update_agent accepted: {str(r)[:200]}")
        la = hk.send(root, {"type": "list_agents"})
        fbrec = next((a for a in la.get("agents") or [] if a.get("id") == FB), {})
        need(fbrec.get("cfg") == {"x": S_NEST, "y": "plain"},
             f"the record holds cfg.x CLEAR, cfg.y as sent: {fbrec.get('cfg')!r}")

        # 4 · the call rows in the memory store
        # each face's leg is the caller of its onward call
        poll(lambda: call_rows("write", FB, "rest") and call_rows("write", FB, "ws"), "rows")
        by_path = {dig(r, "payload.path"): r
                   for r in call_rows("write", FB, "rest") + call_rows("write", FB, "ws")}
        rw, ww = by_path.get("rest.txt") or {}, by_path.get("ws.txt") or {}
        need(dig(rw, "payload.bytes") == SHOWN,
             f"REST write's call row shows bytes={dig(rw, 'payload.bytes')!r}")
        need(rw.get("secret") == ["bytes"], f"REST write's row records the marks: {rw.get('secret')!r}")
        need(dig(ww, "payload.bytes") == SHOWN,
             f"ws write's call row shows bytes={dig(ww, 'payload.bytes')!r}")
        need(ww.get("secret") == ["bytes"], f"ws write's row records the marks: {ww.get('secret')!r}")
        legs = [r for r in store_rows() if r.get("kind") == "call" and r.get("verb") == "inbound_call"
                and dig(r, "payload.item.target") == FB and dig(r, "payload.item.payload.type") == "write"]
        need(len(legs) >= 2 and all(dig(r, "payload.item.payload.bytes") == SHOWN for r in legs),
             f"the legs' inbound_call rows show {[dig(r, 'payload.item.payload.bytes') for r in legs]}")
        need(all(r.get("secret") == ["item.payload.bytes"] for r in legs) and legs != [],
             f"the legs' rows are marked under item.payload.: {[r.get('secret') for r in legs]}")
        up = [r for r in call_rows("update_agent", root, "rest") if dig(r, "payload.id") == FB
              and isinstance(dig(r, "payload.cfg"), dict)]
        u = up[-1] if up else {}
        need(dig(u, "payload.cfg.x") == SHOWN and dig(u, "payload.cfg.y") == "plain",
             f"update_agent's row shows cfg={dig(u, 'payload.cfg')!r}")
        need(u.get("secret") == ["cfg.x"], f"update_agent's row records the marks: {u.get('secret')!r}")
        blob = json.dumps(store_rows())
        need(S_REST not in blob and S_WS not in blob,
             "the clear file values appear NOWHERE in the memory store")

        # 5 · the state stream and the inbox mirror
        def sent_events():
            return [f for f in taps.state if f.get("kind") == "send"
                    and f.get("agent_id") == FB and (f.get("payload") or {}).get("type") == "write"]

        evs = poll(lambda: len(sent_events()) >= 2 and sent_events(), "state")
        need(bool(evs) and all(dig(f, "payload.bytes") == SHOWN for f in evs),
             f"state stream: {FB}'s write events show {[dig(f, 'payload.bytes') for f in evs or []]}")
        sblob = json.dumps(taps.state)
        need(S_REST not in sblob and S_WS not in sblob,
             "the clear file values appear NOWHERE on the state stream (payload, summary)")
        upd = [f for f in taps.state if f.get("kind") == "send"
               and (f.get("payload") or {}).get("type") == "update_agent"]
        need(bool(upd) and all(dig(f, "payload.cfg.x") == SHOWN for f in upd),
             f"state stream: update_agent's send event shows cfg.x={[dig(f, 'payload.cfg.x') for f in upd]}")
        need(all(S_NEST not in json.dumps({k: f.get(k) for k in ('payload', 'summary')}) for f in upd),
             "state stream: update_agent's payload/summary never carry cfg.x clear")
        mir = poll(lambda: [p for p in taps.inbox if p.get("type") == "write"]
                   if len([p for p in taps.inbox if p.get("type") == "write"]) >= 2 else None, "inbox")
        need(bool(mir) and all(p.get("bytes") == SHOWN for p in mir),
             f"inbox mirror to the watcher shows {[p.get('bytes') for p in mir or []]}")
        need(S_REST not in json.dumps(taps.inbox) and S_WS not in json.dumps(taps.inbox),
             "the clear file values appear NOWHERE in the inbox mirror")

        # read back AFTER the leak checks (a read's reply is the handler's answer)
        rb = hk.send(FB, {"type": "read", "path": "rest.txt"})
        need(S_REST in json.dumps(rb), f"read-back returns the CLEAR value: {str(rb)[:160]}")

        # 6 · a mark naming nothing is skipped
        r = post(port, FB, {"type": "write", "path": "skip.txt", "bytes": "x"},
                 {"X-Fantastic-Secret": "nosuch.path"})
        need("error" not in r and (served / "skip.txt").exists(),
             f"a mark naming nothing is skipped, the call runs: {str(r)[:160]}")

        # 7 · malformed / `type` — bad_arg secret, and the write never ran
        r = post(port, FB, {"type": "write", "path": "bad1.txt", "bytes": S_BAD},
                 {"X-Fantastic-Secret": "a..b"})
        need(r.get("reason") == "bad_arg" and r.get("arg") == "secret",
             f"header `a..b` → bad_arg secret: {str(r)[:200]}")
        r = taps.ws_call({"type": "call", "id": "e113-ws-bad", "target": FB,
                          "payload": {"type": "write", "path": "bad2.txt", "bytes": S_BAD},
                          "secret": "bytes"})
        need(r.get("reason") == "bad_arg" and r.get("arg") == "secret",
             f"ws `secret` not a list → bad_arg secret: {str(r)[:200]}")
        r = post(port, FB, {"type": "write", "path": "bad3.txt", "bytes": S_BAD},
                 {"X-Fantastic-Secret": "type"})
        need(r.get("reason") == "bad_arg" and r.get("arg") == "secret",
             f"marking `type` → bad_arg secret: {str(r)[:200]}")
        r = taps.ws_call({"type": "call", "id": "e113-ws-type", "target": FB,
                          "payload": {"type": "write", "path": "bad4.txt", "bytes": S_BAD},
                          "secret": ["type"]})
        need(r.get("reason") == "bad_arg" and r.get("arg") == "secret",
             f"ws marking `type` → bad_arg secret: {str(r)[:200]}")
        need(not any((served / f"bad{i}.txt").exists() for i in range(1, 5)),
             "no refused write ran")
    finally:
        if taps is not None:
            taps.close()
        hk.close()
        shutil.rmtree(work, ignore_errors=True)

    verdict(
        not bad,
        f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""),
    )
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
