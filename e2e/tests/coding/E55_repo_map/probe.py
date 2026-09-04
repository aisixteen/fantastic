"""coding/repo_map — the model MAPS a real codebase it cannot read whole (C14).

The emergent-coder bet, measured: no repo-map ORGAN exists — the map is an
ARTIFACT the model builds from the organs it has (glob/grep through the
searcher, paged reads, the text_file editor) and keeps as a FILE, because
memory is files. The subject is a REAL repo (aider's `coders` package, copied
from the local research clone into the run's workdir, removed after). Weights
are locked out two ways: the map must be COMPLETE for a directory whose truth
the probe computes live, and one module is SYNTHETIC, minted this run — a map
missing it was recited, not built.
"""

from __future__ import annotations

import asyncio
import os
import re
import secrets
import shutil
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
from helpers import Limits, connect, drive, e2e_root, human, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
XID = "searcher"
FB = "rm_fb"

#: the local research clone the subject is copied from — a REAL codebase.
#: Fail loud if absent; nothing is fetched from the network.
AIDER_PKG = e2e_root().parent / "tmp" / "repos" / "coding" / "aider" / "aider"

TASK = (
    "An unfamiliar repository sits at `repo/` — far too big to read whole. "
    "Map ONE package of it: for EVERY `.py` file directly inside "
    "`repo/aider/coders/` that defines a class, write one line "
    "`<filename>: <first class name defined in it>` (the class that appears "
    "first in the file; skip files defining no class). Build the map with the "
    "searcher `searcher` (`grep`/`find` — reflect it; start a job with "
    "{type:'start', command, args}, poll {type:'status', job_id}) and write "
    "the finished map as `map.md` through the editor `notes`, one line per "
    "file, nothing else. Then end your run by calling done — emit <tool_call>{\"name\":\"done\"}</tool_call>."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def _plant() -> tuple[dict[str, str], str]:
    """Copy the REAL package into `repo/` + mint one synthetic module, then
    compute the ground truth the probe scores against (first class per file)."""
    assert AIDER_PKG.is_dir(), f"research clone missing: {AIDER_PKG}"
    shutil.copytree(AIDER_PKG, Path("repo") / "aider")
    token = secrets.token_hex(3)
    synth = Path("repo") / "aider" / "coders" / f"zz_{token}_coder.py"
    synth.write_text(
        f"class Zz{token.capitalize()}Coder:\n    edit_format = \"zz-{token}\"\n"
    )
    truth: dict[str, str] = {}
    for f in sorted((Path("repo") / "aider" / "coders").glob("*.py")):
        m = re.search(r"^class\s+([A-Za-z_][A-Za-z0-9_]*)", f.read_text(), re.MULTILINE)
        if m:
            truth[f.name] = m.group(1)
    return truth, synth.name


async def main():
    root, core, close = await open_kernel(
        prefix="coding_rm_", reach=[XID, FB, "notes", "ai", "cli", "user"]
    )
    try:
        truth, synth_name = _plant()
        for payload in (
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": FB,
                "handler_module": "file_bridge.tools",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*", "notes": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "notes",
                "handler_module": "text_file.tools",
                "target_id": FB,
                "can_call": {FB: "*"},
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": XID,
                "handler_module": "exec_bridge.tools",
                "cwd": os.getcwd(),
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
        await drive(conn, TASK, client_id="rm", via="user")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        got: dict[str, str] = {}
        map_file = Path("map.md")
        if map_file.exists():
            for line in map_file.read_text().splitlines():
                if ":" in line:
                    k, v = line.split(":", 1)
                    got[k.strip().strip("`")] = v.strip().strip("`")
        missing = {k: v for k, v in truth.items() if got.get(k) != v}
        extras = sorted(set(got) - set(truth))
        checks = [
            (
                f"the map is COMPLETE and RIGHT ({len(truth)} class-bearing files)",
                not missing,
            ),
            ("no invented entries", not extras),
            (
                "the minted module is mapped — built live, not recited",
                got.get(synth_name) == truth[synth_name],
            ),
            ("the searcher organ ran", XID in transcript and "grep" in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        if missing:
            print(f"  missing/wrong: {dict(list(missing.items())[:6])}")
        if extras:
            print(f"  extras: {extras[:6]}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "a real package mapped by organs alone — the map is a file it made"
            if allok
            else "a repo_map check failed",
        )
    finally:
        # CLEANUP STAGE — the copied repo is a run artifact, never left behind.
        shutil.rmtree("repo", ignore_errors=True)
        await close()


if __name__ == "__main__":
    asyncio.run(main())
