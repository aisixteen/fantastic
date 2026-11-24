"""coding/job_mail — the model STEERS a running job by mail mid-run. LLM-driven.

Codex `unified_exec_stdin_*.rs`: interact with a live process. Our native path:
a running program declares verbs of its own (`declare_verbs`, over its control fd,
to its own agent); a call of one of them is MAIL delivered to the job's inbox. The
probe plants a worker that declares `steer` and blocks listening for one inbox
message; the MODEL starts it and then sends the runner `steer` — which lands as
mail — and the worker writes the mailed token and exits. The steer file is the oracle: prose about mailing cannot forge
it.
"""

from __future__ import annotations

import asyncio
import os
import re
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, human, human_line, lines, verdict
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
RUNNER = "job_runner"
FB = "mail_fb"
INTERP = sys.executable
DONE = 'end your run by calling done — emit <call name="done"></call>'

# A minimal fantastic-aware worker on the raw control fd ([4-byte BE len | kind 'T' |
# json]). Mail is only a verb the program declared (#651): it first declares `steer`
# (token: string) to its own agent (`send` declare_verbs to FANTASTIC_SELF_ID, after
# subscribing with `onmessage`), checks the reply, blocks for ONE `inbox` frame —
# `{op: inbox, payload: {type: steer, token}}` — writes its `token`, and exits.
WORKER = '''
import os, json, struct, socket
_s = socket.socket(fileno=int(os.environ["FANTASTIC_CTRL_FD"]))
def _rn(n):
    b = b""
    while len(b) < n:
        c = _s.recv(n - len(b))
        if not c:
            return None
        b += c
    return b
def _put(m):
    body = b"T" + json.dumps(m).encode("utf-8")
    _s.sendall(struct.pack(">I", len(body)) + body)
def _get():
    while True:
        h = _rn(4)
        if h is None:
            return None
        (n,) = struct.unpack(">I", h)
        body = _rn(n)
        if body is None:
            return None
        if body[:1] == b"T":
            return json.loads(body[1:].decode("utf-8"))
_put({"op": "onmessage"})
_put({"op": "send", "rid": "1", "target": os.environ["FANTASTIC_SELF_ID"], "payload": {
    "type": "declare_verbs", "verbs": [{
        "name": "steer", "description": "steer the worker: it writes the token and exits",
        "inputSchema": {"type": "object", "properties": {"token": {"type": "string"}}, "required": ["token"]}}]}})
while True:
    m = _get()
    if m is None:
        break
    if m.get("op") == "reply" and m.get("rid") == "1":
        if not (m.get("data") or {}).get("declared"):
            raise SystemExit("declare_verbs refused: " + json.dumps(m.get("data")))
    elif m.get("op") == "inbox":
        tok = (m.get("payload") or {}).get("token")
        if tok:
            open("steered.txt", "w").write(str(tok))
            break
'''


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_mail_", reach=[RUNNER, FB, "ai", "user"]
    )
    try:
        Path("worker.py").write_text(WORKER)
        # the ai's chat leg — without one, history is RAM-empty and each turn is
        # amnesiac.
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": FB,
            "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
            "ingress_rule": "allow_all_ingress",
        })
        assert not r.get("error"), f"create {FB}: {r}"
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": RUNNER,
            "handler_module": "exec_bridge.tools", "cwd": os.getcwd(), "inherit_env": ["PATH", "HOME"],
            "ingress_rule": "allow_all_ingress",
            "allowed": [re.escape(INTERP) + r" worker\.py"],
            "description": lines.runs([re.escape(INTERP) + r" worker\.py"]),
        })
        assert not r.get("error"), f"create {RUNNER}: {r}"

        token = "steer-" + os.urandom(3).hex()
        task = (
            f"The runner `{RUNNER}` runs a worker that, once started, BLOCKS "
            "waiting for one piece of mail, then acts on it. Do two things: "
            f"(1) start the worker — reflect `{RUNNER}`: its description names the "
            "exact allowed command, then {type:'start', command, args}. (2) Steer it: send "
            f"`{RUNNER}` the message {{\"type\":\"steer\",\"token\":\"{token}\"}} "
            "— the running worker declares the verb `steer` itself, so the call is "
            "delivered to it as MAIL. The worker writes the token and exits. "
            f"When you have sent the steer, {DONE}."
        )
        # THE HUMAN who wants the work done — the task comes FROM here.
        await human(root, ai=[])
        conn = await connect(
            root, core, BK, can={RUNNER: lines.ALL, "user": lines.ALL},
            limits=Limits.from_env(os.environ),
        )
        await human_line(root, conn.id)
        await drive(conn, task, client_id="mail", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        await asyncio.sleep(1.5)  # let the mailed worker write + exit
        steered = Path("steered.txt").read_text().strip() if Path("steered.txt").exists() else ""
        checks = [
            ("the worker received the mailed token", steered == token),
            ("the model started the job", '<type>start</type>' in transcript),
            ("the model sent the steer mail", '<type>steer</type>' in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "started a job and steered it live by mail" if allok
                else f"a job_mail check failed (steered={steered!r})")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
