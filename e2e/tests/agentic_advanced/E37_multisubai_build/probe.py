"""LIVE TEST — MULTISUBAI (build): ONE creator builds the three-solver team, wires it,
kicks it off, walks away — and is WOKEN by the team when it concludes, validates
the answer, DELETES the fleet, and stops.

E36 measures the swarm with a harness-built team. This one measures the CREATOR: the
operator leaves three empty solver shells under it (a line naming a missing id is
refused, and a creator cannot hand out lines on agents only its `create` line
reaches — so the shells exist first and the creator holds a `send` line on each);
the creator makes each solver's provider under it, fills each solver's record with
exactly one oracle line and a line on each peer, points them at `notes`, starts all
three in ONE batch with replies DISCARDED, calls `done`. Then, when the solvers have
agreed and written the code, the writer must message the creator; the creator's
pass wakes, reads `answer.txt` on `notes`, confirms the code the solvers report is
the code on disk, tells the human, deletes its three solvers (its `create` line
covers them) and ends with `done`. That last leg — a sleeping mind woken by
its own creation, validating, cleaning up and stopping — is the point of the test.
Acceptance: the reported answer AND the deletion of the sub-agents.

A creator left to work it out is not stuck but SLOW: passes of reflecting, a probe
program, one `update_agent` per line. So the task
here is as detailed as a spec — the record fields to copy, the exact `can` lines
(and that `update_agent` REPLACES a named field whole), the solver instructions
verbatim, the batch kickoff with `<reply_to>null</reply_to>`, and `done`. What is
left to the creator is doing it, correctly, in few passes.

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
from helpers.kernel import home
from helpers import add_lines, creds, lines, memory, probe_agent, relay, verdict  # noqa: E402
from helpers.ai import CHAT_MODULE, KNOBS, split_record  # noqa: E402
from helpers.lines import is_unknown  # noqa: E402
from helpers.ai import IDLE_END, QuietWatch  # noqa: E402
from tests.agentic_advanced.E30_subai import probe as P  # noqa: E402
from tests.agentic_advanced.E36_multisubai import probe as S  # noqa: E402

CREATOR = "ai"
ORACLES, SOLVERS, LEDGER_NAMES, N = S.ORACLES, S.SOLVERS, S.LEDGER_NAMES, S.N
# The ids the FIXTURE builds (E30's set, plus this probe's own): what the model did
# not make. The solver shells are the operator's too, but they are the TEAM the
# creator fills, so they are scored as its work.
MINE = {*P.MINE, "kernel_store", "notes", "user", CREATOR, *ORACLES}
#: the oracles' records before the run: no agent writes on a callee (`doors_ok`)
ORACLE_BEFORE: dict[str, dict] = {}
UNATTENDED = S.UNATTENDED


def solver_instructions(k: int) -> str:
    """E36's kickoff text plus the closing step: tell the creator."""
    base = S.kickoff(k)
    return base.replace(
        "End every pass with `done` once you have nothing more to send;",
        f"When the code has been written to `answer.txt`, the writer sends `{CREATOR}` "
        "(your creator) one message: `CONCLUDED <code>` with one line per digit saying "
        "who owned it and how it was derived; the other two send nothing further. End "
        "every pass with `done` once you have nothing more to send;",
    )


#: The plug's record split as `brain` and `brain_prov` were made from it (`split_record`,
#: #632): the provider's fields, and the chat's (its request knobs among them).
_PROV_FIELDS, _CHAT_FIELDS = split_record(P.BK.record())
#: What the creator copies from `brain`'s record into each solver (a CHAT: the endpoint
#: and key are its provider's, named by `provider_id`): the model (the
#: chat's: it names it on each `complete`; the provider uses the call's model or its
#: `default_model` setting, never its own record's), the provider, the conversation
#: store, the window and the request knobs the plug sets (`max_tokens` bounds a pass
#: inside the server's window). A backend whose record names no model copies none.
#: NOT `handler_module`: the shells are created as chats (`CHAT_MODULE`) and the kernel
#: never lets any caller change an agent's handler (structure fields are not updatable).
COPY = [*(["model"] if "model" in _CHAT_FIELDS else []),
        "provider_id", "memory_store_id", "context_window",
        *(k for k in KNOBS if k in _CHAT_FIELDS)]
