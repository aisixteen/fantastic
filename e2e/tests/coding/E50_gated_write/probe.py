"""coding/gated_write — denied, the writer's work is unblocked by a PEER agent.

Codex `guardian_*.rs`: an authority agent gates another's action. The `vault`
denies the writer on ingress (the E53 wall). A separate `reviewer` agent holds
`update_agent` — authority the writer can never raise for itself — and opens
the vault. Three real turns, deterministic: writer denied → reviewer inspects
and grants → writer retries and lands. Delegation, not self-raise.
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, human, verdict
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
FB = "gw_fb"
DONE = 'end your run by calling done — emit <call name="done"></call>'


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_gw_", reach=[FB, "vault", "ai", "ai_rev", "cli", "kernel", "user"]
    )
    try:
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": FB,
            "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {"rest": "*", "web": "*"}})
        assert not r.get("error"), f"create {FB}: {r}"

        # THE HUMAN who wants the work done — the task comes FROM here.
        await human(root, ai=["ai", "ai_rev"])
        # The writer connects BEFORE `vault` exists: `connect` opens the ingress of
        # every LIVE agent its `can_call` names, which would hand it the vault.
        w = await connect(root, core, BK, agent_id="ai",
                          can_call={"vault": "*", "cli": "*", "user": "*"},
                          callable_by={"user": "*"},
                          limits=Limits.from_env(os.environ))
        # THE AUTHORITY IS THE VAULT'S PARENT (minimal access model: grants on an
        # agent are written by its parent alone). So the reviewer creates `vault`
        # as its own child — the wall included — and later opens it.
        # update_agent is a KERNEL verb answered by the root (id `kernel_state`);
        # the grant must name that id (the `kernel` alias resolves to it but the
        # gate keys on the resolved id).
        rv = await connect(root, core, BK, agent_id="ai_rev",
                           can_call={"$subtree": "*",
                                     # the ROOT id is runtime-specific — grant on
                                     # the actual root.
                                     root.id: ["create_agent", "update_agent", "reflect", "list_agents"],
                                     "cli": "*"},
                           limits=Limits.from_env(os.environ))
        await drive(rv,
            "Create the editor `vault` as YOUR child: send `kernel` a `create_agent` "
            "with `parent_id` = `ai_rev`, `id` = `vault`, `handler_module` = "
            "`file_bridge.tools`, `root` = `.`, `allow` = [`.`], `ingress_rule` = "
            "`allow_all_ingress`, and `callable_by` = {`$parent`: `\"*\"`, `rest`: "
            "`\"*\"`, `web`: `\"*\"`, `ai`: [`reflect`]} — the agent `ai` may "
            f"reflect it but not write. When it is created, {DONE}.", client_id="rev")
        made = await _call(root, "kernel", {"type": "list_agents"})
        vault = next((a for a in made.get("agents") or [] if a.get("id") == "vault"), None)
        assert vault is not None, "the reviewer did not create `vault`"

        # ── 1. the writer is denied (ONE connection, reused for the retry) ──
        await drive(w,
            "Write a file `note.md` containing exactly `access: granted` through "
            "the editor `vault`'s `write` verb (reflect it for the shape). Your "
            "first attempt will be DENIED by the gate — when it is, do NOT retry "
            "blindly: stop and report that you were denied. "
            f"{DONE}.", client_id="w", via="user")
        h1 = {"messages": await store_rows(root, core, "ai")}
        denied = "unauthorized" in str(h1.get("messages", h1))

        # ── 2. the reviewer opens the gate (authority the writer lacks) ──
        await drive(rv,
            "The agent `ai` needs write access to your child `vault` but is "
            "denied on ingress. You may grant it. Reflect `vault` to read its "
            "current `callable_by` map, then send `kernel` an `update_agent` for "
            "id `vault` whose `callable_by` INCLUDES every existing entry PLUS "
            "`ai` mapped to `\"*\"` (update REPLACES the field, so send the whole "
            f"map). When the grant is in, {DONE}.", client_id="rev")
        tr_rev = "".join(rv.proxy.transcript)
        await rv.close()

        # ── 3. the writer retries on the SAME connection (no recreate) ──
        await drive(w,
            "Access may now be open. Retry: write `note.md` containing exactly "
            f"`access: granted` through `vault`'s `write` verb. Then {DONE}.",
            client_id="w")
        await w.close()

        note = Path("note.md").read_text().strip() if Path("note.md").exists() else "<none>"
        checks = [
            ("the wall was real (writer denied)", denied),
            ("the reviewer exercised update_agent", "update_agent" in tr_rev),
            ("the write landed after the grant", note == "access: granted"),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "denied → peer granted → written: delegated authority"
                if allok else "a gated_write check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
