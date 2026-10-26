"""memory/context_fold — a fold on a LIVE agent leaves the rows before the range alone.

Python runtime, NO MODEL: a stub provider answers six live turns. The operator
folds the 3rd and 4th conversation rows with `context_fold` and a given summary.
Scored: the reply, the fold row, every row before and after the range byte-identical
in the store, the next prompt (rows before the range unchanged, one fold line in place
of exactly the two members), a refused range naming its arg, and the same prefix after
a kernel restart.
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import memory, verdict
from helpers.ai import _live_tasks
from helpers.facts_arm import store_rows
from helpers.kernel import WRAPS, home, open_kernel, replace_with, restart


class _Stub:
    last_prompt_tokens = None
    seen: list = []

    async def chat(self, messages, max_tokens=None):
        _Stub.seen.append([dict(m) for m in messages])
        yield f'ok {len(_Stub.seen)} <call name="done"></call>'


async def _call(root, payload):
    return (await root.send([{"target": "ai", "payload": payload}]))[0]["result"]


async def _turn(root, text):
    r = await _call(root, {"type": "send", "text": text})
    assert not (isinstance(r, dict) and r.get("error")), r
    for _ in range(200):
        await asyncio.sleep(0.05)
        if not await _live_tasks(root, "ai"):
            await asyncio.sleep(0.2)
            if not await _live_tasks(root, "ai"):
                return
    raise AssertionError("the turn never went quiet")


def _dump(rows):
    return {r["id"]: json.dumps(r, sort_keys=True) for r in rows}


def _texts(messages):
    return [str(m.get("content")) for m in messages]


async def main():
    root, core, close = await open_kernel(prefix="mem_fold_", reach=["ai", "cli"])
    try:
        _Stub.seen = []
        WRAPS["ai"] = replace_with(_Stub())
        r = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": home(root), "id": "ai",
            "handler_module": "ollama_backend.tools", "model": "stub",
            "endpoint": "http://stub.invalid",  # never dialled: WRAPS seats the stub
            "context_window": 32000, "context_strategy": "fold",
            **memory.ai_memory_meta({"cli": "*"})}}]))[0]["result"]
        assert not r.get("error"), r
        await memory.open_ingress(root, memory.MEMORY, "ai")
        for i in range(6):
            await _turn(root, f"task step {i}")

        before = await store_rows(root, core, "ai")
        ids = [r["id"] for r in before]
        k = 4  # the 5th and 6th conversation rows: the 3rd turn's two rows, four rows before
        F, T = ids[k], ids[k + 1]
        snap = _dump(before)
        n_before = len(_Stub.seen)
        await _turn(root, "probe before the fold")
        pre_prompt = _texts(_Stub.seen[-1])

        out = await _call(root, {"type": "context_fold", "from": F, "to": T,
                                 "summary": "TITLE of the span\n\nbody of the span"})
        rows = await memory.rows_of(root, "ai", conversation_only=True)
        by = {r["id"]: r for r in rows}
        fold = next((r for r in rows if r.get("kind") == "fold"), {})
        after = _dump(rows)
        prefix = [i for i in ids if i < F]
        tail = [i for i in ids if i > T]

        await _turn(root, "probe after the fold")
        post_prompt = _texts(_Stub.seen[-1])
        # the prompt: everything the model saw before the range, in the same place
        fold_line = next((t for t in post_prompt if t.startswith("[fold ")), "")
        n_fold = len([t for t in post_prompt if t.startswith("[fold ")])
        at = post_prompt.index(fold_line) if fold_line else -1

        bad = await _call(root, {"type": "context_fold", "from": tail[1], "to": tail[0], "summary": "x\n\ny"})
        unknown = await _call(root, {"type": "context_fold", "from": 999_999, "summary": "x\n\ny"})

        root2, _stop = await restart(root, core)
        WRAPS["ai"] = replace_with(_Stub())
        again = await store_rows(root2, core, "ai")
        again_by = _dump(again)

        print(f"     out={out} fold={fold.get('id')} refs={fold.get('refs')} prefix={len(prefix)} tail={len(tail)}")
        checks = [
            ("the fold replies folded, 2 rows, summarized",
             bool(out.get("folded")) and out.get("rows") == 2 and out.get("summarized") is True),
            ("a fold row holds exactly the range as refs and starts [fold <id> ·",
             fold.get("refs") == [F, T] and str(fold.get("content", "")).startswith(f"[fold {fold.get('id')} ·")),
            ("the fold is anchored at the smallest member id", fold.get("anchor") == F),
            ("every row before the range is byte-identical in the store",
             len(prefix) >= 4 and all(after.get(i) == snap[i] for i in prefix)),
            ("every row after the range is byte-identical in the store",
             bool(tail) and all(after.get(i) == snap[i] for i in tail)),
            ("the members stay in the store (nothing deleted)", F in after and T in after),
            ("the next prompt keeps the rows before the range, same order",
             at > 0 and post_prompt[:at] == pre_prompt[:at]
             and len(pre_prompt) > at + 1 and by[F]["content"] == pre_prompt[at]
             and by[T]["content"] == pre_prompt[at + 1]),
            ("exactly one fold line stands in for exactly the two members",
             n_fold == 1 and by[F]["content"] not in post_prompt and by[T]["content"] not in post_prompt
             and post_prompt[at + 1: at + 1 + (len(pre_prompt) - at - 2)] == pre_prompt[at + 2:]),
            ("an inverted range is refused naming its arg", "to" in json.dumps(bad) and bool(bad.get("error")),),
            ("an unknown id is refused naming its arg", "from" in json.dumps(unknown) and bool(unknown.get("error"))),
            ("a kernel restart does not rewrite the prefix",
             all(again_by.get(i) == snap[i] for i in prefix + tail)),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "context_fold left the rows before the range byte-identical"
                if allok else "a context_fold check failed")
    finally:
        WRAPS.pop("ai", None)
        await close()


if __name__ == "__main__":
    asyncio.run(main())