#: …and into each solver's PROVIDER (copied from `brain_prov`): the plug's provider
#: fields only — a request knob is the chat's, and a provider refuses it (#632)
PROV_COPY = [*_PROV_FIELDS, "memory_store_id"]
#: THE KEY LINE THE CHAIN NEEDS (as E31): `<solver_k>_prov` reads its key through the
#: forced `creds: get`, and every ancestor whose tree does not hold the key store caps
#: that call — the creator and the solver each hold the same line. The operator writes
#: the creator's beside its `create` line; the creator hands each solver its own.
KEY_LINE = {creds.CREDS: creds.GET}

TASK = (
    "You are the CREATOR here — the mind building the structure. I am going to walk "
    "away, but I am an AGENT here too: if you genuinely cannot proceed without a "
    "decision that is mine, reflect the tree, find me, and send me an `inquiry`.\n\n"
    f"This kernel holds THREE oracles, `{ORACLES[0]}` (the PEOPLE ledger), "
    f"`{ORACLES[1]}` (the ROOMS ledger), `{ORACLES[2]}` (the OWNERSHIP ledger) — each "
    "answers `ask` with a `question` and knows ONLY its own ledger. Together they hide "
    "a 3-digit SAFE CODE. There is also `notes` (a file agent; verbs write/"
    "read, args `path` and `bytes`) and `brain`, an AI agent whose record is your "
    "template (its fields and `brain_prov`'s are spelled at the end). Do NOT solve "
    "the puzzle yourself and do NOT relay facts between "
    "solvers. Build the team, start it, walk away, and come back only when it calls "
    "you.\n\n"
    "HOW REACH WORKS: an agent reaches only its own tree (the agents below it); every "
    "other call needs a LINE in the CALLER's own `can` field, "
    "`{<target id>: {\"call\": [<verbs>]}}` (`[\"*\"]` for every verb). You may hand "
    "on only lines you hold yourself.\n\n"
    "DO EXACTLY THIS, in as few passes as you can — every step is spelled out so you "
    "need no exploration:\n\n"
    f"STEP 1 — FILL THE THREE SOLVERS. They already exist as YOUR OWN CHILDREN, empty "
    f"chats with ids `{SOLVERS[0]}`, `{SOLVERS[1]}`, `{SOLVERS[2]}`. Each solver is a "
    "PAIR: a provider `<solver_k>_prov` and the chat `<solver_k>` that completes "
    "through it. First send ONE batch of three `create_agent` calls to `kernel`, one "
    "provider per solver, each with `parent_id` = `<solver_k>` (UNDER its solver, so "
    "the solver reaches it with no line), these fields copied from the `brain_prov` "
    "fields below: "
    + ", ".join(f"`{f}`" for f in PROV_COPY)
    + ", and `can` exactly `{\"creds\": {\"call\": [\"get\"]}}` (the key store line "
    "your right to create it forces). Then send ONE batch of three `update_agent` "
    "calls to `kernel`, one per solver (`id` = `<solver_k>`), with these fields copied "
    "from the `brain` fields below, `provider_id` set to `<solver_k>_prov`: "
    + ", ".join(f"`{f}`" for f in COPY)
    + ". Set `description` to what the solver is for "
    "(solver k holds the ledger of `oracle_k`). Give each solver this `can`:\n"
    "  `{\"<oracle_k>\": {\"call\": [\"*\"]}, \"notes\": {\"call\": [\"*\"]}, "
    "\"memory\": {\"call\": [\"*\"]}, \"creds\": {\"call\": [\"get\"]}, "
    "\"<solver_j>\": {\"call\": [\"send\"]}, \"<solver_i>\": {\"call\": [\"send\"]}}` — "
    "its own oracle only, `notes`, the store its conversation lives in (`memory`), the "
    "key store line (its provider reads its key through its own `creds` line, and the "
    "solver is that provider's ancestor, so the call passes only when the solver holds "
    "the line too: hand it the one you hold), and `send` on EACH of the two other "
    "solvers.\n"
    "That is: solver k reaches its own oracle and both peers; its provider is below "
    "it. You are their parent, so you reach them with no line.\n\n"
    "STEP 2 — KICK OFF ALL THREE AT ONCE: one batch of three `send` calls, one per "
    "solver, each with `<type>send</type>`, `<client_id>` = the solver's id, "
    "`<reply_to>null</reply_to>` (their first passes are NOT yours to read), and "
    "`<text>` = that solver's instructions, VERBATIM from the three blocks below.\n\n"
    "STEP 3 — tell me in one paragraph what you built, then call `done`. Do not poll, "
    "do not read `answer.txt` yet, do not message the solvers again.\n\n"
    "STEP 4 — LATER, when a solver sends you `CONCLUDED <code>`, you wake up: send "
    "`notes` `<type>read</type><path>answer.txt</path>`, check that the first line "
    "is the same 3-digit code the message names, and tell me `VALIDATED <code>` (or "
    "`MISMATCH` with what you saw).\n\n"
    "STEP 5 — CLEAN UP THE FLEET: the team's work is over. Send `kernel` ONE batch of "
    f"six `delete_agent` calls, the three solvers `{SOLVERS[0]}`, `{SOLVERS[1]}`, "
    f"`{SOLVERS[2]}` and their providers `<solver_k>_prov` (they are your children, so "
    "this needs no extra line), "
    "confirm to me that the six are gone, and call `done`. Nothing else.\n\n"
    "THE SOLVERS' INSTRUCTIONS (send each block verbatim as that solver's `<text>`):\n\n"
    + "\n\n".join(f"=== {SOLVERS[k]} ===\n{solver_instructions(k)}" for k in range(N))
    + "\n\n{template}"
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
                        "parent_id": home(root),
                        "handler_module": "file_bridge.tools", "allow": ["."],
                        "id": "kernel_store",
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
                "parent_id": home(root),
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": "notes",
                # a DATA store — the records under `.fantastic` stay the loader's
                "root": ".fantastic/data",
                "description": "Shared text documents. The team's answer goes to `answer.txt`.",
            },
        },
        {
            "target": root.id,
            "payload": {
                "type": "create_agent",
                "parent_id": home(root),
                "handler_module": "user_probe.tools",
                "id": "user",
                "answer": "do your best judgement",
                "description": "The human this kernel belongs to.",
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
                    "lines": S.LEDGERS[k],
                    "description": f"Oracle {k + 1}: the {LEDGER_NAMES[k]} ledger; answers `ask`. Knows only its own ledger.",
                },
            }
        )
    for out in await root.send(batch):
        assert not (out.get("result") or {}).get("error"), out
    return store


