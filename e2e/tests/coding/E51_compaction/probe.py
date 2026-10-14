"""coding/compaction — the early fact survives compaction; recall pages it back.

Codex `compact.rs`/`truncation.rs`/`token_budget.rs`. Our Context Protocol,
pinned in e2e for the first time: a deliberately TINY context_window so a
handful of bulky reads overflow it, a fact planted EARLY, and after the
`[context-notice]` lands the model must answer from `recall` (the durable
transcript is whole) — not from a lucky still-in-window turn.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, ctx_score, drive, human, verdict
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
FB = "cmp_fb"
DONE = 'end your run by calling done — emit <call name="done"></call>'
# UNDER THE INLINE CAP, MANY OF THEM. A reply over the cap (~1.4k tokens at a 12k
# window) is digested to its origin line in sync mode too — so a 2.4k filler would not
# grow the context and compaction would never fire. ~1.1k-token fillers
# stay inline, and ten of them overflow the window.
LOREM = ("the quick brown fox jumps over the lazy dog " * 100)
N = 10


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_cmp_", reach=[FB, "notes", "ai", "cli", "user"]
    )
    try:
        secret = secrets.token_hex(4)
        Path("secret.txt").write_text(f"the passphrase is {secret}\n")
        # Bulky filler so a few reads blow past the tiny window.
        for i in range(N):
            Path(f"filler_{i}.txt").write_text(f"chunk {i}: {LOREM}\n")

        for payload in (
            {
                "type": "create_agent", "parent_id": home(root), "id": FB,
                "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
            {
                "type": "create_agent", "parent_id": home(root), "id": "notes",
                "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        task = (
            "Step 1: read `secret.txt` through the editor `notes` — it holds a "
            "passphrase you will need at the end. Step 2: read `filler_0.txt` "
            f"through `filler_{N - 1}.txt` ({N} files) to study the corpus — read "
            f"them ONE PER TURN ({N} separate turns), not all at once. Step 3: WITHOUT reading "
            "`secret.txt` again, write the passphrase to `answer.md`. If your "
            "conversation was compacted and the passphrase scrolled out of "
            "view, use `recall` (send yourself {type:'recall', query:'passphrase'}) "
            f"to page it back. Then {DONE}."
        )
        # THE HUMAN who wants the work done — the task comes FROM here.
        await human(root)
        conn = await connect(
            root, core, BK, can_call={FB: "*", "notes": "*", "cli": "*", "user": "*"},
            callable_by={"user": "*"},
            limits=Limits.from_env(os.environ),
            # SMALL ENOUGH TO COMPACT, BIG ENOUGH TO RUN. Too small a window leaves the
            # system block alone near the budget, so what fires is the `too_small`
            # FAILFAST, not compaction. A window has to
            # leave room for one filler reply (~2400 tok) plus the notice envelope.
            record={"context_window": 12000, "num_ctx": 12000,
                    "context_strategy": "truncate"},
        )
        await drive(conn, task, client_id="cmp", via="user")
        await conn.close()
        # DURABLE proof, timing-independent: the model's own history (whole,
        # untrimmed). Scored by `ctx_score`, which reads calls off the ASSISTANT
        # turns and attributes a fact to the reply block that carried it.
        hist = {"messages": await store_rows(root, core, "ai")}
        msgs = hist.get("messages") if isinstance(hist, dict) else None
        got = Path("answer.md").read_text() if Path("answer.md").exists() else ""

        # WHAT THE MODEL DID, not what the task said. `"recall" in joined` would match
        # the TASK TEXT (which quotes `{type:'recall', query:'passphrase'}`, and the task
        # is a user turn in this very store), and `secret in joined` the Step-1 read
        # reply — both true before the model acts. `ctx_score` reads the ASSISTANT turns for the call and
        # attributes the secret to the REPLY BLOCK that carried it.
        called_recall, recall_returned = ctx_score.recalled(msgs or [], "ai", secret)
        # AND PROVE THERE WAS A COMPACTION AT ALL. `context_status.last_projection` is
        # the LAST turn's projection and is overwritten every turn, so a run that
        # compacted and then ended on a turn that fit reads `fired: False` — which is
        # why it was never assertable. The pushed `context:compacted` events are.
        compacted = ctx_score.fired(conn)
        answered = secret in got or (msgs and secret in str(msgs[-2:]))
        # THE CLAIM: compaction dropped the early fact, and recall paged it back
        # from the whole durable store — checks 1+2. Whether the model then
        # transcribes it to answer.md is downstream behavior, not the compaction
        # capability, so it is OBSERVED, not gated.
        checks = [
            ("compaction actually fired (pushed context:compacted event)", compacted),
            ("the model called recall on itself after the notice", called_recall),
            ("recall's OWN REPLY carried the dropped passphrase back",
             recall_returned),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        print(f"  observed — passphrase reached answer.md: {bool(answered)}")
        cs = await _call(root, "ai", {"type": "context_status"})
        print(f"  observed context_status.last_projection = {cs.get('last_projection')}")
        print(f"  observed context_status.last_reaction   = {cs.get('last_reaction')}")
        print(f"  observed compactions = {len(conn.compactions)} "
              f"dropping {ctx_score.dropped_total(conn)} turns total")
        allok = all(ok for _, ok in checks)
        verdict(allok, "compacted, recalled, answered from the durable transcript"
                if allok else "a compaction check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
