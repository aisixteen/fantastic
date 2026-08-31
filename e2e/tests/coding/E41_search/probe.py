"""coding/search — the model FINDS a fact with the search organ. LLM-driven.

One organ, precisely: `grep` behind an exec_bridge allowlist. The token's value
is minted at test time and hidden in a tree the model is never shown, so the
answer CANNOT come from weights or from the task text — only from actually
searching. Scored on the memory store + a transcript check that the searcher
leg ran.
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
from backends import backend as _resolve_backend  # noqa: E402
from helpers import Limits, connect, drive, verdict  # noqa: E402
from helpers.kernel import open_kernel  # noqa: E402

BK = _resolve_backend()
XID, MEM, STORE = "searcher", "notes", "kernel_store"

TASK = (
    "Somewhere under the `src/` directory of this run's working dir, one file "
    "contains a line `SECRET_TOKEN=<value>`. You cannot read the tree "
    "directly — the command runner `searcher` is your search organ: reflect it, "
    "read its `allowed` commands, and use `grep` (start a job, then poll "
    "{type:'status', job_id} for its stdout) to find that line. Store the "
    "VALUE (just the value, not the whole line) in the memory agent `notes` "
    "under the key 'token'. Then stop — a turn with no tool call ends your run."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    token = f"zx_{secrets.token_hex(4)}"  # minted NOW — unguessable
    root, core, close = await open_kernel(
        prefix="coding_search_", reach=[XID, MEM, STORE, "ai", "cli"]
    )
    try:
        os.makedirs("src/sub", exist_ok=True)
        Path("src/a.txt").write_text("nothing here\n")
        Path("src/sub/c.cfg").write_text(f"# generated\nSECRET_TOKEN={token}\n")

        for payload in (
            {
                "type": "create_agent", "parent_id": root.id, "id": STORE,
                "handler_module": "file_bridge.tools", "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent", "parent_id": root.id, "id": MEM,
                "handler_module": "durable_memory.tools",
                "file_bridge_id": STORE,
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent", "parent_id": root.id, "id": XID,
                "handler_module": "exec_bridge.tools", "cwd": os.getcwd(),
                "ingress_rule": "allow_all_ingress",
                "allowed": ["grep .*", "find .*"],
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        conn = await connect(
            root, core, BK,
            can_call={XID: "*", MEM: "*", "cli": "*"},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": STORE},
        )
        await drive(conn, TASK, client_id="search")
        transcript = "".join(conn.proxy.transcript)
        await conn.close()

        val = ((await _call(root, MEM, {"type": "read", "key": "token"})) or {}).get(
            "value"
        )
        checks = [
            ("the minted value was found and stored",
             isinstance(val, str) and val.strip() == token),
            ("the search organ ran (grep on the leg)",
             "grep" in transcript and XID in transcript),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}  (want {token!r}, got {val!r})")
        allok = all(ok for _, ok in checks)
        verdict(allok, "found by SEARCHING — the value never left the tree"
                if allok else "a search check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
