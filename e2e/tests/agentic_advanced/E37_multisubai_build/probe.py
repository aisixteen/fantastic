"""LIVE TEST — MULTISUBAI (build): ONE creator builds the three-solver team, wires it,
kicks it off, walks away — and is WOKEN by the team when it concludes, validates
the answer, DELETES the fleet, and stops.

E36 measures the swarm with a harness-built team. This one measures the CREATOR:
create three AI agents as its children, give each exactly one oracle (both sides of
the edge), let every pair talk (both sides), point them at `notes`, start all three
in ONE batch with replies DISCARDED, call `done`. Then, when the solvers have
agreed and written the code, the writer must message the creator; the creator's
turn wakes, reads `answer.txt` on `notes`, confirms the code the solvers report is
the code on disk, tells the human, deletes its three solvers (its own children —
baseline authority) and ends with `done`. That last leg — a sleeping mind woken by
its own creation, validating, cleaning up and stopping — is the point of the test.
Acceptance (user, 2026-09-10): the reported answer AND the deletion of the sub-agents.

The previous creator run (2026-09-10, killed at turn 13) was not stuck but SLOW:
turns of reflecting, a probe program, one `update_agent` per grant. So the task
here is as detailed as a spec — the record shape to copy, the exact grant maps
(and that `update_agent` REPLACES a named field whole), the solver instructions
verbatim, the batch kickoff with `<reply_to>null</reply_to>`, and `done`. What is
left to the creator is doing it, correctly, in few turns.

Reuses E30's driver (`P.run`) with E36's puzzle, oracle and kickoff text. THREE
solver lanes plus the creator's: run alone.
"""

from __future__ import annotations

import asyncio
import re
import sys
import time
from pathlib import Path

sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from helpers import verdict  # noqa: E402
from tests.agentic_advanced.E30_subai import probe as P  # noqa: E402
from tests.agentic_advanced.E36_multisubai import probe as S  # noqa: E402

CREATOR = "ai"
ORACLES, SOLVERS, LEDGER_NAMES, N = S.ORACLES, S.SOLVERS, S.LEDGER_NAMES, S.N
MINE = {"kernel_state", "core", "kernel_store", "brain", "notes", "user", CREATOR, *ORACLES}
UNATTENDED = S.UNATTENDED


def solver_instructions(k: int) -> str:
    """E36's kickoff text plus the closing step: tell the creator."""
    base = S.kickoff(k)
    return base.replace(
        "End every turn with `done` once you have nothing more to send;",
        f"When the code has been written to `answer.txt`, the writer sends `{CREATOR}` "
        "(your creator) one message: `CONCLUDED <code>` with one line per digit saying "
        "who owned it and how it was derived; the other two send nothing further. End "
        "every turn with `done` once you have nothing more to send;",
    )


