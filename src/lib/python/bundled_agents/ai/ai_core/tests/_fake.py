"""ONE fake for every ai_core test: the kernel row store, as a bundle sees it.

`_ctx(kernel)` in core returns `kernel.ctx` when present and the object itself otherwise,
so a FakeCtx can be handed straight in as `kernel`. It carries the three tables and the
same method names the Kernel has, a `get()` that answers the agent record, and a `send()`
that swallows dispatches (reflects, deliveries) while remembering them."""

from __future__ import annotations

import secrets
import time


class FakeCtx:
    def __init__(self, record: dict | None = None, *, window: int = 12000):
        self.record = (
            record
            if record is not None
            else {"context_window": window, "output_reserve": 0}
        )
        self.rows: dict[str, dict] = {}
        self.bodies: dict[str, bytes] = {}
        self.views: dict[str, dict] = {}
        self._by_agent: dict[str, list[str]] = {}
        self.sent: list = []
        self.emitted: list = []
        self.tasks: dict = {}

    # ── the Kernel surface a bundle uses ─────────────────────────────────
    def mint_task_id(self, agent_id: str) -> str:
        while True:
            tid = f"{agent_id}:{secrets.token_hex(4)}"
            if tid not in self.rows and tid not in self.tasks:
                return tid

    def rows_put(self, agent_id: str, row: dict) -> str:
        rid = row.get("id") or self.mint_task_id(agent_id)
        stored = {**row, "id": rid, "agent": agent_id}
        stored.setdefault("ts", time.time())
        self.rows[rid] = stored
        self._by_agent.setdefault(agent_id, []).append(rid)
        return rid

    def rows_for(self, agent_id: str) -> list[dict]:
        return [
            self.rows[i] for i in self._by_agent.get(agent_id, ()) if i in self.rows
        ]

    def rows_get(self, ids: list[str]) -> list[dict]:
        return [self.rows[i] for i in ids if i in self.rows]

    def body_put(self, pid: str, data: bytes) -> None:
        self.bodies[pid] = bytes(data)

    def body_get(self, pid: str, offset: int, length: int):
        b = self.bodies.get(pid)
        if b is None:
            return None
        chunk = b[offset : offset + max(1, length)]
        nxt = offset + len(chunk)
        return chunk, nxt, nxt >= len(b), len(b)

    def view_put(self, agent_id: str, doc: dict) -> None:
        self.views[agent_id] = dict(doc)

    def view_get(self, agent_id: str) -> dict | None:
        return self.views.get(agent_id)

    # ── the Agent surface a bundle uses ──────────────────────────────────
    def get(self, _id):
        return self.record

    async def send(self, batch):
        for it in batch:
            self.sent.append((it.get("target"), it.get("payload")))
        return [{"pid": None, "result": {}, "status": "completed"} for _ in batch]

    async def emit(self, target, payload):
        self.emitted.append((target, payload))

    # test conveniences
    def rows_of(self, agent_id: str = "ai") -> list[dict]:
        return self.rows_for(agent_id)

    def folds_of(self, agent_id: str = "ai") -> list[dict]:
        return [r for r in self.rows_for(agent_id) if r.get("kind") == "fold"]
