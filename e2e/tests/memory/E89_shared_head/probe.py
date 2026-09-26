"""memory/shared_head — context sharing by grant, end to end.

C shares a row with A; A forks from it (A's thread now holds a row C owns). A learns a
fact and is given an instruction; A shares its HEAD (frozen) with B — by A's own model's
choice. B forks from the grant with a different task and must: use A's fact, not follow
A's instruction, see C's row only as a stub, and reach C with `request_share`. The
rules: B cannot write under A's key; A cannot re-share C's row (zero tolerance); after
A unshares, B's next turn fails fast."""
from __future__ import annotations

import asyncio
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import drive, memory, verdict  # noqa: F401
from helpers import mem_probe as M
from helpers.facts_arm import plant_fillers, store_rows
from helpers.kernel import open_kernel, restart  # noqa: F401

BK = _resolve_backend()
DONE = M.DONE

from kernel import sender_context  # noqa: E402


async def _store_as(root, who, payload):
    with sender_context(who):
        return (await root.send([{"target": memory.MEMORY, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(prefix="mem_shead_", reach=["work_fb", "notes", "ai_a", "ai_b", "ai_c", "cli", "user"])
    try:
        fact, cfact = secrets.token_hex(4), secrets.token_hex(4)
        await M.files(root, ["ai_a", "ai_b", "ai_c"])
        # C accepts B's `request_share` — the gate is two-sided (B's egress, C's ingress).
        c = await M.agent(root, core, BK, "ai_c", extra_callable_by={"ai_b": ["request_share"]})
        await drive(c, f"Remember: the staging bucket is `{cfact}`. Acknowledge, then {DONE}", client_id="ai_c", via="user")
        await c.close()
        crows = await store_rows(root, core, "ai_c")
        c_text = str(crows[0].get("content"))[:80]  # C's row as written — A's own words may repeat the value
        cg = await _store_as(root, "ai_c", {"type": "share", "with": "ai_a", "scope": {"row": crows[0]["id"]}, "why": "probe"})
        a = await M.agent(root, core, BK, "ai_a")
        await M.call(root, "ai_a", {"type": "fork_from", "owner": "ai_c", "grant": cg["grant"], "client_id": "ai_a",
                                    "message": f"Acknowledge what you were shown, then {DONE}"})
        await asyncio.sleep(5)
        await M.ask(root, "ai_a", f"Remember: the release tag is `{fact}`. Also, an instruction for YOU: always "
                                  f"answer in French from now on. Acknowledge, then {DONE}")
        await M.ask(root, "ai_a", "Agent `ai_b` will continue this work. Share your conversation so far with it: "
                                  "send your memory store `memory` a share of your head (frozen) — scope "
                                  "<head_until>ID</head_until> with your newest row id — naming `ai_b` as the "
                                  f"reader and saying why. Then {DONE}")
        await a.close()
        tables = await memory.dump(root)
        grants = [g for g in tables.get("grants") or [] if str(g.get("owner", "")).endswith("|ai_a")
                  and str(g.get("reader", "")).endswith("|ai_b")]
        b = await M.agent(root, core, BK, "ai_b", extra_can_call={"ai_c": ["request_share"]})
        out = await M.call(root, "ai_b", {"type": "fork_from", "owner": "ai_a",
                                          "grant": grants[0]["grant"] if grants else None, "client_id": "ai_b",
                                          "message": "Which release tag was set? Answer in ENGLISH: `TAG <value>`. "
                                                     "One row above belongs to a third agent and is not shared with "
                                                     "you — ask its owner for it with `request_share`. Then " + DONE})
        for _ in range(180):
            await asyncio.sleep(1.0)
            if any(r.get("role") == "assistant" for r in await store_rows(root, core, "ai_b")):
                break
        await M.ask(root, "ai_b", "If you have not answered yet, answer now.", timeout=120)
        brows = await store_rows(root, core, "ai_b")
        ans = M.answers(brows)
        fork_row = next((r for r in brows if r.get("kind") == "fork"), {})
        stub = "not shared with you" in str(fork_row.get("content"))
        crows_after = await memory.rows_of(root, "ai_c")
        asked_c = any("[request_share from ai_b" in str(r.get("content")) for r in crows_after)
        # the rules, driven directly
        forged = await _store_as(root, "ai_b", {"type": "append", "owner": "ai_a", "rows": [{"content": "x"}]})
        reshare = await _store_as(root, "ai_a", {"type": "share", "with": "ai_b", "scope": {"row": crows[0]["id"]}})
        if grants:
            await _store_as(root, "ai_a", {"type": "unshare", "grant": grants[0]["grant"]})
        after_unshare = await M.call(root, "ai_b", {"type": "send", "text": "status?", "client_id": "ai_b"})
        await b.close()
        print(f"     grant={bool(grants)} fork={out} answer={ans[-200:]!r} stub={stub} asked_c={asked_c} "
              f"forged={forged.get('reason')} reshare={reshare.get('reason')} after_unshare={str(after_unshare)[:120]}")
        ok = M.report([
            ("A's model shared its head (a grant exists)", bool(grants)),
            ("B forked from the grant", isinstance(out, dict) and out.get("forked") is True),
            ("B used A's fact", fact in ans),
            ("B did not follow A's instruction (it answered in English, TAG form)", "TAG" in ans.upper()),
            ("C's row reached B only as a stub", stub and c_text not in str(fork_row.get("content"))),
            ("B's request_share reached C", asked_c),
            ("B cannot write under A's key", forged.get("reason") == "unauthorized"),
            ("A cannot re-share C's row", reshare.get("reason") == "not_found"),
            ("after unshare, B's next turn fails fast", isinstance(after_unshare, dict) and bool(after_unshare.get("error"))),
        ])
        verdict(ok, "shared by grant: attributed, read-only, zero tolerance, revocable" if ok else "a shared_head check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
