"""wire — the event stream, the WS frame protocol and the REST envelope
(`SCR/parity_capture_wire.py`, plus the `rest_extra`/`v_rest_probe` cases).

THE STORE IS WIRED ONLY AROUND THE SCHEDULER STEP. A store under a live
`state_subscribe` is a write loop that feeds itself: every ledger flush is an
execution, every execution is a state event, and round 2 measured ~30
write_stream/s and a 4 GB `frames.jsonl` in 25 seconds. The scheduler is the one
step python refuses without a store, so the store is created for it and deleted
after.

Three sockets, three authorities, one kernel:
  * `web` (main port) — an OPEN ws leg with grants: the control socket;
  * `web2/wsx` — an OPEN ws leg with NO target grants: the egress refusal;
  * `web3/wss` — a leg with NO ingress rule at all: SEALED, and every frame on it
    (call, emit, watch, state_subscribe, even a malformed one) has to be refused
    as the leg, which is what the web-edge change made true on every runtime.
"""

from __future__ import annotations

import asyncio
import hashlib
import http.client
import json
import re
import struct
import sys
import time

from families import BASE, OPEN, ai, call, save_kernel_log
from spec import CTX_RESERVE, CTX_WINDOW, wait_port
from stubs.anthropic_sse import AnthropicStub, Handler as AnthropicHandler

NAME = "wire"

CAP_BYTES = 65536
CAP_TEXT = 262144
CAP_HEAD = 256

CHAIN_SCRIPT = [
    'Checking the files agent now.\n<call name="send"><target_id>files</target_id><type>reflect</type></call>',
    'The files agent answered; finishing.\n<call name="done"></call>',
]
BUSY_SCRIPT = [
    'First turn, working.\n<call name="send"><target_id>files</target_id><type>reflect</type></call>',
    '<call name="done"></call>',
]

LEGS = ["rest", "web", "ws", "wsx", "wss", "rests", "web2", "web3"]


def roster(stub_url: str, cb: dict, interp: str) -> list:
    return [
        ("files", "file_bridge.tools", dict(allow=["."], root=".")),
        ("private", "file_bridge.tools", dict(allow=["."], root=".", callable_by={"$self": "*", "$parent": "*"})),
        ("cli", "cli.tools", {}),
        ("runner", "exec_bridge.tools", dict(cwd=".", allowed=[rf"{re.escape(interp)} .*"])),
        ("sched", "scheduler.tools", dict(can_call={**BASE, "cli": "*"})),
        ("wsb", "ws_bridge.tools", dict(transport="ws", host="127.0.0.1", port=9,
                                        peer_id="nowhere")),
        ("relay", "relay_connector.tools", dict(url="ws://127.0.0.1:9/nowhere",
                                                relay_url="ws://127.0.0.1:9/nowhere",
                                                guid="wirecap-guid", partner_guid="wirecap-partner",
                                                reconnect=3)),
        ("ai", "anthropic_backend.tools",
         ai(model="stub-model", endpoint=stub_url, context_window=CTX_WINDOW,
              output_reserve=CTX_RESERVE,
              can_call={**BASE, "files": "*", "cli": "*", "kernel": "*"})),
    ]


def seed_extra(k, w, port):
    """The extra authorities the wire family needs, written BEFORE the daemon.

    `state_subscribe` is privileged, so `"*"` does not cover it — the root names
    it for `ws` and `web` explicitly, which is the whole reason the granted-leg
    and ungranted-leg cases can be told apart at all.
    """
    from helpers.host.driver import ROOT_VERBS
    from spec import read_rec, write_rec

    root = read_rec(w / "agent.yaml")
    root["callable_by"] = {**root["callable_by"], "ws": ["*", "state_subscribe"],
                           "web": ["*", "state_subscribe"], "wsx": "*"}
    write_rec(w / "agent.yaml", root)
    for p in ("agents/web/agent.yaml", "agents/web/agents/ws/agent.yaml"):
        rec = read_rec(w / p)
        rec["can_call"][k.root_id] = ROOT_VERBS + ["state_subscribe"]
        write_rec(w / p, rec)
    reach = {t: "*" for t in k.reach if t != k.root_id}

    def put(rel_path: str, rec: dict) -> None:
        d = w / rel_path
        d.mkdir(parents=True, exist_ok=True)
        write_rec(d / "agent.yaml", rec)

    put("agents/web2", {"id": "web2", "handler_module": "web.tools", "parent_id": k.root_id,
                        "port": k.port2, "can_call": {**BASE, "kernel": "*", **reach}})
    # OPEN leg, NO target grants: the egress half alone.
    put("agents/web2/agents/wsx", {"id": "wsx", "handler_module": "web_ws.tools",
                                   "parent_id": "web2", "ingress_rule": "allow_all_ingress",
                                   "can_call": dict(BASE)})
    # NO ingress rule: SEALED, however well granted its egress is.
    put("agents/web2/agents/rests", {"id": "rests", "handler_module": "web_rest.tools",
                                     "parent_id": "web2",
                                     "can_call": {**BASE, "kernel": "*", **reach}})
    put("agents/web3", {"id": "web3", "handler_module": "web.tools", "parent_id": k.root_id,
                        "port": k.port3, "can_call": {**BASE, "kernel": "*", **reach}})
    put("agents/web3/agents/wss", {"id": "wss", "handler_module": "web_ws.tools",
                                   "parent_id": "web3",
                                   "can_call": {**BASE, "kernel": "*", **reach}})


