"""coding/messages_gate — the kernel's `messages` verb, gated by the OWNER of the rows.

The AI's conversation lives in the kernel's row store under its id. `messages` on the
root reads rows by id at digest resolution; the verb is answered by the root (kernel
verbs skip ingress), so the handler re-runs the owner's ingress itself: the owner
always reads its own rows, the operator reads all, a peer reads them only once the
owner's `callable_by` names it for `messages` — and refusals are LISTED, never
silently dropped. Bodies never travel this way (they stay behind the owner's `pull`).
Driven from above: a planted store, a `user_probe` peer that sends AS ITSELF, the
operator.
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
from helpers import Limits, connect, verdict
from helpers.facts_arm import call, plant_store, store_rows
from helpers.kernel import open_kernel

BK = _resolve_backend()
DIGEST_KEYS = {"id", "agent", "parent", "kind", "role", "head", "bytes", "title", "level", "refs"}


async def main():
    root, core, close = await open_kernel(prefix="coding_msgs_", reach=["ai", "peer", "cli", "user"])
    try:
        secret = secrets.token_hex(4)
        mint = root.ctx.mint_id  # ids come from the kernel's one counter, planted or not
        pid = mint()
        goal = {"role": "user", "id": mint(), "parent": None,
                "content": f"The vault code is `{secret}`. Keep it."}
        turn = {"role": "assistant", "id": mint(), "parent": goal["id"],
                "content": "Noted. " + "x" * 400}
        reply = {"role": "user", "id": mint(), "parent": turn["id"],
                 "content": f'[reply to {pid} · notes read · "the spec" · ↑{turn["id"]}]\n'
                            f"(settled, 5000 bytes — NOT inlined. To find a line in it: send yourself "
                            f"{{type:'grep', query:'<substring>', ids:[{pid}]}}. To read it: {{type:'pull', pid:{pid}, offset:0}})"}
        await plant_store(core, root, "ai", [goal, turn, reply], {pid: "BODY " * 1000})
        ids = [r["id"] for r in await store_rows(root, core, "ai")]
        assert len(ids) == 3, ids

        # the owner — a real AI record (create fires boot → recall over the planted rows)
        conn = await connect(root, core, BK, agent_id="ai",
                             can_call={"cli": "*", "user": "*", "ai": "*"},
                             callable_by={"user": "*"},
                             limits=Limits.from_env(os.environ),
                             record={"context_window": 12000, "num_ctx": 12000})
        # the peer — sends AS ITSELF (a gated peer -> root call with its own ledger row)
        r = await call(root, root.id, {
            "type": "create_agent", "parent_id": root.id, "id": "peer",
            "handler_module": "user_probe.tools", "answer": "n/a",
            "can_call": {root.id: ["messages"]},
            "callable_by": {"rest": "*", "web": "*"},
        })
        assert not r.get("error"), f"create peer: {r}"

        async def as_peer():
            out = await call(root, "peer", {"type": "send", "target": root.id,
                                            "payload": {"type": "messages", "ids": ids}})
            return (out or {}).get("result") or {}

        # 1. nobody named the peer: every id refused, nothing returned
        cold = await as_peer()
        # 2. the owner names the peer for `messages`: digests come back
        # `update_agent` REPLACES `callable_by` whole. Over the wire the driver's
        # own ingress (`rest`, and `web` for watch) was granted at create by
        # `connect`; restating the map without it locks the driver out of the
        # agent it is driving — step 4's `own` send was refused `ai does not
        # accept "send" from rest` (2026-09-12). In process the probe is the
        # operator and never notices.
        leg = {getattr(root, "leg_id"): "*", **{i: "*" for i in getattr(root, "watch_ids", ())}} \
            if getattr(root, "leg_id", None) else {}
        r = await call(root, root.id, {"type": "update_agent", "id": "ai",
                                       "callable_by": {"user": "*", "peer": ["messages"], **leg}})
        assert not (isinstance(r, dict) and r.get("error")), f"grant: {r}"
        warm = await as_peer()
        # 3. the operator sees all
        op = await call(root, root.id, {"type": "messages", "ids": ids})
        # 4. the owner reads its own rows through its own send path
        own = await call(root, "ai", {"type": "send", "target": root.id, "payload": {"type": "messages", "ids": ids}})
        await conn.close()

        cold_rows, warm_rows, op_rows = (x.get("rows") or [] for x in (cold, warm, op))
        warm_keys = set().union(*(set(d) for d in warm_rows)) if warm_rows else set()
        checks = [
            ("cold: the peer gets no rows", cold_rows == []),
            ("cold: every id is listed as refused", sorted(cold.get("refused") or []) == sorted(ids)),
            ("warm: the peer gets every row", sorted(d["id"] for d in warm_rows) == sorted(ids)),
            ("warm: nothing refused", not warm.get("refused")),
            ("warm: digests only — no content, no body", warm_keys <= DIGEST_KEYS and "content" not in warm_keys),
            ("warm: the head is capped", all(len(d.get("head") or "") <= 120 for d in warm_rows)),
            ("warm: the secret is visible in the goal's head", any(secret in (d.get("head") or "") for d in warm_rows)),
            ("warm: the body stays behind pull", all("BODY BODY" not in str(d) for d in warm_rows)),
            ("operator: sees all", sorted(d["id"] for d in op_rows) == sorted(ids) and not op.get("refused")),
            ("owner: reads its own rows", isinstance(own, dict) and not own.get("error")),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        print(f"  cold={cold!r}"[:300]); print(f"  own={own!r}"[:300])
        allok = all(ok for _, ok in checks)
        verdict(allok, "rows read by id; the peer sees them only once the owner names it"
                if allok else "a messages_gate check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