def creator_create(aid: str) -> dict:
    """The creator's `create` line on ITSELF: the solvers and their providers, of the
    chat and provider bundles; a provider holds the key line only as forced here."""
    prov = P.BK.record().get("handler_module")
    return {aid: lines.create(
        who=(*SOLVERS, *[f"{sid}_prov" for sid in SOLVERS]),
        what={CHAT_MODULE: {"*": "*"}, prov: {"*": "*", **creds.creator_line(prov)}})}


async def wire_builder(root, aid: str) -> None:
    """The operator's half, once the creator exists (O11): its `create` line, the three
    empty solver shells under it, and the creator's literal `send` line on each (a
    `create` line with a narrower `who` does not let it hand out lines on the agents
    it reaches; a literal line does). Then, before the model, the writer-subset wall:
    a hand holding the creator's lines may not put a line it lacks into a solver."""
    await add_lines(root, aid, {**creator_create(aid), **KEY_LINE})
    for k, sid in enumerate(SOLVERS):
        r = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": aid, "id": sid,
            "handler_module": CHAT_MODULE,
            "description": f"Solver {k + 1} of three (to be filled by its creator)."}}]))[0]["result"]
        assert not (isinstance(r, dict) and r.get("error")), f"create shell {sid}: {r}"
    await add_lines(root, aid, {sid: lines.call("send") for sid in SOLVERS})
    for oid in ORACLES:
        ORACLE_BEFORE[oid] = await asyncio.to_thread(root.record, oid)
    # A hand with the creator's exact lines (and so the same `create` line) fills a
    # shell with every verb on the key store, where it holds only `get`: refused, and
    # the shell is unchanged.
    can = (await asyncio.to_thread(root.record, aid)).get("can") or {}
    await probe_agent(root, "creator_hand", can=can)
    r = await relay(root, "creator_hand", "kernel", {
        "type": "update_agent", "id": SOLVERS[0], "can": {creds.CREDS: lines.ALL}})
    assert ((r.get("reason") == "not_allowed" and r.get("arg") == "can")
            or is_unknown(r, creds.CREDS)), f"a line the writer lacks was handed out: {r}"
    shell = await asyncio.to_thread(root.record, SOLVERS[0])
    assert not shell.get("can"), f"the refused fill changed {SOLVERS[0]}: {shell.get('can')}"
    gone = (await root.send([{"target": root.id, "payload": {"type": "delete_agent", "id": "creator_hand"}}]))[0]["result"]
    assert not gone.get("error"), f"delete creator_hand: {gone}"


