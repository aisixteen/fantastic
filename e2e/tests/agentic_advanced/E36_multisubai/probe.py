"""LIVE TEST — MULTISUBAI (solve): THREE sub-agents, each with its OWN oracle, must
find a 3-digit safe code that exists only when every pair has talked.

The team is built and kicked off BY THE HARNESS (user, 2026-09-10: "create all by
yourself and program agent kickoff in test harness") — so what is measured is the
SWARM, not a creator's wiring. A creator-built variant is a separate test.

The puzzle (the ring safe): oracle 1 = who works where, oracle 2 = what each room
holds and its desks, oracle 3 = what each person owns. Each oracle ends with ONE
clue: digit 1 needs oracle 1's room and oracle 2's count; digit 2 needs oracle 1's
occupant and oracle 3's count; digit 3 needs oracle 1's room and oracle 2's desks. So the
pairs (1,2), (2,1), (2,3), (3,1) all have to talk — every pair of the three — and
only the three digits together are the code, which all three must AGREE on before
one of them writes it.

No shared threads exist in this kernel: each solver has ONE interleaved
conversation and the only shared medium is a file. Every message is a turn for the
receiver; the storm is the point and is reported (turns per solver, messages).

An AI agent has no standing instructions on its record (a caller's `system_prompt`
REPLACES the whole default prompt, grammar included), so the solver's role rides in
the KICKOFF message — the first user turn of its conversation, which stays in its
context for every turn after.
"""

from __future__ import annotations

import asyncio
import os
import random
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
from backends import backend as _resolve_backend  # noqa: E402
from helpers import creds, memory
from helpers import verdict  # noqa: E402
from helpers.kernel import open_kernel  # noqa: E402, home

BK = _resolve_backend()
BACKEND, MODEL = BK.name, BK.model
CTX = int(os.environ.get("CTX", "65536"))
UNATTENDED = float(os.environ.get("UNATTENDED", "600"))
W = 78

N = 3
ORACLES = [f"oracle_{k}" for k in range(1, N + 1)]
SOLVERS = [f"solver_{k}" for k in range(1, N + 1)]
LEDGER_NAMES = ["PEOPLE", "ROOMS", "OWNERSHIP"]

# ── the puzzle ─────────────────────────────────────────────────────────────────
_PEOPLE = ["Dana", "Priya", "Marek", "Yusuf", "Ilse", "Tomas", "Nadia", "Oren", "Lena", "Kofi"]
_ROOMS = ["the Vault", "Bay 7", "the Loft", "Room 12", "the Annex", "Dock C", "the Gallery", "Lab 3"]
_ITEMS = ["crates", "lamps", "keys", "chairs", "boxes", "cables", "flags", "drums"]


def make_puzzle(rng: random.Random) -> tuple[str, list[list[str]]]:
    """(code, ledgers). Oracle 1: who works where. Oracle 2: what each room holds and
    how many desks it has. Oracle 3: what each person owns. Each ledger ends with ONE
    clue; every clue needs a fact from another oracle, and all three pairs get used:
    digit 1 = 1→2, digit 2 = 2→1→3, digit 3 = 3→1→2."""
    people = rng.sample(_PEOPLE, 6)
    rooms = rng.sample(_ROOMS, 6)
    items = rng.sample(_ITEMS, 3)
    digits = [rng.randint(1, 9) for _ in range(N)]
    who_where = {p: r for p, r in zip(people, rooms)}
    room_count = {r: rng.randint(1, 9) for r in rooms}
    owned = {p: rng.randint(1, 9) for p in people}
    desks = {r: rng.randint(1, 9) for r in rooms}
    # digit 1 = items in p1's room (oracle 2's count); digit 2 = items p2 owns
    # (oracle 3's count, p2 found through oracle 1); digit 3 = desks in p3's room (oracle 1)
    p1, p2, p3 = people[0], people[1], people[2]
    room_count[who_where[p1]] = digits[0]
    owned[p2] = digits[1]
    desks[who_where[p3]] = digits[2]
    # EVERY FACT ABOUT A ROOM LIVES IN THE ROOMS LEDGER. Desks used to sit in the
    # PEOPLE ledger "for a one-hop clue"; measured 2026-09-10: solver_2 (rooms) asked
    # its oracle 73 times for desks it never had while the others waited — the
    # ledger NAME is what the solvers reason from, so the split must honour it.
    ledger1 = [f"{p} works in {who_where[p]}." for p in people] + [
        f"{p} joined in {rng.randint(2010, 2024)}." for p in people
    ]
    ledger2 = [f"{r} holds {room_count[r]} {items[0]}." for r in rooms] + [
        f"{r} has {desks[r]} desks." for r in rooms
    ]
    ledger3 = [f"{p} owns {owned[p]} {items[1]}." for p in people] + [
        f"{p} drives a {rng.choice(['red', 'blue', 'grey', 'green'])} car." for p in people
    ]
    clue1 = (
        f"CLUE for digit 1 of the safe code: it equals the number of {items[0]} held in "
        f"the room where {p1} works. This ledger knows where people work; the ROOMS "
        f"ledger knows how many {items[0]} a room holds."
    )
    clue2 = (
        f"CLUE for digit 2 of the safe code: it equals the number of {items[1]} owned by "
        f"the person who works in {who_where[p2]}. This ledger knows rooms; the PEOPLE "
        f"ledger knows who works where, the OWNERSHIP ledger knows what people own."
    )
    clue3 = (
        f"CLUE for digit 3 of the safe code: it equals the number of desks in the room "
        f"where {p3} works. This ledger knows what people own; the PEOPLE ledger knows "
        f"where people work, the ROOMS ledger knows how many desks each room has."
    )
    rng.shuffle(ledger1)
    rng.shuffle(ledger2)
    rng.shuffle(ledger3)
    return "".join(str(d) for d in digits), [ledger1 + [clue1], ledger2 + [clue2], ledger3 + [clue3]]