TASK = (
    "You are the CREATOR here — the mind building the structure. I am going to walk "
    "away, but I am an AGENT here too: if you genuinely cannot proceed without a "
    "decision that is mine, reflect the tree, find me, and send me an `inquiry`.\n\n"
    f"This kernel holds THREE oracles, `{ORACLES[0]}` (the PEOPLE ledger), "
    f"`{ORACLES[1]}` (the ROOMS ledger), `{ORACLES[2]}` (the OWNERSHIP ledger) — each "
    "answers `ask` with a `question` and knows ONLY its own ledger. Together they hide "
    "a 3-digit SAFE CODE. There is also `notes` (a text document agent; verbs write/"
    "read, args `path` and `bytes`) and `brain`, an AI agent whose RECORD is your "
    "template. Do NOT solve the puzzle yourself and do NOT relay facts between "
    "solvers. Build the team, start it, walk away, and come back only when it calls "
    "you.\n\n"
    "DO EXACTLY THIS, in as few turns as you can — every step is spelled out so you "
    "need no exploration beyond one `list_agents`:\n\n"
    f"STEP 1 — CREATE THREE SOLVERS as YOUR OWN CHILDREN, ids `{SOLVERS[0]}`, "
    f"`{SOLVERS[1]}`, `{SOLVERS[2]}`, in ONE batch of three `create_agent` calls to "
    "`kernel`. Send `kernel` `<type>list_agents</type>` once and copy from the "
    "`brain` record these fields verbatim into each solver: `handler_module`, "
    "`model`, `endpoint`, `max_tokens`, `file_bridge_id`, `context_window`. Set "
    "`parent_id` to YOUR id. Set `description` to what the solver is for "
    f"(solver k holds the ledger of `oracle_k`). Give each solver these grant maps "
    "(a map is a tag whose children are keyed by agent id or a relative token):\n"
    f"  `<can_call>`: `<$self>*</$self> <$parent>*</$parent> <$children>*</$children> "
    f"<oracle_k>*</oracle_k> <notes>*</notes>` and one `<solver_j>*</solver_j>` for "
    "EACH of the two other solvers.\n"
    "  `<callable_by>`: `<$self>*</$self> <$parent>*</$parent> <$children>*</$children>` "
    "and one `<solver_j>*</solver_j>` for EACH of the two other solvers.\n"
    "That is: solver k reaches its own oracle and both peers, and accepts both peers. "
    "You are their parent, so they can reach you and you them without more grants.\n\n"
    "STEP 2 — OPEN THE ORACLES' DOORS. Each oracle accepts only me today. For each "
    "oracle_k, send `kernel` an `update_agent` with `<id>oracle_k</id>` and a "
    "`<callable_by>` map that contains EVERYTHING it has now PLUS `<solver_k>*"
    "</solver_k>` — `update_agent` REPLACES a named field whole, it does not merge, "
    "so read the current map from `list_agents` and resend all of it. Also send "
    "`kernel` one `update_agent` for `notes` adding all three solvers to ITS "
    "`<callable_by>` the same way. Do all four updates in ONE batch.\n\n"
    "STEP 3 — KICK OFF ALL THREE AT ONCE: one batch of three `send` calls, one per "
    "solver, each with `<type>send</type>`, `<client_id>` = the solver's id, "
    "`<reply_to>null</reply_to>` (their first turns are NOT yours to read), and "
    "`<text>` = that solver's instructions, VERBATIM from the three blocks below.\n\n"
    "STEP 4 — tell me in one paragraph what you built, then call `done`. Do not poll, "
    "do not read `answer.txt` yet, do not message the solvers again.\n\n"
    "STEP 5 — LATER, when a solver sends you `CONCLUDED <code>`, you wake up: send "
    "`notes` `<type>read</type><path>answer.txt</path>`, check that the first line "
    "is the same 3-digit code the message names, and tell me `VALIDATED <code>` (or "
    "`MISMATCH` with what you saw).\n\n"
    "STEP 6 — CLEAN UP THE FLEET: the team's work is over. Send `kernel` ONE batch of "
    f"three `delete_agent` calls, `<id>{SOLVERS[0]}</id>`, `<id>{SOLVERS[1]}</id>`, "
    f"`<id>{SOLVERS[2]}</id>` (they are your children, so this needs no extra grant), "
    "confirm to me that the three are gone, and call `done`. Nothing else.\n\n"
    "THE SOLVERS' INSTRUCTIONS (send each block verbatim as that solver's `<text>`):\n\n"
    + "\n\n".join(f"=== {SOLVERS[k]} ===\n{solver_instructions(k)}" for k in range(N))
)


async def fixture(root, *, interpreter=None):
    _ = interpreter
    store = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
                        "id": "kernel_store",
                        # `notes` is the one that actually reads/writes here (the solvers
                        # and the creator go THROUGH it) — measured 2026-09-10: without
                        # it, every write failed and the creator had to open the door.
                        "callable_by": {CREATOR: "*", "brain": "*", "notes": "*", "rest": "*", "web": "*"},
                        "ingress_rule": "allow_all_ingress",
                        "root": ".fantastic",
                    },
                }
            ]
        )
    )[0]["result"]
    batch = [
        {
            "target": root.id,
            "payload": {
                "type": "create_agent",
                "parent_id": root.id,
                "handler_module": "text_file.tools",
                "id": "notes",
                "target_id": store["id"],
                "can_call": {store["id"]: "*"},
                "callable_by": {CREATOR: "*", "rest": "*", "web": "*"},
                "file_bridge_id": store["id"],
                "description": "Shared text documents. The team's answer goes to `answer.txt`.",
            },
        },
        {
            "target": root.id,
            "payload": {
                "type": "create_agent",
                "parent_id": root.id,
                "handler_module": "user_probe.tools",
                "id": "user",
                "answer": "do your best judgement",
                "description": "The human this kernel belongs to.",
                "can_call": {CREATOR: "*"},
                "callable_by": {CREATOR: "*", "rest": "*", "web": "*"},
            },
        },
    ]
    for k, oid in enumerate(ORACLES):
        batch.append(
            {
                "target": root.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": root.id,
                    "handler_module": "oracle_probe.tools",
                    "id": oid,
                    "lines": S.LEDGERS[k],
                    "description": f"Oracle {k + 1}: the {LEDGER_NAMES[k]} ledger; answers `ask`. Knows only its own ledger.",
                    "callable_by": {CREATOR: "*", "rest": "*", "web": "*"},
                },
            }
        )
    for out in await root.send(batch):
        assert not (out.get("result") or {}).get("error"), out
    return store


