"""ONE fake for every ai_core test: the kernel counter + the agent's MEMORY STORE, as a
bundle sees them.

`_ctx(kernel)` in core returns `kernel.ctx` when present and the object itself otherwise,
so a FakeCtx can be handed straight in as `kernel`. It answers `mint_id()` (the kernel's
one counter), `get()` (the agent record — naming `memory_store_id: mem`), and `send()`:
a send to `mem` is answered by an in-memory store with the ram_store verbs the bundle
uses (append / read / search / grants), every other dispatch is swallowed and
remembered. The store's rows are keyed by the one owner a test drives (`owner`, "ai").
Tests seed rows with `rows_put(owner, row)` and read them with `rows_of()`."""

from __future__ import annotations

import time

STORE = "mem"


class FakeCtx:
    def __init__(self, record: dict | None = None, *, window: int = 12000):
        self.record = (
            record
            if record is not None
            else {"context_window": window, "output_reserve": 0}
        )
        self.record.setdefault("memory_store_id", STORE)
        self.owner = "ai"
        self.rows: dict[int, dict] = {}
        self._by_agent: dict[str, list[int]] = {}
        self.sent: list = []
        self.store_calls: list = []
        self.emitted: list = []
        self.tasks: dict = {}
        self.inboxes: dict = {}
        self.next_id = 1
        # TEST CONVENIENCE: a fixture may name rows with tags ("user:goal", "ai:a0")
        # and the fake translates each tag to a real int id on first sight, so a
        # test reads as prose while the store holds what a store holds. `id(tag)`
        # returns the int a tag became (minting it if unseen).
        self._alias: dict[str, int] = {}

    # ── the Kernel surface a bundle uses ─────────────────────────────────
    def _verb_docs(self, payload):
        """The kernel's `docs`, read in process by routing: this fake documents
        no verb, so no routing field clashes."""
        return {"of": payload.get("of"), "verbs": {}}

    def spawn_task(self, agent_id: str, verb: str, coro) -> int:
        """Fork background work the way the kernel does — a registered task with
        an id from the one counter (ai_core delivers replies this way; there is no
        second, unregistered path for a fake to fall into)."""
        import asyncio

        tid = self.mint_id()
        self.tasks[tid] = {
            "agent": agent_id,
            "verb": verb,
            "_task": asyncio.get_running_loop().create_task(coro),
        }
        return tid

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
        """Seed a row straight into the store under `agent_id`."""
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

    # ── the store (ram_store verbs, one owner) ───────────────────────────
    def _store(self, p: dict) -> dict:
        t = p.get("type")
        owner = self.owner
        if t == "append":
            ids = []
            for r in p.get("rows") or []:
                ids.append(
                    self.rows_put(owner, {k: v for k, v in r.items() if k != "agent"})
                )
            return {"ids": ids, "appended": len(ids)}
        if t == "read":
            if p.get("grant") is not None:
                return {"error": "fake store: no grants", "reason": "not_found"}
            # APPEND ORDER. In the real store an agent's own rows come back in id
            # order, and for them append order IS id order (the kernel mints as they
            # happen); fixtures choose ids freely, so the fake keeps append order.
            rows = list(self.rows_for(owner))
            flk = p.get("from_last_kind")
            if flk:
                last = next(
                    (r["id"] for r in reversed(rows) if r.get("kind") == flk), None
                )
                if last is not None:
                    rows = [r for r in rows if r["id"] >= last]
            if p.get("after") is not None:
                rows = [r for r in rows if r["id"] > p["after"]]
            if p.get("kinds"):
                rows = [r for r in rows if r.get("kind") in p["kinds"]]
            if p.get("ids") is not None:
                rows = [r for r in rows if r["id"] in set(p["ids"])]
            if p.get("order") == "desc":
                rows = list(reversed(rows))
            if p.get("limit"):
                rows = rows[: p["limit"]]
            return {
                "rows": [{k: v for k, v in r.items() if k != "agent"} for r in rows],
                "next": None,
                "total": len(rows),
            }
        if t == "search":
            q = str(p.get("query") or "").lower()
            width = p.get("width") or 200
            rows = list(self.rows_for(owner))
            if p.get("ids"):
                folds = {r["id"]: r for r in rows if r.get("refs")}
                scope, stack = set(), list(p["ids"])
                while stack:
                    x = stack.pop()
                    if x in scope:
                        continue
                    scope.add(x)
                    stack.extend((folds.get(x) or {}).get("refs") or [])
                rows = [r for r in rows if r["id"] in scope]
            out = []
            for r in rows:
                c = r.get("content")
                text = c if isinstance(c, str) else str(c)
                if r.get("title"):
                    text = f"{r['title']}\n{text}"
                for n, line in enumerate(text.splitlines(), 1):
                    if q in line.lower():
                        out.append(
                            {"id": r["id"], "line": n, "text": line.strip()[:width]}
                        )
            return {"matches": out, "total": len(out), "truncated": False}
        if t == "grants":
            return {"grants": []}
        return {"error": f"fake store: unknown type {t!r}", "reason": "unhandled"}

    # ── the Agent surface a bundle uses ──────────────────────────────────
    def get(self, _id):
        return self.record

    async def send(self, batch):
        out = []
        for it in batch:
            p = it.get("payload") or {}
            if it.get("target") == self.record.get("memory_store_id"):
                self.store_calls.append(p)
                res = self._store(p)
                out.append({"pid": None, "result": res, "status": "completed"})
                continue
            self.sent.append((it.get("target"), p))
            out.append({"pid": None, "result": {}, "status": "completed"})
        return out

    async def emit(self, target, payload, **_kw):
        self.emitted.append((target, payload))

    # test conveniences
    def rows_of(self, agent_id: str = "ai") -> list[dict]:
        return self.rows_for(agent_id)

    def folds_of(self, agent_id: str = "ai") -> list[dict]:
        return [r for r in self.rows_for(agent_id) if r.get("kind") == "fold"]
