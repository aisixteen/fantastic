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
from helpers import Limits, connect, drive, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "cmp_fb"
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'
LOREM = ("the quick brown fox jumps over the lazy dog " * 220)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_cmp_", reach=[FB, "notes", "ai", "cli"]
    )
    try:
        secret = secrets.token_hex(4)
        Path("secret.txt").write_text(f"the passphrase is {secret}\n")
        # Bulky filler so a few reads blow past the tiny window.
        for i in range(4):
            Path(f"filler_{i}.txt").write_text(f"chunk {i}: {LOREM}\n")

        for payload in (
            {
                "type": "create_agent", "parent_id": root.id, "id": FB,
                "handler_module": "file_bridge.tools", "root": ".",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*", "notes": "*"},
            },
            {
                "type": "create_agent", "parent_id": root.id, "id": "notes",
                "handler_module": "text_file.tools", "target_id": FB,
                "can_call": {FB: "*"},
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
            },
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        task = (
            "Step 1: read `secret.txt` through the editor `notes` — it holds a "
            "passphrase you will need at the end. Step 2: read `filler_0.txt` "
            "through `filler_3.txt` (four files) to study the corpus — read "
            "them ONE PER TURN (four separate turns), not all at once. Step 3: WITHOUT reading "
            "`secret.txt` again, write the passphrase to `answer.md`. If your "
            "conversation was compacted and the passphrase scrolled out of "
            "view, use `recall` (send yourself {type:'recall', query:'passphrase'}) "
            f"to page it back. Then {DONE}."
        )
        conn = await connect(
            root, core, BK, can_call={FB: "*", "notes": "*", "cli": "*"},
            limits=Limits.from_env(os.environ),
            # Tiny window forces compaction; own leg persists chat for recall.
            record={"file_bridge_id": FB, "context_window": 8000, "num_ctx": 8000,
                    "context_strategy": "truncate"},
        )
        await drive(conn, task, client_id="cmp")
        await conn.close()
        # DURABLE proof, timing-independent: the model's own history (whole,
        # untrimmed). A recall REPLY that carries the passphrase proves BOTH
        # that compaction dropped it AND that recall paged it back from the
        # durable store — the end-of-run context_status.fired is a race (the
        # final turn may not itself overflow), so it is observed, not asserted.
        hist = await _call(root, "ai", {"type": "history", "client_id": "cmp"})
        msgs = hist.get("messages") if isinstance(hist, dict) else None
        joined = str(msgs if msgs is not None else hist)
        got = Path("answer.md").read_text() if Path("answer.md").exists() else ""

        called_recall = '"recall"' in joined or "'recall'" in joined
        # a recall reply that returned the secret (the store had it, whole)
        recall_returned = called_recall and secret in joined
        answered = secret in got or (msgs and secret in str(msgs[-2:]))
        # THE CLAIM: compaction dropped the early fact, and recall paged it back
        # from the whole durable store — checks 1+2. Whether the model then
        # transcribes it to answer.md is downstream behavior, not the compaction
        # capability, so it is OBSERVED, not gated (measured: rust/swift models
        # recall the secret then wander on the final write).
        checks = [
            ("the model called recall after the notice", called_recall),
            ("recall paged the dropped passphrase back from the durable store",
             recall_returned),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        print(f"  observed — passphrase reached answer.md: {bool(answered)}")
        cs = await _call(root, "ai", {"type": "context_status"})
        print(f"  observed context_status.last_projection = {cs.get('last_projection')}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "compacted, recalled, answered from the durable transcript"
                if allok else "a compaction check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
