"""fantastic_agent — the reference helper a py_agent program imports (stdlib only).

Copy this file next to the program in its bundle folder:

    import fantastic_agent as fa

    def search(p):                       # p = the call's payload: {"type": "search", ...}
        hits = ...
        fa.emit({"type": "progress", "found": len(hits)})   # an event on this agent
        return {"hits": hits}             # the answer; a refusal is {"error", "reason"?, "hint"?}

    fa.serve({"search": search})         # says hello with these verbs, then answers calls

`fa.send(target, payload)` calls another agent AS this agent (its grants decide) and
returns the answer. Calls run one at a time, in order, on a worker thread, so a
handler may itself `send`. The kernel closing the channel ends the program.

Wire: frames `[4-byte BE length | kind | body]` on the inherited fd FANTASTIC_CTRL_FD —
kind `T` UTF-8 JSON, kind `B` the io_bridge binary frame `[4-byte BE H | H-byte JSON
header | raw body]` (header `_binary_path` names where the raw body belongs).
"""

from __future__ import annotations

import json
import os
import queue
import socket
import struct
import threading
import traceback

SELF_ID = os.environ.get("FANTASTIC_SELF_ID")
MAX_FRAME = 16 * 1024 * 1024

_sock = socket.socket(fileno=int(os.environ["FANTASTIC_CTRL_FD"]))
_wlock = threading.Lock()
_pending: dict = {}
_rid = [0]


def _write(obj: dict) -> None:
    data = b"T" + json.dumps(obj).encode("utf-8")
    if len(data) > MAX_FRAME:
        raise ValueError("fantastic_agent: frame exceeds 16 MiB — stream it in chunks")
    with _wlock:
        _sock.sendall(struct.pack(">I", len(data)) + data)


def _read_exact(n: int) -> bytes | None:
    buf = b""
    while len(buf) < n:
        chunk = _sock.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def _unframe(body: bytes) -> dict:
    if body[:1] == b"B":
        (hl,) = struct.unpack(">I", body[1:5])
        msg = json.loads(body[5 : 5 + hl].decode("utf-8"))
        path = msg.pop("_binary_path", None)
        if path:
            node = msg
            for step in path[:-1]:
                node = node[step]
            node[path[-1]] = body[5 + hl :]
        return msg
    return json.loads(body[1:].decode("utf-8"))


def send(target: str, payload: dict | None = None):
    """Call `target` as this agent; returns its answer."""
    with _wlock:
        _rid[0] += 1
        rid = str(_rid[0])
    q: queue.Queue = queue.Queue()
    _pending[rid] = q
    _write({"op": "send", "rid": rid, "target": target, "payload": payload or {}})
    return q.get().get("data")


def emit(payload: dict) -> None:
    """An event on this agent's inbox — its watchers (a panel) see it."""
    _write({"op": "emit", "payload": payload})


def serve(handlers: dict) -> None:
    """Say hello with `handlers`' verbs, then answer every call until the kernel
    closes the channel."""
    _write({"op": "hello", "verbs": sorted(handlers)})
    calls: queue.Queue = queue.Queue()

    def _worker() -> None:
        while True:
            m = calls.get()
            payload = m.get("payload") or {}
            fn = handlers.get(payload.get("type"))
            if fn is None:
                result = {
                    "error": f"no verb {payload.get('type')!r}",
                    "reason": "unhandled",
                }
            else:
                try:
                    result = fn(payload)
                except Exception as e:  # noqa: BLE001 — the raise is the answer
                    traceback.print_exc()
                    result = {
                        "error": f"{payload.get('type')} raised {type(e).__name__}: {e}"
                    }
            _write({"op": "result", "rid": m.get("rid"), "result": result})

    threading.Thread(target=_worker, daemon=True).start()
    while True:
        head = _read_exact(4)
        if head is None:
            break
        (n,) = struct.unpack(">I", head)
        body = _read_exact(n)
        if body is None:
            break
        try:
            m = _unframe(body)
        except (ValueError, KeyError, IndexError, struct.error):
            continue
        op = m.get("op")
        if op == "call":
            calls.put(m)
        elif op == "reply":
            q = _pending.pop(m.get("rid"), None)
            if q is not None:
                q.put(m)
    os._exit(0)  # the kernel is gone