class Recorder:
    """Every ws frame on every socket, in one ordered stream."""

    def __init__(self, rec, t0) -> None:
        self.rec = rec
        self.t0 = t0
        self.frames: list[dict] = []
        self.last_in = time.monotonic()

    def rel(self) -> float:
        return round(time.monotonic() - self.t0, 4)

    def add(self, sock: str, direction: str, **kw) -> dict:
        fr = {"harness": {"t": self.rel()}, "sock": sock, "dir": direction, **kw}
        self.frames.append(fr)
        if direction == "in":
            self.last_in = time.monotonic()
        self.rec.jsonl("frames", fr)
        return fr

    def mark(self, label: str, **kw) -> None:
        self.add("-", "mark", label=label, **kw)

    def msg(self, sock: str, direction: str, m) -> dict:
        if isinstance(m, str):
            if len(m) > CAP_TEXT:
                return self.add(sock, direction, kind="text", len=len(m), truncated=True,
                                sha256=hashlib.sha256(m.encode()).hexdigest(),
                                text_head=m[:CAP_HEAD * 8])
            try:
                parsed = json.loads(m)
            except ValueError:
                parsed = None
            return self.add(sock, direction, kind="text", text=m, json=parsed)
        raw = bytes(m)
        d = {"kind": "binary", "len": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
        if len(raw) <= CAP_BYTES:
            d["hex"] = raw.hex()
        else:
            d["hex_head"] = raw[:CAP_HEAD].hex()
        if len(raw) >= 4:
            hl = struct.unpack(">I", raw[:4])[0]
            d["head_len"] = hl
            if 4 + hl <= len(raw):
                try:
                    d["head"] = json.loads(raw[4:4 + hl].decode("utf-8"))
                except Exception as e:  # noqa: BLE001 - a bad head is data
                    d["head_error"] = repr(e)
                body = raw[4 + hl:]
                d["body_len"] = len(body)
                if len(body) <= CAP_BYTES:
                    d["body_hex"] = body.hex()
                else:
                    d["body_hex_head"] = body[:CAP_HEAD].hex()
                    d["body_sha256"] = hashlib.sha256(body).hexdigest()
        return self.add(sock, direction, **d)


def body_repr(raw):
    if raw is None:
        return None
    try:
        return {"len": len(raw), "text": raw.decode("utf-8")}
    except UnicodeDecodeError:
        return {"len": len(raw), "hex": raw.hex()}


class Sock:
    def __init__(self, W, name: str, port: int, path: str) -> None:
        self.W, self.name, self.port, self.path = W, name, port, path
        self.ws = None
        self.reader = None
        self.closed = True
        self.opens = 0
        self.handshakes: dict = {}

    @property
    def url(self) -> str:
        return f"ws://127.0.0.1:{self.port}{self.path}"

    async def open(self) -> bool:
        import websockets
        from websockets.asyncio.client import connect

        self.opens += 1
        key = f"{self.name}#{self.opens}"
        try:
            self.ws = await connect(self.url, origin=f"http://127.0.0.1:{self.port}",
                                    max_size=None, ping_interval=None, proxy=None,
                                    open_timeout=10, user_agent_header="wirecap/1")
        except websockets.exceptions.InvalidStatus as e:
            r = e.response
            self.handshakes[key] = {"url": self.url, "refused": True, "status": r.status_code,
                                    "reason": r.reason_phrase,
                                    "headers": list(r.headers.raw_items()),
                                    "body": body_repr(r.body or b"")}
            self.W.add(self.name, "open_refused", url=self.url, status=r.status_code)
            return False
        except Exception as e:  # noqa: BLE001 - a refused upgrade is data
            self.handshakes[key] = {"url": self.url, "refused": True, "error": repr(e)}
            self.W.add(self.name, "open_error", url=self.url, error=repr(e))
            return False
        r = self.ws.response
        self.handshakes[key] = {"url": self.url, "status": r.status_code,
                                "reason": r.reason_phrase, "headers": list(r.headers.raw_items())}
        self.W.add(self.name, "open", url=self.url, status=r.status_code)
        self.closed = False
        self.reader = asyncio.create_task(self._read())
        return True

    async def _read(self):
        ws = self.ws
        try:
            async for m in ws:
                self.W.msg(self.name, "in", m)
        except Exception as e:  # noqa: BLE001 - a close is data
            self.W.add(self.name, "read_error", error=repr(e))
        self.closed = True
        self.W.add(self.name, "close", code=ws.close_code, reason=ws.close_reason)

    async def ensure(self) -> bool:
        if self.closed:
            self.W.mark(f"reopen {self.name}")
            return await self.open()
        return True

    async def send(self, data) -> bool:
        if self.closed or self.ws is None:
            self.W.add(self.name, "send_skipped_closed")
            return False
        self.W.msg(self.name, "out", data)
        try:
            await self.ws.send(data)
            return True
        except Exception as e:  # noqa: BLE001
            self.W.add(self.name, "send_error", error=repr(e))
            return False

    async def close(self):
        if self.ws is not None and not self.closed:
            try:
                await self.ws.close()
            except Exception:  # noqa: BLE001
                pass
            if self.reader:
                try:
                    await asyncio.wait_for(self.reader, 5)
                except Exception:  # noqa: BLE001
                    pass


def binframe(head: dict, body: bytes) -> bytes:
    hb = json.dumps(head).encode()
    return struct.pack(">I", len(hb)) + hb + body


def run(ctx) -> None:  # noqa: C901 - one family, one shape; splitting it hides the order
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "hello.txt").write_bytes(b"hello wire\n")
    W = Recorder(rec, time.monotonic())

    stub = AnthropicStub(rec, "stub", ports.take("stub"))
    stub.start(AnthropicHandler)
    stub.set_chain("boot", CHAIN_SCRIPT)

    cb = {**{leg: "*" for leg in LEGS}, "ai": "*", "sched": "*"}
    agents = roster(stub.url(), cb, sys.executable)
    reach = [a[0] for a in agents] + ["store", "ghost"]
    port2, port3 = ports.take("web2"), ports.take("web3")

    def _seed(k, w, port):
        k.port2, k.port3 = port2, port3
        seed_extra(k, w, port)

    k = ctx.boot(reach=reach, port=ports.take("main"), seed_extra=_seed)
    PORTS = {"main": k.port, "web2": port2, "web3": port3}
    PROBES: dict = {}

    def rest_probe(name: str, target: str, payload: dict, timeout: float = 30):
        r = call(k, target, payload, timeout=timeout)
        if isinstance(r, dict):
            r = {kk: (v.hex() if isinstance(v, (bytes, bytearray)) else v) for kk, v in r.items()}
        PROBES[name] = {"harness": {"t": W.rel()}, "target": target, "payload": payload, "reply": r}
        return r

    def http_case(name: str, port: int, method: str, path: str, body=None,
                  headers=None, timeout: float = 30) -> dict:
        role = {v: kk for kk, v in PORTS.items()}.get(port, str(port))
        out = {"case": name, "harness": {"t": W.rel()},
               "request": {"method": method, "port_role": role, "path": path,
                           "headers": headers or {},
                           "body": body_repr(body) if body is not None else None}}
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
            c.request(method, path, body=body, headers=headers or {})
            r = c.getresponse()
            raw = r.read()
            out["response"] = {"status": r.status, "reason": r.reason, "version": r.version,
                               "headers": [[kk, v] for kk, v in r.getheaders()],
                               "body": body_repr(raw)}
            c.close()
        except Exception as e:  # noqa: BLE001 - a transport failure on a CASE is the answer
            out["response"] = {"__transport_error__": repr(e)}
        rec.jsonl("rest", out)
        return out

    async def wait_for(pred, timeout: float, start: int = 0, sock=None):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for f in W.frames[start:]:
                if sock is not None and f["sock"] != sock:
                    continue
                if f["dir"] == "in" and pred(f):
                    return f
            await asyncio.sleep(0.05)
        return None

    async def quiet(ms: int = 1200, max_s: float = 8.0) -> None:
        t_end = time.monotonic() + max_s
        await asyncio.sleep(0.2)
        while time.monotonic() < t_end:
            if time.monotonic() - W.last_in >= ms / 1000:
                return
            await asyncio.sleep(0.05)

    def is_reply(fid):
        def p(f):
            j = f.get("json") if f.get("kind") == "text" else f.get("head")
            return isinstance(j, dict) and j.get("type") in ("reply", "error") and j.get("id") == fid
        return p

    def event_type(tp: str):
        def p(f):
            j = f.get("json")
            return (isinstance(j, dict) and j.get("type") == "event"
                    and isinstance(j.get("payload"), dict) and j["payload"].get("type") == tp)
        return p

    CALL_N = [0]

    async def ws_call(sock, target: str, payload: dict, timeout: float = 30, fid=None):
        if fid is None:
            CALL_N[0] += 1
            fid = f"wirecap-{CALL_N[0]}"
        start = len(W.frames)
        await sock.send(json.dumps({"type": "call", "target": target, "payload": payload, "id": fid}))
        return await wait_for(is_reply(fid), timeout, start, sock.name)

    async def proto_case(sock, case: str, data, expect_id=None, window_ms: int = 1500,
                         max_s: float = 8.0) -> None:
        await sock.ensure()
        start = len(W.frames)
        t_send = W.rel()
        sent_ok = await sock.send(data)
        got = None
        if expect_id is not None:
            got = await wait_for(is_reply(expect_id), max_s, start, sock.name)
        await quiet(window_ms, max_s)
        rec.jsonl("protocol", {
            "case": case, "sock": sock.name, "url": sock.url,
            "harness": {"t_send": t_send}, "sent_ok": sent_ok,
            "sent": ({"kind": "text", "text": data} if isinstance(data, str)
                     else {"kind": "binary", "hex": bytes(data).hex()}),
            "expect_id": expect_id, "reply_found": got is not None,
            "frames": W.frames[start:], "socket_closed_after": sock.closed})

    # ── phases ───────────────────────────────────────────────────────────────
    async def phase_events(ctrl, auto_ai) -> None:
        W.mark("phase events")
        await ctrl.open()
        await auto_ai.open()
        await quiet(1000, 4)

        W.mark("discover ws client id")
        ps = rest_probe("kernel_ps_after_ws_open", "kernel", {"type": "ps"})
        la = rest_probe("kernel_list_agents_after_ws_open", "kernel", {"type": "list_agents"})
        found = sorted(set(re.findall(r"_ws_[0-9a-zA-Z_]+", json.dumps([ps, la], default=str))))
        ctx.meta["ws_client_ids_discovered"] = found

        W.mark("watch frames")
        for src in ("ai", "runner", "sched", "wsb", "relay"):
            await ctrl.send(json.dumps({"type": "watch", "src": src}))
        await quiet(1000, 4)
        # A registered watcher that never reads: C28/C29's subject is the agent
        # with NO inbox behind the registration.
        W.mark("watch plant (registered, no inbox)")
        await ctrl.send(json.dumps({"type": "watch", "src": "files"}))
        await quiet(600, 3)
        # Someone else's synthetic client id (C78): an emit naming an id this
        # socket did not mint.
        W.mark("emit naming another client's synthetic id")
        await ctrl.send(json.dumps({"type": "emit", "target": "_ws_deadbee",
                                    "payload": {"type": "wirecap_ping"}}))
        await quiet(600, 3)

        chains = []
        if found:
            chains.append(("ws_client", found[0]))
        chains += [("web_client", "web_wirecap01"), ("cli", "cli")]
        ctx.meta["chains"] = chains
        for label, cid in chains:
            W.mark(f"ai chain {label}", client_id=cid)
            stub.set_chain(label, CHAIN_SCRIPT)
            r = await ws_call(ctrl, "ai", {"type": "send",
                                           "text": "Reflect the files agent, then finish.",
                                           "client_id": cid}, timeout=150)
            if r is None:
                W.mark(f"ai chain {label}: no reply within 150s")
            await quiet(1500, 10)

        # B2.17 — a second send WHILE the first is in flight.
        W.mark("ai second send while busy")
        stub.set_chain("busy", [{"text": BUSY_SCRIPT[0], "stall": 3}, BUSY_SCRIPT[1]])
        start = len(W.frames)
        await ctrl.send(json.dumps({"type": "call", "target": "ai", "id": "wirecap-busy-1",
                                    "payload": {"type": "send", "text": "first, slow",
                                                "client_id": "busy"}}))
        await asyncio.sleep(0.7)
        await ctrl.send(json.dumps({"type": "call", "target": "ai", "id": "wirecap-busy-2",
                                    "payload": {"type": "send", "text": "second, while busy",
                                                "client_id": "busy"}}))
        await wait_for(is_reply("wirecap-busy-2"), 20, start, "ctrl")
        await quiet(1500, 20)

        W.mark("exec_bridge job")
        CALL_N[0] += 1
        fid = f"wirecap-{CALL_N[0]}"
        start = len(W.frames)
        rep = await ws_call(ctrl, "runner", {"type": "start", "command": sys.executable,
                                             "args": ["-c", "print(1)"]}, fid=fid)
        await wait_for(event_type("job_done"), 20, start, "ctrl")
        await quiet(800, 4)
        job_id = None
        try:
            job_id = (rep or {}).get("json", {}).get("data", {}).get("job_id")
        except Exception:  # noqa: BLE001
            pass
        await ws_call(ctrl, "runner", {"type": "status", **({"job_id": job_id} if job_id else {})})
        await quiet(600, 3)

        # A4.30/A4.12/A4.15 — stop a live job, two jobs at once, clear then progress.
        W.mark("exec_bridge stop a running job")
        start = len(W.frames)
        rep2 = await ws_call(ctrl, "runner", {"type": "start", "command": sys.executable,
                                              "args": ["-c", "import time; time.sleep(5)"]})
        jid2 = None
        try:
            jid2 = (rep2 or {}).get("json", {}).get("data", {}).get("job_id")
        except Exception:  # noqa: BLE001
            pass
        await asyncio.sleep(0.7)
        await ws_call(ctrl, "runner", {"type": "stop", **({"job_id": jid2} if jid2 else {})})
        await quiet(1000, 8)
        W.mark("exec_bridge two jobs then status")
        await ws_call(ctrl, "runner", {"type": "start", "command": sys.executable,
                                       "args": ["-c", "print('a')"]})
        await ws_call(ctrl, "runner", {"type": "start", "command": sys.executable,
                                       "args": ["-c", "print('b')"]})
        await quiet(1200, 8)
        await ws_call(ctrl, "runner", {"type": "status"})
        W.mark("exec_bridge clear then progress")
        await ws_call(ctrl, "runner", {"type": "clear"})
        await ws_call(ctrl, "runner", {"type": "start", "command": sys.executable,
                                       "args": ["-c", "print('after clear')"]})
        await quiet(1200, 8)
        # A4.11 — a job writing relay frames (null payload, target '') to the ctrl fd.
        W.mark("exec_bridge job writing control frames")
        prog = ("import json,os,sys\n"
                "fd=int(os.environ.get('FANTASTIC_CTRL_FD') or 0)\n"
                "f=os.fdopen(fd,'w') if fd else sys.stdout\n"
                "f.write(json.dumps({'type':'relay','target':'','payload':None})+'\\n')\n"
                "f.flush()\n")
        await ws_call(ctrl, "runner", {"type": "start", "command": sys.executable,
                                       "args": ["-c", prog]})
        await quiet(1500, 8)

        W.mark("scheduler fire: create store")
        PROBES["store_create"] = {"harness": {"t": W.rel()}, "reply": k.create(
            parent_id=k.root_id, handler_module="file_bridge.tools", allow=["."], id="store", **OPEN,
            root=".fantastic", callable_by=cb)}
        await asyncio.sleep(1.0)
        W.mark("scheduler fire")
        start = len(W.frames)
        rep = await ws_call(ctrl, "sched", {"type": "schedule", "target": "cli",
                                            "payload": {"type": "say", "text": "tick from sched"},
                                            "interval_seconds": 2})
        sid = None
        try:
            sid = rep["json"]["data"].get("schedule_id")
        except Exception:  # noqa: BLE001
            pass
        f1 = await wait_for(event_type("schedule_fired"), 15, start, "ctrl")
        if f1 is not None:
            await wait_for(lambda f: event_type("schedule_fired")(f) and f is not f1, 10,
                           W.frames.index(f1) + 1, "ctrl")
        await ws_call(ctrl, "sched", {"type": "history", "limit": 5})
        # A5.4 — boot an already-booted scheduler, twice.
        await ws_call(ctrl, "sched", {"type": "boot"})
        await ws_call(ctrl, "sched", {"type": "boot"})
        await ws_call(ctrl, "sched", {"type": "unschedule", **({"schedule_id": sid} if sid else {})})
        await quiet(800, 4)
        W.mark("scheduler fire: delete store")
        rest_probe("store_delete", k.root_id, {"type": "delete_agent", "id": "store"})
        await quiet(800, 4)

        W.mark("ws_bridge + relay unreachable")
        await ws_call(ctrl, "wsb", {"type": "reconnect"}, timeout=40)
        await quiet(1500, 6)
        await ws_call(ctrl, "wsb", {"type": "forward", "target": "files",
                                    "payload": {"type": "reflect"}, "timeout": 2}, timeout=40)
        await ws_call(ctrl, "wsb", {"type": "reflect"})
        await ws_call(ctrl, "relay", {"type": "reconnect"}, timeout=40)
        await quiet(1500, 6)
        await ws_call(ctrl, "relay", {"type": "reflect"})
        await quiet(1000, 4)

        W.mark("unwatch frames")
        for src in ("ai", "runner", "sched", "wsb", "relay", "files"):
            await ctrl.send(json.dumps({"type": "unwatch", "src": src}))
        await quiet(800, 3)
        W.mark("phase events end")

    async def phase_protocol(proto, wsx, sealed) -> None:
        W.mark("phase protocol")
        await proto.open()
        await quiet(800, 3)

        def C(**kw):
            return json.dumps({"type": "call", **kw})

        cases = [
            ("malformed_json_text", "{not json", None),
            ("json_array_text", "[1]", None),
            ("json_string_text", '"call"', None),
            ("empty_text", "", None),
            ("call_ok_reference", C(target="files", payload={"type": "reflect"}, id="p-ok-1"), "p-ok-1"),
            ("call_without_id", C(target="files", payload={"type": "reflect"}), ""),
            ("call_without_payload", C(target="files", id="p-nopayload-1"), "p-nopayload-1"),
            ("call_payload_not_object", C(target="files", payload="reflect", id="p-strpayload-1"), "p-strpayload-1"),
            ("call_without_target", C(payload={"type": "reflect"}, id="p-notarget-1"), "p-notarget-1"),
            ("call_unknown_target", C(target="ghost", payload={"type": "reflect"}, id="p-ghost-1"), "p-ghost-1"),
            ("call_target_refuses", C(target="private", payload={"type": "reflect"}, id="p-private-1"), "p-private-1"),
            ("call_target_not_in_leg_reach", C(target="outsider", payload={"type": "reflect"}, id="p-outsider-1"), "p-outsider-1"),
            ("call_id_with_colon", C(target="files", payload={"type": "reflect"}, id="p:colon"), "p:colon"),
            ("call_id_numeric", json.dumps({"type": "call", "target": "files", "payload": {"type": "reflect"}, "id": 7}), 7),
            ("call_id_reuse_same_target", C(target="files", payload={"type": "reflect"}, id="p-ok-1"), "p-ok-1"),
            ("call_id_reuse_other_target", C(target="cli", payload={"type": "reflect"}, id="p-ok-1"), "p-ok-1"),
            ("unknown_frame_type", json.dumps({"type": "bogus", "id": "p-bogus-1"}), None),
            ("frame_without_type", json.dumps({"id": "p-notype-1", "target": "files"}), None),
            ("text_call_read_stream", C(target="files", payload={"type": "read_stream", "path": "hello.txt", "offset": 0}, id="p-rs-1"), "p-rs-1"),
            ("text_call_write_stream_str_bytes", C(target="files", payload={"type": "write_stream", "path": "ws_text.txt", "offset": 0, "bytes": "xyz"}, id="p-ws-1"), "p-ws-1"),
            ("text_call_write_stream_no_bytes", C(target="files", payload={"type": "write_stream", "path": "ws_text2.txt", "offset": 0}, id="p-ws-2"), "p-ws-2"),
            ("binary_call_write_stream",
             binframe({"type": "call", "target": "files",
                       "payload": {"type": "write_stream", "path": "ws_bin.bin", "offset": 0, "bytes": None},
                       "id": "p-bin-1", "_binary_path": "payload.bytes"}, b"\x00\x01binary\xff"), "p-bin-1"),
            ("binary_call_read_stream_back",
             binframe({"type": "call", "target": "files",
                       "payload": {"type": "read_stream", "path": "ws_bin.bin", "offset": 0},
                       "id": "p-bin-2"}, b""), "p-bin-2"),
            ("binary_frame_too_short", b"\x00\x00\x00", None),
            ("binary_frame_header_overclaims", b"\x00\x00\x10\x00{}", None),
            ("binary_frame_bad_header_json", struct.pack(">I", 5) + b"{nope", None),
            ("binary_frame_bad_binary_path",
             binframe({"type": "call", "target": "files", "payload": {"type": "reflect"},
                       "id": "p-bin-3", "_binary_path": "payload.nope.deep"}, b"zz"), "p-bin-3"),
            ("emit_to_auto_watched_host", json.dumps({"type": "emit", "target": "web", "payload": {"type": "wirecap_ping", "n": 1}}), None),
            ("watch_no_src", json.dumps({"type": "watch"}), None),
            ("watch_unknown_agent", json.dumps({"type": "watch", "src": "ghost", "id": "p-w-ghost"}), None),
            ("watch_agent_that_refuses", json.dumps({"type": "watch", "src": "private", "id": "p-w-private"}), None),
            ("watch_agent_not_in_leg_reach", json.dumps({"type": "watch", "src": "outsider", "id": "p-w-outsider"}), None),
            ("watch_ok_ai", json.dumps({"type": "watch", "src": "ai"}), None),
            ("watch_duplicate_ai", json.dumps({"type": "watch", "src": "ai"}), None),
            ("unwatch_ai", json.dumps({"type": "unwatch", "src": "ai"}), None),
            ("unwatch_never_watched", json.dumps({"type": "unwatch", "src": "runner"}), None),
            ("state_subscribe_granted_leg", json.dumps({"type": "state_subscribe"}), None),
            ("call_while_state_subscribed", C(target="files", payload={"type": "reflect"}, id="p-st-1"), "p-st-1"),
            ("state_unsubscribe", json.dumps({"type": "state_unsubscribe"}), None),
            ("call_after_state_unsubscribe", C(target="files", payload={"type": "reflect"}, id="p-st-2"), "p-st-2"),
            ("state_unsubscribe_again", json.dumps({"type": "state_unsubscribe"}), None),
        ]
        for case, data, eid in cases:
            W.mark(f"proto {case}")
            await proto_case(proto, case, data, eid)

        W.mark("phase protocol: open leg without grants (web2/wsx)")
        await wsx.open()
        await quiet(800, 3)
        for case, data, eid in [
            ("wsx_call_target_not_granted", C(target="files", payload={"type": "reflect"}, id="x-1"), "x-1"),
            ("wsx_watch_not_granted", json.dumps({"type": "watch", "src": "ai", "id": "x-w-1"}), None),
            ("wsx_state_subscribe_not_granted", json.dumps({"type": "state_subscribe", "id": "x-s-1"}), None),
        ]:
            W.mark(f"proto {case}")
            await proto_case(wsx, case, data, eid)

        W.mark("phase protocol: SEALED leg (web3/wss), auto-watch of runner")
        ok = await sealed.open()
        await quiet(800, 3)
        if ok:
            for case, data, eid in [
                ("sealed_call", C(target="files", payload={"type": "reflect"}, id="s-1"), "s-1"),
                ("sealed_call_with_auth_token", json.dumps({"type": "call", "target": "files", "payload": {"type": "reflect"}, "id": "s-2", "auth_token": "nope"}), "s-2"),
                ("sealed_emit", json.dumps({"type": "emit", "target": "sched", "payload": {"type": "wirecap_ping"}}), None),
                ("sealed_watch", json.dumps({"type": "watch", "src": "ai", "id": "s-w-1"}), None),
                ("sealed_state_subscribe", json.dumps({"type": "state_subscribe", "id": "s-s-1"}), None),
                ("sealed_malformed", "{not json", None),
            ]:
                W.mark(f"proto {case}")
                await proto_case(sealed, case, data, eid)
            W.mark("proto sealed_autowatch_leak: a runner job via REST")
            start = len(W.frames)
            rest_probe("sealed_leak_runner_start", "runner",
                       {"type": "start", "command": sys.executable,
                        "args": ["-c", "print('sealed')"]})
            await asyncio.sleep(2.5)
            await quiet(800, 3)
            rec.jsonl("protocol", {"case": "sealed_autowatch_leak", "sock": sealed.name,
                                   "url": sealed.url,
                                   "frames": [f for f in W.frames[start:]
                                              if f["sock"] in (sealed.name, "-")]})
        else:
            rec.jsonl("protocol", {"case": "sealed_open", "sock": sealed.name, "url": sealed.url,
                                   "handshake": sealed.handshakes.get(f"{sealed.name}#1")})
        W.mark("phase protocol end")

    def phase_rest() -> None:
        P = PORTS["main"]
        J = {"content-type": "application/json"}
        OCT = "application/octet-stream"
        cases = [
            ("json_read_stream", "POST", "/rest/files", json.dumps({"type": "read_stream", "path": "hello.txt", "offset": 0}).encode(), J),
            ("json_read_stream_length", "POST", "/rest/files", json.dumps({"type": "read_stream", "path": "hello.txt", "offset": 2, "length": 4}).encode(), J),
            ("json_read_stream_missing_file", "POST", "/rest/files", json.dumps({"type": "read_stream", "path": "missing.txt", "offset": 0}).encode(), J),
            ("json_write_stream_str_bytes", "POST", "/rest/files", json.dumps({"type": "write_stream", "path": "rest_json.txt", "offset": 0, "bytes": "abc"}).encode(), J),
            ("json_write_stream_no_bytes", "POST", "/rest/files", json.dumps({"type": "write_stream", "path": "rest_json2.txt", "offset": 0}).encode(), J),
            ("octet_write_stream", "POST", "/rest/files", b"\x00\x01octet\xff",
             {"content-type": OCT, "X-Fantastic-Payload": json.dumps({"type": "write_stream", "path": "rest_oct.bin", "offset": 0}), "X-Fantastic-Bytes-Field": "bytes"}),
            ("octet_write_stream_no_field_header", "POST", "/rest/files", b"nofield",
             {"content-type": OCT, "X-Fantastic-Payload": json.dumps({"type": "write_stream", "path": "rest_oct2.bin", "offset": 0})}),
            ("octet_write_stream_custom_field", "POST", "/rest/files", b"custom",
             {"content-type": OCT, "X-Fantastic-Payload": json.dumps({"type": "write_stream", "path": "rest_oct3.bin", "offset": 0}), "X-Fantastic-Bytes-Field": "data"}),
            ("octet_read_stream", "POST", "/rest/files", b"",
             {"content-type": OCT, "X-Fantastic-Payload": json.dumps({"type": "read_stream", "path": "rest_oct.bin", "offset": 0}), "X-Fantastic-Bytes-Field": "bytes"}),
            ("octet_payload_header_malformed", "POST", "/rest/files", b"xyz",
             {"content-type": OCT, "X-Fantastic-Payload": "{nope", "X-Fantastic-Bytes-Field": "bytes"}),
            ("octet_payload_header_not_object", "POST", "/rest/files", b"xyz",
             {"content-type": OCT, "X-Fantastic-Payload": "[1,2]", "X-Fantastic-Bytes-Field": "bytes"}),
            ("octet_no_payload_header", "POST", "/rest/files", b"xyz", {"content-type": OCT}),
            # C34 — an octet POST to an id nobody created, and to a bare agent.
            ("octet_to_absent_agent", "POST", "/rest/absent", b"xyz",
             {"content-type": OCT, "X-Fantastic-Payload": json.dumps({"type": "write_stream", "path": "x", "offset": 0}), "X-Fantastic-Bytes-Field": "bytes"}),
            ("octet_to_root", "POST", "/rest/__ROOT__", b"xyz",
             {"content-type": OCT, "X-Fantastic-Payload": json.dumps({"type": "reflect"}), "X-Fantastic-Bytes-Field": "bytes"}),
            ("malformed_json", "POST", "/rest/files", b"{not json", J),
            ("json_array_body", "POST", "/rest/files", b"[1]", J),
            ("empty_body", "POST", "/rest/files", b"", J),
            ("empty_object_no_type", "POST", "/rest/files", b"{}", J),
            ("invalid_utf8_body", "POST", "/rest/files", b'{"type": "reflect", "x": "\xff\xfe"}', J),
            ("nan_body", "POST", "/rest/files", b'{"type": "reflect", "x": NaN}', J),
            ("wide_int_body", "POST", "/rest/files", b'{"type": "reflect", "x": 123456789012345678901234567890}', J),
            ("no_content_type", "POST", "/rest/files", json.dumps({"type": "reflect"}).encode(), {}),
            ("unknown_verb", "POST", "/rest/files", json.dumps({"type": "nope"}).encode(), J),
            ("missing_target_trailing_slash", "POST", "/rest/", json.dumps({"type": "reflect"}).encode(), J),
            ("missing_target_no_slash", "POST", "/rest", json.dumps({"type": "reflect"}).encode(), J),
            ("unknown_agent", "POST", "/rest/ghost", json.dumps({"type": "reflect"}).encode(), J),
            ("unauthorized_target_ingress", "POST", "/rest/private", json.dumps({"type": "reflect"}).encode(), J),
            ("unauthorized_target_egress", "POST", "/rest/outsider", json.dumps({"type": "reflect"}).encode(), J),
            ("get_target", "GET", "/rest/files", None, {}),
            ("get_reflect_root_tree_none", "GET", "/rest/_reflect?tree=none", None, {}),
            ("get_reflect_flags", "GET", "/rest/_reflect?readme=true&tree=ids&bundles=ids", None, {}),
            ("get_reflect_target", "GET", "/rest/_reflect/files", None, {}),
            ("get_reflect_unknown", "GET", "/rest/_reflect/ghost", None, {}),
            ("get_reflect_unauthorized", "GET", "/rest/_reflect/private", None, {}),
            ("options_target", "OPTIONS", "/rest/files", None, {}),
            ("options_cors_preflight", "OPTIONS", "/rest/files", None,
             {"Origin": "http://example.test", "Access-Control-Request-Method": "POST",
              "Access-Control-Request-Headers": "content-type"}),
            ("put_target", "PUT", "/rest/files", json.dumps({"type": "reflect"}).encode(), J),
            ("null_reply_cli_say", "POST", "/rest/cli", json.dumps({"type": "say", "text": "hello from rest", "source": "wirecap"}).encode(), J),
            # A9.5–A9.7: the cli bundle's own status/tool_calling payloads, read
            # back out of the kernel log.
            ("cli_status", "POST", "/rest/cli", json.dumps({"type": "status", "text": "wirecap status", "state": "busy"}).encode(), J),
            ("cli_tool_calling", "POST", "/rest/cli", json.dumps({"type": "tool_calling", "calls": [{"name": "send", "target_id": "files"}]}).encode(), J),
            ("cli_say_non_string", "POST", "/rest/cli", json.dumps({"type": "say", "text": 42}).encode(), J),
            ("null_reply_rest_boot_self", "POST", "/rest/rest", json.dumps({"type": "boot"}).encode(), J),
            # A11.4/A11.5/A11.28 — the leg's own verbs, asked of the leg.
            ("rest_self_nope", "POST", "/rest/rest", json.dumps({"type": "nope"}).encode(), J),
            ("rest_self_shutdown", "POST", "/rest/rest", json.dumps({"type": "shutdown"}).encode(), J),
            ("rest_self_get_routes", "POST", "/rest/rest", json.dumps({"type": "get_routes"}).encode(), J),
            ("rest_self_handle_route", "POST", "/rest/rest", json.dumps({"type": "handle_route", "path": "/x"}).encode(), J),
            ("exec_id_first", "POST", "/rest/files", json.dumps({"type": "reflect"}).encode(), {**J, "X-Fantastic-Execution-Id": "wirecap-exec-1"}),
            ("exec_id_repeat_same_target", "POST", "/rest/files", json.dumps({"type": "reflect"}).encode(), {**J, "X-Fantastic-Execution-Id": "wirecap-exec-1"}),
            ("exec_id_repeat_other_target", "POST", "/rest/cli", json.dumps({"type": "reflect"}).encode(), {**J, "X-Fantastic-Execution-Id": "wirecap-exec-1"}),
            ("exec_id_with_colon", "POST", "/rest/files", json.dumps({"type": "reflect"}).encode(), {**J, "X-Fantastic-Execution-Id": "a:b"}),
            ("auth_header_on_open_leg", "POST", "/rest/files", json.dumps({"type": "reflect"}).encode(), {**J, "X-Fantastic-Auth": "whatever"}),
            ("fileproxy_ok", "GET", "/files/file/hello.txt", None, {}),
            ("fileproxy_missing", "GET", "/files/file/missing.txt", None, {}),
            ("fileproxy_denied", "GET", "/private/file/hello.txt", None, {}),
            ("fileproxy_unknown_agent", "GET", "/ghost/file/x.txt", None, {}),
            ("fileproxy_pdf_extension", "GET", "/files/file/doc.pdf", None, {}),
            ("fileproxy_unknown_extension", "GET", "/files/file/thing.zzz", None, {}),
            # A11.3 / D11 — the extras python gets from FastAPI and the ports serve
            # themselves. Asked on every runtime, answered differently on purpose.
            ("get_index", "GET", "/", None, {}),
            ("get_transport_js", "GET", "/transport.js", None, {}),
            ("get_asset", "GET", "/_assets/x", None, {}),
            ("get_files_slash", "GET", "/files/", None, {}),
            ("get_openapi", "GET", "/openapi.json", None, {}),
            ("get_docs", "GET", "/docs", None, {}),
        ]
        (ctx.work / "doc.pdf").write_bytes(b"%PDF-1.4 stub\n")
        (ctx.work / "thing.zzz").write_bytes(b"zzz\n")
        for name, method, path, body, headers in cases:
            http_case(name, P, method, path.replace("__ROOT__", k.root_id), body, headers)
        # Two requests on ONE keep-alive connection (A13.x): the second must not
        # inherit anything from the first.
        try:
            c = http.client.HTTPConnection("127.0.0.1", P, timeout=30)
            out = []
            for i in (1, 2):
                c.request("POST", "/rest/files", body=json.dumps({"type": "reflect"}).encode(),
                          headers={"content-type": "application/json", "connection": "keep-alive"})
                r = c.getresponse()
                raw = r.read()
                out.append({"n": i, "status": r.status, "reason": r.reason,
                            "headers": [[kk, v] for kk, v in r.getheaders()],
                            "body": body_repr(raw)})
            c.close()
            rec.jsonl("rest", {"case": "keepalive_two_requests", "responses": out})
        except Exception as e:  # noqa: BLE001
            rec.jsonl("rest", {"case": "keepalive_two_requests", "__transport_error__": repr(e)})
        # C22 — the SAME execution id on create_agent with two different parents.
        for n, parent in ((1, k.root_id), (2, "files")):
            http_case(f"exec_id_create_parent_{n}", P, "POST", f"/rest/{k.root_id}",
                      json.dumps({"type": "create_agent", "id": f"joiner{n}",
                                  "parent_id": parent, "handler_module": "cli.tools"}).encode(),
                      {"content-type": "application/json",
                       "X-Fantastic-Execution-Id": "wirecap-joined-1"})
        P2 = PORTS["web2"]
        http_case("sealed_leg_post", P2, "POST", "/rests/files", json.dumps({"type": "reflect"}).encode(), J)
        http_case("sealed_leg_post_with_auth", P2, "POST", "/rests/files",
                  json.dumps({"type": "reflect"}).encode(), {**J, "X-Fantastic-Auth": "nope"})
        http_case("sealed_leg_octet", P2, "POST", "/rests/files", b"xyz",
                  {"content-type": OCT,
                   "X-Fantastic-Payload": json.dumps({"type": "write_stream", "path": "sealed.bin", "offset": 0}),
                   "X-Fantastic-Bytes-Field": "bytes"})
        http_case("sealed_leg_get_reflect", P2, "GET", "/rests/_reflect/files", None, {})
        if ctx.slow:
            # A13.18 — 45 s of socket idle, and the 2 MB body. Only under --slow:
            # they add a minute per runtime and measure a library default (D16).
            big = json.dumps({"type": "write_stream", "path": "big.bin", "offset": 0,
                              "bytes": "x" * (2 * 1024 * 1024)}).encode()
            http_case("body_2mb", P, "POST", "/rest/files", big, J, timeout=120)

    async def main() -> None:
        ctrl = Sock(W, "ctrl", PORTS["main"], "/web/ws")
        auto_ai = Sock(W, "auto_ai", PORTS["main"], "/ai/ws")
        proto = Sock(W, "proto", PORTS["main"], "/web/ws")
        wsx = Sock(W, "wsx", PORTS["web2"], "/web2/ws")
        sealed = Sock(W, "sealed", PORTS["web3"], "/runner/ws")
        socks = (ctrl, auto_ai, proto, wsx, sealed)
        try:
            await phase_events(ctrl, auto_ai)
            await phase_protocol(proto, wsx, sealed)
            W.mark("phase rest")
            await asyncio.to_thread(phase_rest)
            await quiet(1000, 4)
            W.mark("phase rest end")
        finally:
            for s in socks:
                await s.close()
            hs: dict = {}
            for s in socks:
                hs.update(s.handshakes)
            rec.json("handshakes", hs)

    try:
        setup = {"web2_up": wait_port(port2), "web3_up": wait_port(port3), "creates": {}}
        for aid, hm, meta in agents:
            setup["creates"][aid] = k.create(
                **{"parent_id": k.root_id, "handler_module": hm, "id": aid,
                   **OPEN, "callable_by": cb, **meta})
        # `outsider` is created but named in NO leg's reach: the egress refusal.
        setup["creates"]["outsider"] = k.create(
            parent_id=k.root_id, id="outsider", handler_module="file_bridge.tools", allow=["."],
            root=".", **OPEN, callable_by=cb)
        rec.json("setup", setup)
        time.sleep(1.5)
        asyncio.run(main())
        written = {}
        for n in ("ws_text.txt", "ws_text2.txt", "ws_bin.bin", "rest_json.txt", "rest_json2.txt",
                  "rest_oct.bin", "rest_oct2.bin", "rest_oct3.bin", "sealed.bin"):
            p = ctx.work / n
            written[n] = p.read_bytes().hex() if p.exists() else None
        rec.json("written_files", written)
    finally:
        rec.json("probes", PROBES)
        rec.json("frames_index", {"total": len(W.frames)})
        stub.flush()
        rec.http("http", k.http)
        k.close()
        stub.stop()
        save_kernel_log(rec, k)
