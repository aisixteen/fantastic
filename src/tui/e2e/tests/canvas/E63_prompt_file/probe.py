"""E63 prompt_file — `.fantastic/prompt.md`, the project's system block:

a) a fresh project SEEDS it with ai_core's default block as placeholders
   (`{tree}`, `{self}`, `{howto}`) and the CLI's building rules — so the first
   turn's system block is the default one (who it is, the tree, the how-tos)
   and the rules;
b) a canvas AI names it (`system_prompt_file: {bridge: prompt_file, path:
   prompt.md}`) and reads it through the bridge, as itself;
c) an EDIT of the file is the block from the next turn on — no restart; the
   placeholders are still filled (its own id), the replaced default is gone.

The model is the parity chat stub: it records each request and answers in plain
text (a turn ends)."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, say, spot  # noqa: E402

E2E = Path(__file__).resolve().parents[6] / "e2e"
MARK = "CUSTOM-SYS-63"


class _Rec:
    def __init__(self, root: Path) -> None:
        self.root = root

    def raw(self, rel: str, data: bytes) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def json(self, rel: str, obj) -> None:
        p = self.root / f"{rel}.json"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(obj, default=str))


def main() -> bool:
    import socket
    sys.path.insert(0, str(E2E / "parity"))
    from stubs.openai_chat import Handler, OpenAIChatStub  # noqa: E402

    rig = Rig(__file__, live=False)
    rig.build("vllm")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    stub = OpenAIChatStub(_Rec(rig.art / "stub"), "stub", port)
    systems: list[str] = []
    #: each request's newest message, beside its system block: a plain answer
    #: starts the runtime's no-call reminders, turns of their own, so "the next
    #: request" may be one of those — a turn is found by what it answers
    lasts: list[str] = []

    def responder(req: dict, _raw: bytes):
        msgs = req.get("messages") or []
        if msgs and msgs[0].get("role") == "system":
            c, last = msgs[0].get("content"), msgs[-1].get("content")
            systems.append(c if isinstance(c, str) else json.dumps(c))
            lasts.append(last if isinstance(last, str) else json.dumps(last))
        return "ok"
    stub.responder = responder
    stub.start(Handler)
    # the saved set-up: every count named, so nothing is read from the (stub) server
    rig.make_home({"handler_module": "vllm_backend.tools", "endpoint": stub.base_url(), "model": "stub",
                   "context_window": 32768, "max_tokens": 4096, "kv_slots": 4, "kv_overflow": "evict"})
    rig.init()
    prompt = rig.proj / ".fantastic" / "prompt.md"
    r: dict = {}
    info: dict = {}
    secs = rig.limits["seconds"]

    async def turns(n: int) -> bool:
        for _ in range(secs * 2):
            if len(systems) >= n:
                return True
            await asyncio.sleep(0.5)
        return False

    async def turn_of(words: str, start: int) -> int | None:
        """The index of the first request from `start` on answering `words`."""
        for _ in range(secs * 2):
            hit = next((i for i in range(start, len(lasts)) if words in lasts[i]), None)
            if hit is not None:
                return hit
            await asyncio.sleep(0.5)
        return None

    async def drive() -> None:
        async with rig.page(1600, 1000) as pg:
            await asyncio.sleep(1.5)
            # a) seeded, the default layout
            seeded = prompt.read_text() if prompt.exists() else ""
            r["seeded"] = seeded.startswith("{tree}\n\n{self}\n\n{howto}\n\n## Building here\n") and "create_agent" in seeded
            await spot(pg, 400, 300, "hello")
            r["turn1"] = await turns(1)
            s1 = systems[0] if systems else ""
            info["turn1"] = s1[:300]
            r["turn1_default"] = "You are `" in s1 and "## Reaching other agents" in s1 and "{tree}" not in s1 and "## Building here" in s1
            # b) the AI names the file
            import yaml
            recs = [yaml.safe_load(p.read_text()) or {} for p in (rig.proj / ".fantastic" / "agents").rglob("agent.yaml")]
            rec = next((x for x in recs if x.get("handler_module") == "vllm_backend.tools"), {})
            info["ai"] = rec.get("id")
            r["ai_names_file"] = rec.get("file_bridge_id") == "prompt_file" and rec.get("system_prompt_file") == ".fantastic/prompt.md"
            r["records_persist"] = "persist through 'prompt_file' failed" not in (rig.proj / ".fantastic" / "cli" / "kernel.log").read_text(errors="ignore")
            # c) an edit is the block on the next turn: the turn that answers "again"
            await asyncio.sleep(4)
            before = len(systems)
            prompt.write_text(f"{MARK} — you are on a test canvas.\n\n{{self}}\n")
            await asyncio.sleep(1.5)
            await say(pg, "again")
            at = await turn_of("again", before)
            r["turn2"] = at is not None
            s2 = systems[at] if at is not None else ""
            info["turn2"] = s2[:300]
            r["turn2_edited"] = s2.startswith(MARK) and f"You are `{rec.get('id', '?')}`" in s2 and "## Reaching other agents" not in s2

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
        stub.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