PUZZLE_SEED = int(os.environ.get("PUZZLE_SEED") or time.time_ns() % 1_000_000)
CODE, LEDGERS = make_puzzle(random.Random(PUZZLE_SEED))


def kickoff(k: int) -> str:
    me, oracle = SOLVERS[k], ORACLES[k]
    peers = [(SOLVERS[j], LEDGER_NAMES[j]) for j in range(N) if j != k]
    return (
        f"You are `{me}`, one of three solvers. Together you must find a 3-digit SAFE "
        f"CODE. You hold the {LEDGER_NAMES[k]} ledger through your oracle `{oracle}`: "
        "send it `ask` with a precise `question` (it returns matching ledger lines; a "
        "vague question returns noise). Your oracle ends with a CLUE for ONE digit; "
        "the clue needs a fact only another solver's oracle knows. Your peers: "
        + ", ".join(f"`{p}` holds the {n} ledger" for p, n in peers)
        + ". You cannot reach their oracles — ASK THEM by sending a message, and "
        "ANSWER them when they ask you (ask your own oracle to answer). There are no "
        "shared channels: talk by sending messages directly.\n\n"
        "Protocol: (1) ask your oracle for the clue and any fact you need; (2) ask "
        "the peer whose ledger the clue names, and answer peers' questions from your "
        "oracle; (3) when you believe you know the whole 3-digit code, send BOTH "
        "peers `CANDIDATE <code>` with how you got each digit; (4) when you receive "
        "a CANDIDATE, check the digit you own and reply `AGREE <code>` or "
        "`DISAGREE <code> <reason>`; (5) only when all three agree does ONE of you "
        "(the one who broadcast the candidate) write the code, alone on the first "
        "line, to `answer.txt` on `notes` (a file agent: verb `write`, "
        "`path`, `bytes`). End every turn with `done` once you have nothing more to "
        "send; you will be woken by the next message. Begin: ask your oracle now."
    )


def _ai_record(agent_id: str, root, store, *, oracle: str, peers: list[str], description: str) -> dict:
    meta: dict = {
        "type": "create_agent",
        "parent_id": home(root),
        "id": agent_id,
        "description": description,
        "context_window": CTX,
        "can_call": {
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            oracle: "*",
            "notes": "*",
            store["id"]: "*",
            memory.MEMORY: "*",
            **{p: "*" for p in peers},
        },
        # ITS MEMORY: the conversation lives in the kernel's `memory` store.
        "memory_store_id": memory.MEMORY,
        "callable_by": {
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            "rest": "*",
            "web": "*",
            **{p: "*" for p in peers},
        },
    }
    # The plug describes ITSELF (e2e/backends) — see E30's `_ai_meta`. The probe's
    # own wiring (grants, description) wins over the plug's.
    for k, v in _resolve_backend(BACKEND).record().items():
        meta.setdefault(k, v)
    return meta