async def _rows(root, aid):
    return await memory.rows_of(root, aid, conversation_only=True)


async def _assistant_text(root, aid) -> str:
    return " ".join(str(r.get("content") or "") for r in await _rows(root, aid) if r.get("role") == "assistant")


async def score(root, store_id, total, new_total, transcript):
    _ = (store_id, total, new_total, transcript)
    print("\n" + "=" * (P.W + 4))
    print(f"  puzzle seed={S.PUZZLE_SEED} code={S.CODE}")
    records = await P.records_of(root)
    ai_module = P.bundle(records.get("brain", {}))
    solvers = [aid for aid, a in records.items() if aid not in MINE and P.bundle(a) == ai_module]

    # ── 1. STRUCTURE: three solvers, the creator's children, one oracle each, pairs both ways ──
    def can(aid):
        return records.get(aid, {}).get("can") or {}

    per = {s: {o for o in ORACLES if o in can(s)} for s in solvers}
    oracle_ok = len(solvers) == N and all(len(v) == 1 for v in per.values()) and len(set().union(*per.values())) == N if per else False
    # no agent writes on a callee: the oracles' records are as the fixture made them
    doors_ok = bool(ORACLE_BEFORE) and all(records.get(o) == ORACLE_BEFORE.get(o) for o in ORACLES)
    pairs_ok = all(t in can(s) for s in solvers for t in solvers if s != t) if solvers else False
    children_ok = all(records.get(s, {}).get("parent_id") == CREATOR for s in solvers)
    c1 = oracle_ok and doors_ok and pairs_ok and children_ok
    print(
        f"    1. team built and wired             {'OK' if c1 else 'NO'} solvers={per} "
        f"oracles_untouched={doors_ok} pairs_both_ways={pairs_ok} children={children_ok}"
    )

    # ── 2. ONE-BATCH KICKOFF with replies discarded, then `done` ──
    creator = (await _rows(root, CREATOR))
    kick_pass = None
    for r in creator:
        if r.get("role") != "assistant":
            continue
        body = str(r.get("content") or "")
        n = sum(1 for s in solvers if re.search(rf"<target_id>\s*{s}\s*</target_id>", body))
        if n == N:
            kick_pass = body
            break
    discarded = bool(kick_pass) and len(re.findall(r"<reply_to>\s*null\s*</reply_to>", kick_pass)) >= N
    c2 = kick_pass is not None and discarded
    print(f"    2. one-batch kickoff, replies null  {'OK' if c2 else 'NO'} kickoff_pass={'yes' if kick_pass else 'no'} reply_to_null={discarded}")

    # ── 3. THE SWARM CONCLUDES (E36's measure) ──
    print(f"       … the team works unattended, up to {UNATTENDED:.0f}s")
    t0 = time.monotonic()
    answer = None
    watch = QuietWatch(root)
    while time.monotonic() - t0 < UNATTENDED:
        p = Path(".fantastic/data/answer.txt")
        if p.exists():
            first = (p.read_text(errors="ignore").strip().splitlines() or [""])[0].strip()
            if re.fullmatch(r"\d{3}", first):
                answer = first
                break
        if await watch.quiet():
            print(f"       … nothing in flight for {IDLE_END:.0f}s — the team stopped")
            break
        await asyncio.sleep(5.0)
    c3 = answer == S.CODE
    print(f"    3. the code is right                {'OK' if c3 else 'NO'} answer={answer!r} want={S.CODE} after={time.monotonic() - t0:.0f}s")

    # ── 4. THE CREATOR IS WOKEN, VALIDATES, DELETES THE FLEET, STOPS ──
    woke = validated = stopped = deleted = False
    if c3:
        # UP TO THE UNATTENDED LIMIT, ENDED BY THE IDLE STOP: a fixed cut can stop the
        # creator mid-work on a one-request-at-a-time GPU (awaiting its own calls'
        # replies); a quiet kernel ends the wait instead.
        end = time.monotonic() + UNATTENDED
        watch = QuietWatch(root)
        while time.monotonic() < end and not stopped:
            rows = (await _rows(root, CREATOR))
            # the wake: a root user row from a solver carrying CONCLUDED
            idx = next((i for i, r in enumerate(rows) if r.get("role") == "user" and "CONCLUDED" in str(r.get("content") or "")), None)
            if idx is not None:
                woke = True
                after = " ".join(str(r.get("content") or "") for r in rows[idx:] if r.get("role") == "assistant")
                validated = bool(re.search(r"<target_id>\s*notes\s*</target_id>", after)) and bool(re.search(r"VALIDATED\s+" + S.CODE, after))
                ids = set(await asyncio.to_thread(root.agent_ids))
                deleted = validated and not any(s in ids for s in solvers)
                stopped = 'name="done"' in after and deleted
            if not stopped and await watch.quiet():
                print(f"       … nothing in flight for {IDLE_END:.0f}s — the creator stopped")
                break
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
        rec = {k: v for k, v in records.get(s_, {}).items() if k not in ("can", "description")}
        print(f"  {s_}: {rec}")
    print("\n  ── the exchange (peer messages, in send order) ──")
    sent: list[tuple[float, str, str, str]] = []
    for s_ in solvers:
        for r in (await _rows(root, s_)):
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
        errs = [str(r.get("content") or "") for r in (await _rows(root, s_)) if r.get("role") == "user" and ("<error>" in str(r.get("content") or "") or "NOT dispatched" in str(r.get("content") or ""))]
        kinds = {}
        for e in errs:
            m = re.search(r"<error>(.*?)</error>|NOT dispatched: ([^.]*)", e, re.DOTALL)
            key = " ".join(((m.group(1) or m.group(2)) if m else e)[:90].split())
            kinds[key] = kinds.get(key, 0) + 1
        print(f"  {s_}: {len(errs)} — " + "; ".join(f"{k} ×{n}" for k, n in sorted(kinds.items(), key=lambda t: -t[1])[:4]))
        last = [r for r in (await _rows(root, s_)) if r.get("role") == "assistant"]
        if last:
            print(f"     last pass: {' '.join(str(last[-1].get('content') or '').split())[:200]}")
    passes = {s: sum(1 for r in (await _rows(root, s)) if r.get("role") == "assistant") for s in solvers}
    ct = sum(1 for r in (await _rows(root, CREATOR)) if r.get("role") == "assistant")
    print(f"\n  observed — creator passes        : {ct}")
    print(f"  observed — solver passes         : {passes}  (storm: {sum(passes.values())} passes)")
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
        extra_reach={o: lines.ALL for o in ORACLES} | {"notes": lines.ALL},
        prefix="multisubai_build_",
        sub_ais=list(SOLVERS),
        wire_builder=wire_builder,
        handed=KEY_LINE,
    )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    asyncio.run(main())
