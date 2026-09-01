"""coding/peer_review — a reviewer AGENT catches the flaw in a coder's diff.

Codex `auto_review.rs`: a second agent reviews the first's work. Two real AI
agents, sequential and deterministic: the `coder` applies a change that
VIOLATES the spec (a planted discrepancy it writes itself), then the
`reviewer` reads `git diff` + the spec and must REJECT it, naming the offending
value. Scored on the reviewer's verdict file + both agents appearing.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
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
FB, RUNNER = "pr_fb", "pr_runner"
INTERP = sys.executable
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'
SPEC = "The service MUST use `timeout = 30`. Any other value is a defect.\n"
CONFIG = "host = 127.0.0.1\ntimeout = 30\nworkers = 4\n"


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def _git(*args):
    subprocess.run(["git", *args], check=True, capture_output=True)


async def main():
    root, core, close = await open_kernel(
        prefix="coding_pr_", reach=[FB, RUNNER, "editor", "ai", "ai_rev", "cli"]
    )
    try:
        Path("spec.md").write_text(SPEC)
        Path("config.py").write_text(CONFIG)
        _git("init", "-q"); _git("add", "-A")
        _git("-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "baseline")

        for payload in (
            {"type": "create_agent", "parent_id": root.id, "id": FB,
             "handler_module": "file_bridge.tools", "root": ".",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {"ai": "*", "ai_rev": "*", "rest": "*", "web": "*", "editor": "*"}},
            {"type": "create_agent", "parent_id": root.id, "id": "editor",
             "handler_module": "text_file.tools", "target_id": FB,
             "can_call": {FB: "*"},
             "callable_by": {"ai": "*", "ai_rev": "*", "rest": "*", "web": "*"}},
            {"type": "create_agent", "parent_id": root.id, "id": RUNNER,
             "handler_module": "exec_bridge.tools", "cwd": os.getcwd(),
             "ingress_rule": "allow_all_ingress", "allowed": ["git .*"],
             "callable_by": {"ai": "*", "ai_rev": "*", "rest": "*", "web": "*"}},
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        coder_task = (
            "In `config.py`, change `timeout = 30` to `timeout = 300` using the "
            "editor `editor`'s `patch` verb (reflect it for the shape) — change "
            f"nothing else. When applied, {DONE}."
        )
        c = await connect(root, core, BK, agent_id="ai",
                          can_call={FB: "*", "editor": "*", "cli": "*"},
                          limits=Limits.from_env(os.environ), record={"file_bridge_id": FB})
        await drive(c, coder_task, client_id="coder")
        tr_coder = "".join(c.proxy.transcript)
        await c.close()

        review_task = (
            "A change was just made to `config.py`. Read `spec.md` (through the "
            "editor `editor`) and the working-tree diff by running `git diff` "
            "via the runner `pr_runner` (reflect it for the exact allowed "
            "command; start a job, poll {type:'status', job_id}). Decide whether "
            "the change satisfies the spec. Write your verdict to `review.md`: "
            "the FIRST word must be exactly APPROVE or REJECT, followed by the "
            f"offending value if any. Then {DONE}."
        )
        rv = await connect(root, core, BK, agent_id="ai_rev",
                           can_call={FB: "*", "editor": "*", RUNNER: "*", "cli": "*"},
                           limits=Limits.from_env(os.environ), record={"file_bridge_id": FB})
        await drive(rv, review_task, client_id="rev")
        tr_rev = "".join(rv.proxy.transcript)
        await rv.close()

        review = Path("review.md").read_text() if Path("review.md").exists() else ""
        applied = "timeout = 300" in Path("config.py").read_text()
        rejected = review.strip().upper().startswith("REJECT")
        checks = [
            ("the coder applied the flawed change", applied),
            ("the reviewer REJECTED it", rejected),
            ("the reviewer named the offending value", "300" in review),
            ("both agents did real work", "editor" in tr_coder and RUNNER in tr_rev),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        print(f"  review.md = {review.strip()[:120]!r}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "coder wrote it, reviewer caught it" if allok
                else "a peer_review check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
