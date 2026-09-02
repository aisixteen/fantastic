"""coding/job_mail — the model STEERS a running job by mail mid-run. LLM-driven.

Codex `unified_exec_stdin_*.rs`: interact with a live process. Our native path:
any verb the exec_bridge does not own is MAIL delivered to the running job's
inbox over its control fd. The probe plants a worker that blocks listening for
one inbox message; the MODEL starts it and then sends the runner a message the
bundle does not own — which lands as mail — and the worker writes the mailed
token and exits. The steer file is the oracle: prose about mailing cannot forge
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
from helpers import Limits, connect, drive, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
RUNNER = "job_runner"
FB = "mail_fb"
INTERP = sys.executable
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'

# A minimal fantastic-aware worker: raw control-fd reader, blocks for ONE inbox
# frame ([4-byte BE len | kind 'T' | json], op=='inbox'), writes its `token`, exits.
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
_sub = b'T{"op":"onmessage"}'
_s.sendall(struct.pack(">I", len(_sub)) + _sub)
while True:
    h = _rn(4)
    if h is None:
        break
    (n,) = struct.unpack(">I", h)
    body = _rn(n)
    if body is None:
        break
    if body[:1] != b"T":
        continue
    m = json.loads(body[1:].decode("utf-8"))
    if m.get("op") == "inbox":
        tok = (m.get("payload") or {}).get("token")
        if tok:
            open("steered.txt", "w").write(str(tok))
            break
'''


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_mail_", reach=[RUNNER, FB, "ai", "cli"]
    )
    try:
        Path("worker.py").write_text(WORKER)
        # the ai's chat leg — without one, history is RAM-empty and each turn is
        # amnesiac (measured on E46/E53).
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": root.id, "id": FB,
            "handler_module": "file_bridge.tools", "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {"ai": "*", "rest": "*", "web": "*"},
        })
        assert not r.get("error"), f"create {FB}: {r}"
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": root.id, "id": RUNNER,
            "handler_module": "exec_bridge.tools", "cwd": os.getcwd(),
            "ingress_rule": "allow_all_ingress",
            "allowed": [re.escape(INTERP) + r" worker\.py"],
            "callable_by": {"ai": "*", "rest": "*", "web": "*"},
        })
        assert not r.get("error"), f"create {RUNNER}: {r}"

        token = "steer-" + os.urandom(3).hex()
        task = (
            f"The runner `{RUNNER}` runs a worker that, once started, BLOCKS "
            "waiting for one piece of mail, then acts on it. Do two things: "
            f"(1) start the worker — reflect `{RUNNER}` for the exact allowed "
            "command, then {type:'start', command, args}. (2) Steer it: send "
            f"`{RUNNER}` the message {{\"type\":\"steer\",\"token\":\"{token}\"}} "
            "— the runner does not own the verb `steer`, so it is delivered to "
            "the running worker as MAIL. The worker writes the token and exits. "
            f"When you have sent the steer, {DONE}."
        )
        conn = await connect(
            root, core, BK, can_call={RUNNER: "*", "cli": "*"},
            limits=Limits.from_env(os.environ), record={"file_bridge_id": FB},
        )
        await drive(conn, task, client_id="mail")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        await asyncio.sleep(1.5)  # let the mailed worker write + exit
        steered = Path("steered.txt").read_text().strip() if Path("steered.txt").exists() else ""
        checks = [
            ("the worker received the mailed token", steered == token),
            ("the model started the job", '"start"' in transcript),
            ("the model sent the steer mail", '"steer"' in transcript),
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