async def build(root):
    store = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": "file_bridge.tools", "allow": ["."],
                        "id": "kernel_store",
                        "callable_by": {**{s: "*" for s in SOLVERS}, "rest": "*", "web": "*"},
                        "ingress_rule": "allow_all_ingress",
                        "root": ".fantastic",
                    },
                }
            ]
        )
    )[0]["result"]
    assert not store.get("error"), store
    batch = [
        {
            "target": root.id,
            "payload": {
                "type": "create_agent",
                "parent_id": home(root),
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": "notes",
                # a DATA store — the records under `.fantastic` stay the loader's
                "root": ".fantastic/data",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {**{s: "*" for s in SOLVERS}, "rest": "*", "web": "*"},
                "description": "Shared text documents. The team's answer goes to `answer.txt`.",
            },
        },
    ]
    for k, oid in enumerate(ORACLES):
        batch.append(
            {
                "target": root.id,
                "payload": {
                    "type": "create_agent",
                    "parent_id": home(root),
                    "handler_module": "oracle_probe.tools",
                    "id": oid,
                    "lines": LEDGERS[k],
                    "description": f"Oracle {k + 1}: the {LEDGER_NAMES[k]} ledger; answers `ask`. Knows only its own ledger.",
                    "callable_by": {SOLVERS[k]: "*", "rest": "*", "web": "*"},
                },
            }
        )
    for k, sid in enumerate(SOLVERS):
        batch.append(
            {
                "target": root.id,
                "payload": _ai_record(
                    sid,
                    root,
                    store,
                    oracle=ORACLES[k],
                    peers=[s for s in SOLVERS if s != sid],
                    description=f"Solver {k + 1} of three: holds the {LEDGER_NAMES[k]} ledger through {ORACLES[k]}.",
                ),
            }
        )
    for out in await root.send(batch):
        assert not (out.get("result") or {}).get("error"), out
    for sid in SOLVERS:  # the store's half of each memory wire
        await memory.open_ingress(root, memory.MEMORY, sid)
        if _resolve_backend().key_ref:
            await creds.grant_reader(root, sid)
    return store


async def _rows(root, aid):
    return await memory.rows_of(root, aid, conversation_only=True)


async def _turns(root, aid) -> int:
    return sum(1 for r in await _rows(root, aid) if r.get("role") == "assistant")


