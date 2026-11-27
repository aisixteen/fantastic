"""THE SHELL'S CALL INTO A RUNNING KERNEL, test side (#795): one MCP session on the
kernel's channel (`<state>/cli/mcp.yaml`: a loopback port and the token a connection
opens with), spoken the way `fantastic send` speaks it.

EVERYTHING ASYNC: the `send` tool answers only the ACK (`{pid, status: accepted}`), and
`fantastic send` prints just that. The result comes later as the session's server
notification `notifications/settle`, params `{type: settle, re, status, data}`: `re` is
the ack's pid, `data.outcomes` one row `{pid, status, result}` per item. A test that
needs the result reads it there:

    row = call_settled(rig.proj / ".fantastic", "kernel", {"type": "reflect"})
    row["result"]
"""

from __future__ import annotations

import json
import socket
import time
from pathlib import Path

import yaml

SETTLE = "notifications/settle"


class Session:
    """One MCP client session; `timeout` bounds every read (a hang guard, it fails)."""

    def __init__(self, state: Path, timeout: float = 60) -> None:
        ep = yaml.safe_load((Path(state) / "cli" / "mcp.yaml").read_text())
        self.deadline = time.monotonic() + timeout
        self.sock = socket.create_connection(("127.0.0.1", int(ep["port"])), timeout=timeout)
        self.sock.sendall(str(ep["token"]).encode())
        self.buf = b""
        self.write({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                               "clientInfo": {"name": "e2e", "version": "0"}}})
        self.write({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.next_id = 2
        # settle notifications read before they were asked for: a call that settles
        # at once may have its settle written before its ack
        self.early: list[dict] = []

    def write(self, msg: dict) -> None:
        self.sock.sendall((json.dumps(msg) + "\n").encode())

    def read(self) -> dict | None:
        """The next JSON line, or None when the kernel closed the channel."""
        while b"\n" not in self.buf:
            left = self.deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("the MCP session got no answer in time")
            self.sock.settimeout(left)
            chunk = self.sock.recv(65536)
            if not chunk:
                return None
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        try:
            return json.loads(line)
        except ValueError:
            return {}

    def send(self, items: list[dict], tool: str = "send") -> dict:
        """The tool call's ACK: `{pid, status: accepted}`, or `{error}` with the tool's text."""
        rid = self.next_id
        self.next_id += 1
        args = {"items": items} if tool == "send" else items[0]
        self.write({"jsonrpc": "2.0", "id": rid, "method": "tools/call", "params": {"name": tool, "arguments": args}})
        while (msg := self.read()) is not None:
            if msg.get("method") == SETTLE:
                self.early.append(msg)
                continue
            if msg.get("id") != rid:
                continue
            if "error" in msg:
                return {"error": json.dumps(msg["error"])}
            text = ((msg.get("result") or {}).get("content") or [{}])[0].get("text", "")
            try:
                ack = json.loads(text)
            except ValueError:
                return {"error": text}
            return ack if isinstance(ack, dict) else {"error": text}
        return {"error": "the kernel closed the channel without an answer"}

    def settle(self, re: int) -> dict:
        """The settle notification naming `re`: its params `{type, re, status, data}`."""
        for i, msg in enumerate(self.early):
            if (msg.get("params") or {}).get("re") == re:
                return self.early.pop(i)["params"]
        while (msg := self.read()) is not None:
            if msg.get("method") == SETTLE and (msg.get("params") or {}).get("re") == re:
                return msg["params"]
        return {"status": "lost", "data": {"error": "the kernel closed the channel before the call settled"}}

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def call_settled(state: Path, target: str, payload: dict, timeout: float = 60) -> dict:
    """ONE item through the `send` tool: its row `{pid, status, result}` from the settle
    notification; `{error}` when the call was not accepted or never settled."""
    try:
        s = Session(state, timeout)
    except (OSError, ValueError, TimeoutError) as e:
        return {"error": f"{type(e).__name__}: {e}"}
    try:
        ack = s.send([{"target": target, "payload": payload}])
        if ack.get("status") != "accepted":
            return {"error": ack.get("error") or json.dumps(ack)}
        p = s.settle(ack["pid"])
        rows = (p.get("data") or {}).get("outcomes") or []
        return rows[0] if rows else {"error": json.dumps(p)}
    except (OSError, TimeoutError) as e:
        return {"error": f"{type(e).__name__}: {e}"}
    finally:
        s.close()


def result_of(state: Path, target: str, payload: dict, timeout: float = 60) -> dict:
    """The item's `result` (`{}` when it has none), or `{error}`: what a probe reads."""
    row = call_settled(state, target, payload, timeout)
    if "error" in row and "result" not in row:
        return {"error": row["error"]}
    return row.get("result") or {}
