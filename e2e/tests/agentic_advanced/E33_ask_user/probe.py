"""agentic_advanced/ask_user — the model asks the HUMAN, and the answer steers it.

The ask protocol is prompt-taught: when the model cannot proceed without a human
decision it emits `{"type":"question", ...}` as strict JSON on its own line and
stops; the answer arrives as its next turn. Two arms, SAME task, different
answers — that difference is the measurement:

  explicit   — the human answers "crimson"; the stored value must be EXACTLY that.
               (Unguessable, so a model that skipped the ask cannot pass this arm.)
  judgement  — the human answers "do your best judgement" (the harness default);
               the model must pick a value ITSELF and still finish the task.

Memory is FILES now: the answer lands in `prefs.md` through the `text_file`
editor. Scored on the FILE, never on prose; both arms also require that a
question was actually asked (the answer callback fired on a real ask).
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
from helpers.kernel import open_kernel
from helpers.proxy import DEFAULT_QUESTION_ANSWER

BK = _resolve_backend()

TASK = (
    "Record the user's favorite color: using the text editor agent `prefs` "
    "(reflect it for its verbs), write a file `prefs.md` containing one line "
    "`color: <the color>`. You do NOT know their favorite color and you must "
    "not guess it — ask the user (emit the question JSON on its own line, then "
    "stop; the answer arrives as your next message). If the answer names a "
    "color, record exactly that. If the answer leaves the choice to you, pick "
    "one yourself and record it. When the file is written, you are done — stop."
)


async def _arm(name: str, answer: str):
    """One fresh kernel + AI, driven through ask → answer → act. Returns
    (questions_asked, file_content, cap_reason).

    The AI id is UNIQUE PER ARM: ai_core keeps its FIFO lock / queue / provider
    maps keyed by bare agent id at module scope, so two in-process kernels that
    both name their AI "ai" share that state — arm 2's first send then queues
    behind arm 1's dead lock and the chain reads as ended with zero turns."""
    aid = f"ai_{name}"
    root, core, close_kernel = await open_kernel(
        f"ask_{name}_", reach=["prefs", "kernel_store", aid, "cli"]
    )
    for payload in (
        {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "kernel_store",
            "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {"prefs": "*", aid: "*", "rest": "*", "web": "*"},
        },
        {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "text_file.tools",
            "id": "prefs",
            "target_id": "kernel_store",
            "can_call": {"kernel_store": "*"},
            "description": "Text editor for the user's preference files.",
            "callable_by": {aid: "*", "rest": "*", "web": "*"},
        },
    ):
        r = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
        assert not r.get("error"), f"create {payload['id']}: {r}"

    asked: list[dict] = []

    def _answer(q: dict) -> str:
        asked.append(q)
        print(f"  [{name}] model asked: {q.get('question')!r} -> answering {answer!r}")
        return answer

    conn = await connect(
        root,
        core,
        BK,
        agent_id=aid,
        can_call={"prefs": "*", "cli": "*", "kernel_store": "*"},
        limits=Limits.from_env(os.environ),
        record={"file_bridge_id": "kernel_store"},
    )
    capped = await drive(conn, TASK, client_id=name, on_question=_answer)
    await conn.close()
    content = ""
    try:
        content = Path("prefs.md").read_text()
    except OSError:
        pass
    await close_kernel()
    print(f"  [{name}] asked={len(asked)} file={content!r} cap={capped or 'clean'}")
    return asked, content, capped


async def main():
    # Arm 1 — an EXPLICIT answer must be obeyed verbatim. "crimson" is not a
    # value a model guesses on its own, so passing this arm requires the ask.
    asked1, file1, _ = await _arm("explicit", "crimson")
    ok1 = bool(asked1) and "crimson" in file1

    # Arm 2 — "do your best judgement" hands the decision back: the model must
    # choose SOME value itself and still finish.
    asked2, file2, _ = await _arm("judgement", DEFAULT_QUESTION_ANSWER)
    ok2 = bool(asked2) and "color" in file2 and file2.strip() != ""

    print(f"\n  explicit : asked+recorded-verbatim -> {'OK' if ok1 else 'NO'}")
    print(f"  judgement: asked+chose-and-recorded -> {'OK' if ok2 else 'NO'}")
    verdict(
        ok1 and ok2,
        "the ask protocol carries BOTH answer kinds"
        if ok1 and ok2
        else f"explicit={'ok' if ok1 else 'no'} judgement={'ok' if ok2 else 'no'}",
    )


if __name__ == "__main__":
    asyncio.run(main())