def _rows(root, aid):
    return [r for r in root.ctx.rows_for(aid) if r.get("kind") not in ("call", "task", "body", "view")]


def _assistant_text(root, aid) -> str:
    return " ".join(str(r.get("content") or "") for r in _rows(root, aid) if r.get("role") == "assistant")


async def score(root, store_id, total, new_total, transcript):
    _ = (store_id, total, new_total, transcript)
    print("\n" + "=" * (P.W + 4))
    print(f"  puzzle seed={S.PUZZLE_SEED} code={S.CODE}")
    listing = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]
    records = {a["id"]: a for a in (listing.get("agents") or [])}
    ai_module = records.get("brain", {}).get("handler_module")
    solvers = [a["id"] for a in records.values() if a["id"] not in MINE and a.get("handler_module") == ai_module]

    # ── 1. STRUCTURE: three solvers, the creator's children, one oracle each, pairs both ways ──
    def cc(aid):
        return records.get(aid, {}).get("can_call") or {}

    def cb(aid):
        return records.get(aid, {}).get("callable_by") or {}

    per = {s: {o for o in ORACLES if o in cc(s)} for s in solvers}
    oracle_ok = len(solvers) == N and all(len(v) == 1 for v in per.values()) and len(set().union(*per.values())) == N if per else False
    doors_ok = all(any(s in cb(o) for s in solvers) for o in ORACLES)
    pairs_ok = all(t in cc(s) and s in cb(t) for s in solvers for t in solvers if s != t) if solvers else False
    children_ok = all(records.get(s, {}).get("parent_id") == CREATOR for s in solvers)
    c1 = oracle_ok and doors_ok and pairs_ok and children_ok
    print(
        f"    1. team built and wired             {'OK' if c1 else 'NO'} solvers={per} "
        f"oracle_doors={doors_ok} pairs_both_ways={pairs_ok} children={children_ok}"
    )

    # ── 2. ONE-BATCH KICKOFF with replies discarded, then `done` ──
    creator = _rows(root, CREATOR)
    kick_turn = None
    for r in creator:
        if r.get("role") != "assistant":
            continue
        body = str(r.get("content") or "")
        n = sum(1 for s in solvers if re.search(rf"<target_id>\s*{s}\s*</target_id>", body))
        if n == N:
            kick_turn = body
            break
    discarded = bool(kick_turn) and len(re.findall(r"<reply_to>\s*null\s*</reply_to>", kick_turn)) >= N
    c2 = kick_turn is not None and discarded
    print(f"    2. one-batch kickoff, replies null  {'OK' if c2 else 'NO'} kickoff_turn={'yes' if kick_turn else 'no'} reply_to_null={discarded}")

    # ── 3. THE SWARM CONCLUDES (E36's measure) ──
    print(f"       … the team works unattended, up to {UNATTENDED:.0f}s")
    t0 = time.monotonic()
    answer = None
    while time.monotonic() - t0 < UNATTENDED:
        p = Path(".fantastic/answer.txt")
        if p.exists():
            first = (p.read_text(errors="ignore").strip().splitlines() or [""])[0].strip()
            if re.fullmatch(r"\d{3}", first):
                answer = first
                break
        await asyncio.sleep(5.0)
    c3 = answer == S.CODE
    print(f"    3. the code is right                {'OK' if c3 else 'NO'} answer={answer!r} want={S.CODE} after={time.monotonic() - t0:.0f}s")

    # ── 4. THE CREATOR IS WOKEN, VALIDATES, DELETES THE FLEET, STOPS ──
    woke = validated = stopped = deleted = False
    if c3:
        end = time.monotonic() + 240.0
        while time.monotonic() < end and not stopped:
            rows = _rows(root, CREATOR)
            # the wake: a root user row from a solver carrying CONCLUDED
            idx = next((i for i, r in enumerate(rows) if r.get("role") == "user" and "CONCLUDED" in str(r.get("content") or "")), None)
            if idx is not None:
                woke = True
                after = " ".join(str(r.get("content") or "") for r in rows[idx:] if r.get("role") == "assistant")
                validated = bool(re.search(r"<target_id>\s*notes\s*</target_id>", after)) and bool(re.search(r"VALIDATED\s+" + S.CODE, after))
                live = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]
                ids = {a["id"] for a in (live.get("agents") or [])}
                deleted = validated and not any(s in ids for s in solvers)
                stopped = 'name="done"' in after and deleted
            if not stopped:
                await asyncio.sleep(5.0)
    c4 = woke and validated and deleted and stopped
    print(
        f"    4. woken → validated → fleet deleted → done {'OK' if c4 else 'NO'} "
        f"woken={woke} validated={validated} deleted={deleted} done={stopped}"
    )

    # ── THE EVIDENCE, always: the records the creator wrote, the exchange, the errors ──
    print("\n  ── solver records (as the creator wrote them) ──")
    for s_ in solvers:
        rec = {k: v for k, v in records.get(s_, {}).items() if k not in ("can_call", "callable_by", "description")}
        print(f"  {s_}: {rec}")
    print("\n  ── the exchange (peer messages, in send order) ──")
    sent: list[tuple[float, str, str, str]] = []
    for s_ in solvers:
        for r in _rows(root, s_):
            if r.get("role") != "assistant":
                continue
            for m in re.finditer(r"<call name=\"send\">(.*?)</call>", str(r.get("content") or ""), re.DOTALL):
                blk = m.group(1)
                tgt = re.search(r"<target_id>\s*(\S+?)\s*</target_id>", blk)
                if tgt is None:  # a broadcast: `<to><item>…</item>…</to>`
                    to = re.search(r"<to>(.*?)</to>", blk, re.DOTALL)
                    names = re.findall(r"<item>\s*(\S+?)\s*</item>", to.group(1)) if to else []
                    tgt = re.match(r"(.*)", "+".join(names)) if names else None
                txt = re.search(r"<text>(.*?)</text>", blk, re.DOTALL)
                if tgt and txt and (tgt.group(1) in (*solvers, CREATOR) or "+" in tgt.group(1)):
                    sent.append((float(r.get("ts") or 0), s_, tgt.group(1), " ".join(txt.group(1).split())))
    for ts, frm, to, txt in sorted(sent):
        print(f"  {ts - (sent[0][0] if sent else ts):6.0f}s {frm} → {to}: {txt[:150]}")
    print("\n  ── errors and refusals the solvers received ──")
    for s_ in solvers:
        errs = [str(r.get("content") or "") for r in _rows(root, s_) if r.get("role") == "user" and ("<error>" in str(r.get("content") or "") or "NOT dispatched" in str(r.get("content") or ""))]
        kinds = {}
        for e in errs:
            m = re.search(r"<error>(.*?)</error>|NOT dispatched: ([^.]*)", e, re.DOTALL)
            key = " ".join(((m.group(1) or m.group(2)) if m else e)[:90].split())
            kinds[key] = kinds.get(key, 0) + 1
        print(f"  {s_}: {len(errs)} — " + "; ".join(f"{k} ×{n}" for k, n in sorted(kinds.items(), key=lambda t: -t[1])[:4]))
        last = [r for r in _rows(root, s_) if r.get("role") == "assistant"]
        if last:
            print(f"     last turn: {' '.join(str(last[-1].get('content') or '').split())[:200]}")
    turns = {s: sum(1 for r in _rows(root, s) if r.get("role") == "assistant") for s in solvers}
    ct = sum(1 for r in _rows(root, CREATOR) if r.get("role") == "assistant")
    print(f"\n  observed — creator turns        : {ct}")
    print(f"  observed — solver turns         : {turns}  (storm: {sum(turns.values())} turns)")
    ok = c1 and c2 and c3 and c4
    print(f"\n  RESULT: {'PASS — built, started, slept, woken, validated, cleaned up, stopped' if ok else 'FAIL'}")
    verdict(ok)
    return ok


async def main() -> None:
    ok = await P.run(
        task=TASK,
        banner="MULTISUBAI (build) — the creator builds the team, sleeps, is woken, validates",
        fixture=fixture,
        scorer=score,
        mine=MINE,
        extra_reach={o: "*" for o in ORACLES} | {"notes": "*"},
        prefix="multisubai_build_",
    )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    asyncio.run(main())
