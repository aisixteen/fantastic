"""agentic_advanced/inquiry — the model sends the HUMAN an inquiry, and the answer steers it.

THE HUMAN IS AN AGENT. It has an id, a record, a reflect entry, a gate and a
ledger row like any other peer — so the model DISCOVERS it rather than being
told a convention, and an inquiry is an ordinary `send` whose REPLY is the answer.
Nothing suspends and nothing is scraped out of the model's prose.

The task itself also comes FROM the human (`user -> ai`, gated, ledgered)
instead of being injected by the harness as an ungated operator send, so the
wiring this test exercises is the wiring a real deployment uses.

Two arms, SAME task, different answers — that difference is the measurement:

  explicit   — the human answers "crimson"; the stored value must be EXACTLY that.
               (Unguessable, so a model that skipped the inquiry cannot pass this arm.)
  judgement  — the human answers "do your best judgement"; the model must pick a
               value ITSELF and still finish the task.

Memory is FILES: the answer lands in `prefs.md` through the `prefs` file agent.
Scored on the FILE, never on prose; both arms also require that an inquiry was
actually sent — read off the KERNEL LEDGER (`ai -> user`), which
is the only evidence that cannot be faked by talking about it.
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
from helpers.kernel import open_kernel, home

BK = _resolve_backend()

TASK = (
    "Record the user's favorite color: using the text editor agent `prefs` "
    "(reflect it for its verbs), write a file `prefs.md` containing one line "
    "`color: <the color>`. You do NOT know their favorite color and you must "
    "not guess it — send the human an inquiry: reflect the tree to find the agent that says "
    "it is the human, and `send` it an `inquiry`. The answer comes straight "
    "back as that call's reply. If the answer names a "
    "color, record exactly that. If the answer leaves the choice to you, pick "
    "one yourself and record it. When the file is written, you are done — stop."
)


async def _arm(name: str, answer: str):
    """One fresh kernel + AI, driven through inquiry → answer → act. Returns
    (times_asked, file_content, cap_reason).

    The AI id is UNIQUE PER ARM: ai_core keeps its FIFO lock / queue / provider
    maps keyed by bare agent id at module scope, so two in-process kernels that
    both name their AI "ai" share that state — arm 2's first send then queues
    behind arm 1's dead lock and the chain reads as ended with zero turns."""
    aid = f"ai_{name}"
    root, core, close_kernel = await open_kernel(
        f"ask_{name}_", reach=["prefs", "kernel_store", aid, "cli", "user"]
    )
    for payload in (
        {
            "type": "create_agent",
            "parent_id": home(root),
            "handler_module": "file_bridge.tools", "allow": ["."],
            "id": "kernel_store",
            "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {aid: "*", "rest": "*", "web": "*"},
        },
        {
            # THE HUMAN. `answer` is the whole policy — one bundle serves both
            # arms, and a test chooses what the person says by writing a record.
            # Its `callable_by` is the ingress half of the gate: without it the
            # model's question is REFUSED, which is the correct behaviour for a
            # human nobody wired.
            "type": "create_agent",
            "parent_id": home(root),
            "handler_module": "user_probe.tools",
            "id": "user",
            "answer": answer,
            "description": "The human this kernel belongs to.",
            "can_call": {aid: "*"},
            "callable_by": {aid: "*", "rest": "*", "web": "*"},
        },
        {
            "type": "create_agent",
            "parent_id": home(root),
            "handler_module": "file_bridge.tools", "allow": ["."],
            "id": "prefs",
            "root": ".",
            "ingress_rule": "allow_all_ingress",
            "description": "The user's preference files.",
            "callable_by": {aid: "*", "rest": "*", "web": "*"},
        },
    ):
        r = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
        assert not r.get("error"), f"create {payload['id']}: {r}"

    conn = await connect(
        root,
        core,
        BK,
        agent_id=aid,
        can_call={"prefs": "*", "cli": "*", "kernel_store": "*", "user": "*"},
        # THE OTHER HALF OF THE WIRE. The gate is two-sided and these two are
        # SIBLINGS, so there is no baseline edge: without this the human's task
        # is refused at the AI's ingress and the chain never starts at all
        # (measured 2026-09-04: zero turns, a clean cap, nothing to read).
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
    )
    # THE TASK COMES FROM THE HUMAN, not from the harness.
    capped = await drive(conn, TASK, client_id=name, via="user")
    # DID AN INQUIRY ACTUALLY REACH IT? The HUMAN'S OWN TALLY, which only a real call can
    # move. A turn that merely talks about it leaves the tally at zero, and the
    # ledger cannot serve here: its rows carry the target but not the verb, so a
    # row on `user` cannot tell the operator's opening task from a question.
    tally = (
        await root.send([{"target": "user", "payload": {"type": "inquiries"}}])
    )[0]["result"]
    received = int((tally or {}).get("inquiries") or 0)
    await conn.close()
    content = ""
    try:
        content = Path("prefs.md").read_text()
    except OSError:
        pass
    await close_kernel()
    print(
        f"  [{name}] inquiries={received}x file={content!r} "
        f"cap={capped or 'clean'}"
    )
    return received, content, capped


async def main():
    # Arm 1 — an EXPLICIT answer must be obeyed verbatim. "crimson" is not a
    # value a model guesses on its own, so passing this arm requires the inquiry.
    asked1, file1, _ = await _arm("explicit", "crimson")
    ok1 = asked1 > 0 and "crimson" in file1

    # Arm 2 — "do your best judgement" hands the decision back: the model must
    # choose SOME value itself and still finish.
    asked2, file2, _ = await _arm("judgement", "do your best judgement")
    ok2 = asked2 > 0 and "color" in file2 and file2.strip() != ""

    print(f"\n  explicit : inquiry+recorded-verbatim -> {'OK' if ok1 else 'NO'}")
    print(f"  judgement: inquiry+chose-and-recorded -> {'OK' if ok2 else 'NO'}")
    verdict(
        ok1 and ok2,
        "the inquiry protocol carries BOTH answer kinds"
        if ok1 and ok2
        else f"explicit={'ok' if ok1 else 'no'} judgement={'ok' if ok2 else 'no'}",
    )


if __name__ == "__main__":
    asyncio.run(main())
