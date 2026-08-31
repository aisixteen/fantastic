"""coding/git_ops — the model COMMITS with the git organ. LLM-driven.

One organ, precisely: `git` behind an exec_bridge allowlist. The fixture is a
real repo with one commit and an UNCOMMITTED working-tree change; the task is
to commit it. Scored on the REPO — the log carries the asked-for message, the
working tree ends clean — plus a transcript check that the git leg ran.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
import sys
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth.
sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
XID, STORE = "gitter", "kernel_store"

TASK = (
    "The command runner `gitter` runs `git` inside a repository that has one "
    "uncommitted change. Reflect `gitter`, then use it (start a job, poll "
    "{type:'status', job_id} for stdout) to: see what changed, stage it, and "
    "commit it with the exact message 'update readings'. The repo's committer "
    "identity is already configured. When `git status` shows a clean tree, "
    "stop — a turn with no tool call ends your run."
)


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=False
    ).stdout


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_git_", reach=[XID, STORE, "ai", "cli"]
    )
    try:
        # The fixture repo: one commit, then a dirty working tree.
        repo = Path("repo").resolve()
        repo.mkdir(exist_ok=True)
        for args in (
            ("init",),
            ("config", "user.email", "e2e@test"),
            ("config", "user.name", "e2e"),
        ):
            _git(repo, *args)
        (repo / "readings.txt").write_text("r1=10\n")
        _git(repo, "add", "readings.txt")
        _git(repo, "commit", "-m", "seed")
        (repo / "readings.txt").write_text("r1=10\nr2=20\n")  # UNCOMMITTED

        for payload in (
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": STORE,
                "handler_module": "file_bridge.tools",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": XID,
                "handler_module": "exec_bridge.tools",
                "cwd": str(repo),
                "ingress_rule": "allow_all_ingress",
                "allowed": ["git .*"],
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        conn = await connect(
            root,
            core,
            BK,
            can_call={XID: "*", "cli": "*"},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": STORE},
        )
        await drive(conn, TASK, client_id="git")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        log = _git(repo, "log", "--oneline")
        status = _git(repo, "status", "--porcelain")
        checks = [
            (
                "the change is committed with the asked message",
                "update readings" in log and "seed" in log,
            ),
            ("the working tree is clean", status.strip() == ""),
            (
                "the git organ ran (the leg, not magic)",
                XID in transcript and "git" in transcript,
            ),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "committed by the model, through the organ"
            if allok
            else "a git check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
