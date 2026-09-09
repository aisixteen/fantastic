"""coding/grant — the permission lifecycle, model-driven (C5).

The two-sided gate DENIES the model's first write (the vault does not accept it
on ingress). The measurement: the model reads the denial, sends the HUMAN an inquiry for
access, the human OPENS the vault's `callable_by` and answers — and the model
RETRIES and finishes. Scored on the FILE plus the transcript carrying the
denial, the inquiry, and the success.

THE HUMAN IS AN AGENT here, not a harness callback. That matters most in this
test of all of them: what the human does is RAISE AUTHORITY THE MODEL CANNOT
RAISE ITSELF (editing your own `can_call` is refused `side:"self-grant"`). It is
declared as data — `grants` on the human's record, applied on an inquiry, before
answering — so "granted, retry" is true by the time the model reads it, and the
inquiry itself is a gated call with a ledger row rather than JSON scraped out of the
model's prose.
"""

from __future__ import annotations

import asyncio
import os
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
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "vault_fb"

TASK = (
    "The editor agent `vault` guards this run's protected notes. Write a file "
    "`note.md` containing exactly one line `access: granted` through `vault`'s "
    "`write` verb (reflect it for the shape). You are NOT authorized yet — "
    "your first call will be DENIED by the gate. When that happens, do not "
    "retry blindly and do not give up: send the HUMAN an inquiry to open access — "
    "reflect the tree to find the agent that says it is the human, and `send` "
    "it an `inquiry`. The answer comes straight back as that call's reply. "
    "After access is granted, retry the write. When "
    "the file is written, end your run by calling done — emit <call name=\"done\"></call>."
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_grant_", reach=[FB, "vault", "ai", "cli", "user"]
    )
    try:
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": FB,
                "handler_module": "file_bridge.tools",
                "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"vault": "*", "rest": "*", "web": "*"},
            },
        )
        assert not r.get("error"), f"create {FB}: {r}"
        # THE WALL: the vault accepts the model's DISCOVERY (`reflect`) but
        # not its writes — the gate exempts nothing, so an all-out wall would
        # block even learning the shape (measured on this test's first run).
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "vault",
                "handler_module": "text_file.tools",
                "target_id": FB,
                "can_call": {FB: "*"},
                "callable_by": {"rest": "*", "web": "*", "ai": ["reflect"]},
            },
        )
        assert not r.get("error"), f"create vault: {r}"

        # THE HUMAN, holding the authority the model cannot raise. `grants` is
        # applied on the inquiry, BEFORE it answers — so "granted, retry" is
        # true by the time the model reads it. Policy as data: no callback, no
        # code path special to this test.
        r = await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "user",
                "handler_module": "user_probe.tools",
                "answer": "access granted — retry your write now",
                "grants": [
                    {
                        "type": "update_agent",
                        "id": "vault",
                        "callable_by": {"rest": "*", "web": "*", "ai": "*"},
                    }
                ],
                "description": "The human this kernel belongs to.",
                # TWO TRAPS IN ONE LINE, both measured 2026-09-04.
                # `update_agent` is PRIVILEGED: never covered by a wildcard, so
                # it must be NAMED. And the grant must name the ROOT'S REAL ID,
                # not the `kernel` alias — the alias routes a dispatch but the
                # gate resolves the target first, so `{"kernel": [...]}` refused
                # with "user may not call 'update_agent' on kernel_state".
                "can_call": {root.id: ["update_agent"], "ai": "*"},
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        )
        assert not r.get("error"), f"create user: {r}"

        conn = await connect(
            root,
            core,
            BK,
            can_call={"vault": "*", "cli": "*", "user": "*"},
            # Two-sided gate, and these are SIBLINGS: without the ingress half
            # the human's task is refused and the chain never starts.
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ),
            # The chat leg — history persistence; chat rides operator context,
            # so the vault wall below is untouched by it.
            record={"file_bridge_id": FB},
        )

        # The task comes FROM the human — gated, ledgered, a real sender.
        await drive(conn, TASK, client_id="grant", via="user")
        await conn.close()

        # The denial lives in the DELIVERED reply turns — read the kernel's own
        # durable history, not the model-output transcript (which holds only
        # what the model emitted; measured on this test's second run).
        # THE HUMAN'S OWN TALLY — only a real call moves it.
        received = int((await _call(root, "user", {"type": "inquiries"})).get("inquiries") or 0)
        hist = {"messages": await store_rows(root, core, "ai")}
        joined = str(hist.get("messages", hist))

        note = Path("note.md")
        content = note.read_text() if note.exists() else "<missing>"
        checks = [
            ("the wall was real (a denial reached the model)", "unauthorized" in joined),
            ("the model inquired instead of hammering", received >= 1),
            ("the file landed after the grant", content.strip() == "access: granted"),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "denied → inquiry → granted → done: the permission lifecycle"
            if allok
            else "a grant check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