async def main() -> None:
    print("=" * (W + 4))
    print(f"  MULTISUBAI (solve) · {BACKEND}/{MODEL} · seed={PUZZLE_SEED} code={CODE}")
    print("=" * (W + 4))
    root, core, close = await open_kernel("multisubai_", reach=["kernel_store", "notes", *ORACLES, *SOLVERS])
    try:
        await build(root)
        # THE KICKOFF — all three at once, from the operator, replies discarded: the
        # harness is not a participant and must not be woken by their first turns.
        t0 = time.monotonic()
        kicks = [
            asyncio.create_task(
                root.send(
                    [
                        {
                            "target": sid,
                            "payload": {"type": "send", "text": kickoff(k), "client_id": sid},
                            "timeout": UNATTENDED,
                        }
                    ]
                )
            )
            for k, sid in enumerate(SOLVERS)
        ]
        print(f"  kicked off {SOLVERS} at once; watching up to {UNATTENDED:.0f}s\n")
        answer = None
        seen_rows: set = set()
        while time.monotonic() - t0 < UNATTENDED:
            await asyncio.sleep(15.0)
            p = Path(".fantastic/data/answer.txt")
            if p.exists():
                first = (p.read_text(errors="ignore").strip().splitlines() or [""])[0].strip()
                if re.fullmatch(r"\d{3}", first):
                    answer = first
            turns = {s: (await _turns(root, s)) for s in SOLVERS}
            live = len(root.ctx.tasks) if hasattr(root.ctx, "tasks") else "?"
            print(f"  · {time.monotonic() - t0:5.0f}s turns={turns} tasks={live} answer={answer!r}", flush=True)
            # THE TAILS, live: every NEW row of every solver since the last tick —
            # a storm you can only count is a storm you cannot understand.
            for s in SOLVERS:
                for r in (await _rows(root, s)):
                    rid = r.get("id")
                    if rid in seen_rows:
                        continue
                    seen_rows.add(rid)
                    role = r.get("role") or r.get("kind")
                    body = " ".join(str(r.get("content") or "").split())
                    print(f"      {s} {role:9} {body[:220]}", flush=True)
            if answer is not None:
                # let the agreement turns land, then stop
                await asyncio.sleep(20.0)
                break
        for t in kicks:
            if not t.done():
                t.cancel()

        # ── score ──
        print("\n" + "=" * (W + 4))
        c1 = answer == CODE
        print(f"    1. the code is right                {'OK' if c1 else 'NO'} answer={answer!r} want={CODE}")
        asked = {}
        for k, s in enumerate(SOLVERS):
            text = " ".join(str(r.get("content") or "") for r in (await _rows(root, s)) if r.get("role") == "assistant")
            asked[s] = len(re.findall(rf"<target_id>\s*{ORACLES[k]}\s*</target_id>", text))
        c2 = all(v >= 1 for v in asked.values())
        print(f"    2. each solver asked its oracle     {'OK' if c2 else 'NO'} asks={asked}")
        talked = set()
        peer_msgs = 0
        for s in SOLVERS:
            text = " ".join(str(r.get("content") or "") for r in (await _rows(root, s)) if r.get("role") == "assistant")
            for t in SOLVERS:
                if t != s:
                    n = len(re.findall(rf"<target_id>\s*{t}\s*</target_id>", text))
                    peer_msgs += n
                    if n:
                        talked.add(frozenset((s, t)))
        pairs = {frozenset((a, b)) for i, a in enumerate(SOLVERS) for b in SOLVERS[i + 1 :]}
        c3 = pairs <= talked
        print(f"    3. every pair talked                {'OK' if c3 else 'NO'} pairs={len(talked)}/{len(pairs)} peer_messages={peer_msgs}")
        agrees = candidates = 0
        for s in SOLVERS:
            text = " ".join(str(r.get("content") or "") for r in (await _rows(root, s)) if r.get("role") == "assistant")
            agrees += len(re.findall(r"\bAGREE\b", text))
            candidates += len(re.findall(r"\bCANDIDATE\b", text))
        c4 = candidates >= 1 and agrees >= N - 1
        print(f"    4. candidate broadcast + agreement  {'OK' if c4 else 'NO'} candidates={candidates} agrees={agrees}")
        # THE EXCHANGE, readable: every message a solver sent to a peer, in order,
        # one line each — the storm's content, not just its size.
        print("\n  ── the exchange (peer messages, in send order) ──")
        sent: list[tuple[float, str, str, str]] = []
        for s in SOLVERS:
            for r in (await _rows(root, s)):
                if r.get("role") != "assistant":
                    continue
                body = str(r.get("content") or "")
                for m in re.finditer(
                    r"<call name=\"send\">(.*?)</call>", body, re.DOTALL
                ):
                    blk = m.group(1)
                    tgt = re.search(r"<target_id>\s*(\S+?)\s*</target_id>", blk)
                    if tgt is None:  # a broadcast: `<to><item>…</item>…</to>`
                        to = re.search(r"<to>(.*?)</to>", blk, re.DOTALL)
                        names = re.findall(r"<item>\s*(\S+?)\s*</item>", to.group(1)) if to else []
                        tgt = re.match(r"(.*)", "+".join(names)) if names else None
                    txt = re.search(r"<text>(.*?)</text>", blk, re.DOTALL)
                    if tgt and txt and (tgt.group(1) in SOLVERS or "+" in tgt.group(1)):
                        sent.append((float(r.get("ts") or 0), s, tgt.group(1), " ".join(txt.group(1).split())))
        for ts, frm, to, txt in sorted(sent):
            print(f"  {ts - (sent[0][0] if sent else ts):6.0f}s {frm} → {to}: {txt[:150]}")
        turns = {s: (await _turns(root, s)) for s in SOLVERS}
        received = {s: sum(1 for r in (await _rows(root, s)) if r.get("role") == "user") for s in SOLVERS}
        print(f"\n  observed — turns per solver     : {turns}  (storm: {sum(turns.values())} turns)")
        print(f"  observed — messages received    : {received}")
        print(f"  observed — wall                 : {time.monotonic() - t0:.0f}s")
        ok = c1 and c2 and c3 and c4
        print(f"\n  RESULT: {'PASS — three minds, three oracles, one code' if ok else 'FAIL'}")
        verdict(ok)
        sys.exit(0 if ok else 1)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
