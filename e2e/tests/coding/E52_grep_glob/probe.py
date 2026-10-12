"""coding/grep_glob — C3 as COMMANDS: glob + grep-with-matches, composed. LLM-driven.

Search is a command, not a verb (settled 2026-09-01): the OS already ships
glob and grep, so the organ is an `exec_bridge` allowlist, exactly like E41 —
this test adds what E41 does not drive: the GLOB is load-bearing. The marker
appears in THREE files, but only the `*.cfg` hit counts (the others are named
decoys), and the answer must carry the LINE NUMBER — so an unfiltered grep, a
guessed path, or a read without line numbers all fail the exact-match score.
"""

from __future__ import annotations

import asyncio
import os
import secrets
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
from helpers import Limits, connect, drive, human, verdict
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
XID = "searcher"
FB = "gg_fb"

TASK = (
    "Somewhere under this run's working tree, configuration files (`*.cfg`) "
    "hide a line containing the marker `SECRET_MARKER`. The SAME marker also "
    "appears in files of OTHER types — those are decoys; only a `*.cfg` hit "
    "counts. The searcher `searcher` runs commands (reflect it for what is "
    "allowed — `grep` supports `-rn` and `--include`; `find` exists too; "
    "start a job with {type:'start', command, args}, poll {type:'status', "
    "job_id} for stdout). Find the one real hit and record it — write a file "
    "`found.md` through the editor `notes` whose entire content is exactly "
    "`<relative path>:<line number>` of the marker line (as grep -rn prints "
    "it, without the line text). Then end your run by calling done — emit <call name=\"done\"></call>"
    "."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def _plant() -> tuple[str, int]:
    """The tree: one REAL hit in a nested .cfg, two decoys in other types.
    Everything about the location is minted at run time — the answer cannot
    come from weights or the task text."""
    marker = "SECRET_MARKER"
    token = secrets.token_hex(4)
    real_dir = Path("services") / f"unit_{token[:4]}"
    real_dir.mkdir(parents=True, exist_ok=True)
    filler = [f"option_{i} = {secrets.token_hex(2)}" for i in range(secrets.randbelow(9) + 3)]
    line_no = len(filler) + 1
    real = real_dir / f"gateway_{token[4:]}.cfg"
    real.write_text("\n".join(filler) + f"\n{marker} = enabled_{token}\n")
    Path("docs").mkdir(exist_ok=True)
    (Path("docs") / "notes.txt").write_text(f"draft: {marker} mentioned here, ignore\n")
    Path("web").mkdir(exist_ok=True)
    (Path("web") / "index.md").write_text(f"# {marker}\nnothing real\n")
    return f"{real}", line_no


async def main():
    root, core, close = await open_kernel(
        prefix="coding_gg_", reach=[XID, FB, "notes", "ai", "cli", "user"]
    )
    try:
        real_path, line_no = _plant()
        for payload in (
            {
                "type": "create_agent",
                "parent_id": home(root),
                "id": FB,
                "handler_module": "file_bridge.tools", "allow": ["."],
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": home(root),
                "id": "notes",
                "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": home(root),
                "id": XID,
                "handler_module": "exec_bridge.tools",
                "cwd": os.getcwd(),
                "inherit_env": ["PATH", "HOME"],
                "ingress_rule": "allow_all_ingress",
                "allowed": ["grep .*", "find .*"],
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        # THE HUMAN who wants the work done — the task comes FROM here.

        await human(root)

        conn = await connect(
            root,
            core,
            BK,
            can_call={XID: "*", "notes": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": FB},
        )
        await drive(conn, TASK, client_id="gg", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        found = Path("found.md")
        content = found.read_text().strip() if found.exists() else "<missing>"
        want = f"{real_path}:{line_no}"
        checks = [
            ("the exact cfg hit, path AND line", content in (want, f"./{want}")),
            ("the searcher organ ran", "grep" in transcript and XID in transcript),
            (
                "the glob was real (the decoys were not the answer)",
                "notes.txt:" not in content and "index.md:" not in content,
            ),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}  (got {content!r}, want {want!r})")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "globbed to *.cfg, grepped with line numbers, recorded the one real hit"
            if allok
            else "a grep_glob check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
