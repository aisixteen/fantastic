"""ONE fake for every ai_core test: the kernel row store, as a bundle sees it.

`_ctx(kernel)` in core returns `kernel.ctx` when present and the object itself otherwise,
so a FakeCtx can be handed straight in as `kernel`. It carries the three tables and the
same method names the Kernel has, a `get()` that answers the agent record, and a `send()`
that swallows dispatches (reflects, deliveries) while remembering them."""

from __future__ import annotations

import time


class FakeCtx:
    def __init__(self, record: dict | None = None, *, window: int = 12000):
        self.record = (
            record
            if record is not None
            else {"context_window": window, "output_reserve": 0}
        )
        self.rows: dict[int, dict] = {}
        self._by_agent: dict[str, list[int]] = {}
        self.sent: list = []
        self.emitted: list = []
        self.tasks: dict = {}
        self.next_id = 1
        # TEST CONVENIENCE: a fixture may name rows with tags ("user:goal", "ai:a0")
        # and the fake translates each tag to a real int id on first sight, so a
        # test reads as prose while the store holds what the kernel holds. `id(tag)`
        # returns the int a tag became (minting it if unseen).
        self._alias: dict[str, int] = {}

    # ── the Kernel surface a bundle uses ─────────────────────────────────
    def mint_id(self) -> int:
        n = self.next_id
        self.next_id = n + 1
        return n

    def id(self, tag) -> int:
        if isinstance(tag, int) and not isinstance(tag, bool):
            return tag
        if tag not in self._alias:
            self._alias[tag] = self.mint_id()
        return self._alias[tag]

    def _translate(self, row: dict) -> dict:
        out = dict(row)
        if isinstance(out.get("id"), str):
            out["id"] = self.id(out["id"])
        if isinstance(out.get("parent"), str):
            out["parent"] = self.id(out["parent"])
        if isinstance(out.get("refs"), list):
            out["refs"] = [self.id(x) if isinstance(x, str) else x for x in out["refs"]]
        return out

    def rows_put(self, agent_id: str, row: dict) -> int:
        row = self._translate(row)
        rid = row.get("id")
        if rid is None:
            rid = self.mint_id()
        elif rid in self.rows:
            raise ValueError(f"rows_put: id already stored, got {rid!r}")
        else:
            # A fixture may hand a row an int it chose (a fold built by hand): the
            # fake treats it as pre-minted and keeps the counter above it.
            self.next_id = max(self.next_id, rid + 1)
        stored = {**row, "id": rid, "agent": agent_id}
        stored.setdefault("ts", time.time())
        self.rows[rid] = stored
        self._by_agent.setdefault(agent_id, []).append(rid)
        return rid

    def rows_for(self, agent_id: str) -> list[dict]:
        return [
            self.rows[i] for i in self._by_agent.get(agent_id, ()) if i in self.rows
        ]

    def rows_get(self, ids: list[int]) -> list[dict]:
        return [self.rows[i] for i in ids if i in self.rows]

    def body_put(self, agent_id: str, pid, text: str) -> int:
        return self.rows_put(
            agent_id, {"kind": "body", "parent": self.id(pid), "content": text}
        )

    def body_get(self, pid, offset: int, length: int):
        pid = self.id(pid)
        row = next(
            (
                r
                for r in reversed(list(self.rows.values()))
                if r.get("kind") == "body" and r.get("parent") == pid
            ),
            None,
        )
        if row is None:
            return None
        b = str(row.get("content", ""))
        chunk = b[offset : offset + max(1, length)]
        nxt = offset + len(chunk)
        return chunk, nxt, nxt >= len(b), len(b)

    def view_put(self, agent_id: str, doc: dict) -> int:
        return self.rows_put(agent_id, {**doc, "kind": "view"})

    def view_get(self, agent_id: str) -> dict | None:
        row = next(
            (r for r in reversed(self.rows_for(agent_id)) if r.get("kind") == "view"),
            None,
        )
        if row is None:
            return None
        return {
            k: v for k, v in row.items() if k not in ("kind", "agent", "id", "parent")
        }

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
