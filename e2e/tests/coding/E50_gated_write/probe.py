"""coding/gated_write — denied, the writer's work is unblocked by a PEER agent.

Codex `guardian_*.rs`: an authority agent gates another's action. The writer
holds only `reflect` on the `vault`, so its write is `not_allowed` naming `write`
(the E53 wall). A separate `reviewer` agent holds `update` on the writer's `can` and a
line on the vault — authority the writer can never raise for itself (an agent's
own `can` is never its own to write) — and hands the writer the `write` it
holds. Three real passes, deterministic: writer denied → reviewer inspects and
grants → writer retries and lands. Delegation, not self-raise.
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
from helpers import Limits, connect, drive, human, human_line, lines, probe_agent, relay, verdict
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel, home
from helpers.lines import is_not_allowed, is_unknown

BK = _resolve_backend()
FB = "gw_fb"
DONE = 'end your run by calling done — emit <call name="done"></call>'
#: the writer's line on the vault before the grant, and the one the reviewer hands it
WRITER_VAULT = lines.call("reflect")
GRANTED_VAULT = lines.call("reflect", "write")


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_gw_", reach=[FB, "vault", "ai", "ai_rev", "kernel", "user"]
    )
    try:
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": FB,
            "handler_module": "file_bridge.tools", "allow": ["."], "root": "."})
        assert not r.get("error"), f"create {FB}: {r}"
        # THE VAULT, made by the operator's leg (`rest`) under itself: no agent reaches
        # it but by a line in its own `can`.
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": "vault",
            "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
            "description": "The gated editor: write needs a line from the reviewer."})
        assert not r.get("error"), f"create vault: {r}"

        # THE HUMAN who wants the work done — the task comes FROM here. Its lines on
        # the two AIs are written once each exists.
        await human(root, ai=[])
        # THE WRITER may reflect the vault, never write it.
        w = await connect(root, core, BK, agent_id="ai",
                          can={"vault": WRITER_VAULT, "user": lines.ALL},
                          limits=Limits.from_env(os.environ))
        await human_line(root, w.id)
        # THE REVIEWER: a line on the vault (a literal-id line reaches its whole tree,
        # so it may hand out any part of it) and `update` on the writer's `can`.
        rv = await connect(root, core, BK, agent_id="ai_rev",
                           can={"vault": lines.ALL, "ai": {"update": ["can"]},
                                lines.ROOT: lines.call("reflect")},
                           limits=Limits.from_env(os.environ))
        await human_line(root, rv.id)
        writer_can = root.record("ai").get("can") or {}
        assert writer_can.get("vault") == WRITER_VAULT, f"the writer's vault line: {writer_can}"

        # ── 0. harness, before the model: no self-raise, no hand-out without a hold ──
        # A hand holding the writer's lines writes its OWN `can`: `not_allowed` naming `can`.
        await probe_agent(root, "w_hand", can={"vault": WRITER_VAULT})
        r = await relay(root, "w_hand", "kernel", {
            "type": "update_agent", "id": "w_hand", "can": {"vault": GRANTED_VAULT}})
        assert is_not_allowed(r, "can"), f"an agent's update of its own can must be refused naming can: {r}"
        # A third agent with `update` on the writer's `can` but NO line on the vault
        # cannot hand out a vault line: not_allowed on `can`, or the vault unknown.
        await probe_agent(root, "ai_third", can={"ai": {"update": ["can"]}})
        r = await relay(root, "ai_third", "kernel", {
            "type": "update_agent", "id": "ai", "can": {**writer_can, "vault": GRANTED_VAULT}})
        assert ((r.get("reason") == "not_allowed" and r.get("arg") == "can")
                or is_unknown(r, "vault")), f"a writer without a vault line handed one out: {r}"
        assert (root.record("ai").get("can") or {}) == writer_can, "the refused hand-out changed the writer's can"
        for hand in ("w_hand", "ai_third"):
            gone = await _call(root, root.id, {"type": "delete_agent", "id": hand})
            assert not gone.get("error"), f"delete {hand}: {gone}"

        # ── 1. the writer is denied (ONE connection, reused for the retry) ──
        await drive(w,
            "Write a file `note.md` containing exactly `access: granted` through "
            "the editor `vault`'s `write` verb (reflect it for the shape). Your "
            "first attempt will be DENIED by the gate — when it is, do NOT retry "
            "blindly: stop and report that you were denied. "
            f"{DONE}.", client_id="w", via="user")
        h1 = await store_rows(root, core, "ai")
        # the writer sees the vault (its `reflect` line), so the gate names the verb
        # its line lacks; matched on the rows' TEXT (a repr of the rows escapes the
        # quotes)
        seen = "\n".join(str(r.get("content") or "") for r in h1)
        denied = "not_allowed" in seen and "holds no call 'write' on 'vault'" in seen

        # ── 2. the reviewer opens the gate (authority the writer lacks) ──
        await drive(rv,
            "The agent `ai` needs write access to the editor `vault` but is "
            "denied: it holds only `reflect` there. You may grant it — a grant is a "
            "line in the CALLER's own `can` field, `{<target id>: {\"call\": [verbs]}}`. "
            "Reflect `ai` to read its current `can`, then send `kernel` an "
            "`update_agent` for id `ai` whose `can` INCLUDES every existing line, "
            "with the `vault` line set to `{\"call\": [\"reflect\", \"write\"]}` "
            "(update REPLACES the field, so send the whole map). "
            f"When the grant is in, {DONE}.", client_id="rev")
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
