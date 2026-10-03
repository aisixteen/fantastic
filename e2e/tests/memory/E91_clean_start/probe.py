"""memory/clean_start — a kernel on an EMPTY memory store (a stubbed model).

`kernel_start` with a fresh ram_store; an AI agent created and wired to it (explicit
gates both ends); its boot is lazy — nothing read. The first turn reads an EMPTY
head, answers, and writes its turn (user + assistant rows) into its own key; the
kernel's execution rows land under `kernel` in the same store. Nothing errors."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import memory, verdict
from helpers.kernel import WRAPS, open_kernel, replace_with


class _Stub:
    last_prompt_tokens = None

    async def chat(self, messages, max_tokens=None):
        yield 'hello <call name="done"></call>'


async def main():
    root, core, close = await open_kernel(prefix="memory_clean_", reach=["ai", "cli"])
    try:
        import ollama_backend.tools  # noqa: F401 — the bundle the AI runs

        r = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": root.id, "id": "ai",
            "handler_module": "ollama_backend.tools", "model": "stub",
            # required at the write; never dialled — the provider is stubbed below
            "endpoint": "http://127.0.0.1:9",
            **memory.ai_memory_meta({"cli": "*"})}}]))[0]["result"]
        assert not r.get("error"), r
        await memory.open_ingress(root, memory.MEMORY, "ai")
        before = await memory.rows_of(root, "ai")
        WRAPS["ai"] = replace_with(_Stub())
        out = (await root.send([{"target": "ai", "payload": {"type": "send", "text": "hi"}}]))[0]
        await asyncio.sleep(0.3)
        rows = await memory.rows_of(root, "ai", conversation_only=True)
        await root.ctx.flush_executions()
        tables = await memory.dump(root)
        guid = root.record["guid"]
        kernel_rows = [x for x in tables["rows"] if x["owner"] == f"{guid}|kernel"]
        ai_rows = [x for x in tables["rows"] if x["owner"] == f"{guid}|ai"]
        roles = [x.get("role") for x in rows]
        print(f"     before={len(before)}  reply={out['status']}  roles={roles}  "
              f"kernel_rows={len(kernel_rows)}  ai_rows={len(ai_rows)}")
        checks = [
            ("the store started empty for the agent", before == []),
            ("the first turn ran without error", out["status"] == "completed"
             and not (out["result"] or {}).get("error")),
            ("its turn is in its own key: user then assistant", roles[:2] == ["user", "assistant"]),
            ("the kernel's execution rows landed under `kernel`", len(kernel_rows) > 0),
            ("the agent's rows and the kernel's are separate keys", len(ai_rows) >= 2),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "clean start: empty head, first turn stored, kernel rows under kernel"
                if allok else "a clean_start check failed")
    finally:
        WRAPS.pop("ai", None)
        await close()


if __name__ == "__main__":
    asyncio.run(main())
