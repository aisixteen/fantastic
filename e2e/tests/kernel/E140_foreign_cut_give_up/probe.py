"""kernel/foreign_cut_give_up — the call checker on the STREAM (#499) and its retries
inside the turn (#766), against a SCRIPTED model. NO MODEL.

A local OpenAI-compatible server plays the model (two: `m` for the AI, `mb` for the
BOSS it hands its text to); the kernel's own AI agents (on both runtimes:
`ai_chat.tools` chats naming a `vllm_backend` provider, #521) on the
PACKAGED rust host run the chains, driven through `web_rest`.

  A. the BOSS asks the AI (`inquiry`); the model writes a foreign call tag
     (`<invoke name="send">` followed by a loop of closers) on every attempt: each
     attempt fails at the tag and is re-asked inside the same turn (request k+1 is
     request 1's messages plus one `{attempt as written, report}` pair per failed
     attempt, the report `[malformed call — nothing ran]`); after exactly
     `1 + CALL_RETRIES` = 3 attempts (`constants.rs` `CALL_RETRIES = 2`) the harness
     stops the turn — no 4th request though the script holds more — and stores
     the turn's call `<call name="stop"><reason>…</reason></call>` with the reason
     `res/default/stop_reason.txt` filled with 3; no attempt and no loop is stored;
     the BOSS, the sender, is told that reason;
  B. nine turns in a row that each fail twice and then answer with a real call,
     then a turn that fails twice and ends with `done`: no count runs across turns
     — 30 requests, no stop; each turn's first request holds no failed attempt or
     report of an earlier turn; the first request shows A's stored stop call;
  C. a malformed answer after thinking: its re-ask holds the attempt, not the
     thought; the stored row is the accepted attempt's, with its own thinking
     (`has_thinking`); nothing of the failed attempt is stored.
"""

from __future__ import annotations

import json
import os
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER, ROOT, call
from helpers.verdict import verdict
from mocks.vllm import ThinkingVllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, BOSS = "ai", "boss"
PROVS = {AI: "ai_prov", BOSS: "boss_prov"}
MODEL = "mock-scripted"
TOKEN = "E140-" + secrets.token_hex(4)
FOREIGN = 'Sending it.\n<invoke name="send">\n' + '</invoke>\n' * 400
DONE = '<call name="done"></call>'
ATTEMPTS = 3  # 1 + CALL_RETRIES (src/lib/rust/crates/bundles/fantastic-ai-core/src/constants.rs)
# res/default/stop_reason.txt filled with ATTEMPTS
STOP_REASON = (f"{ATTEMPTS} attempts in a row wrote malformed calls or no call, so the "
               "harness stopped the turn.")
# the stored row is `render_call("stop", {reason})` = `<call name="stop">\n<reason>…</reason>\n</call>`
# (tool_parse/render.rs); the history digest's `head` flattens `\n` to a space and
# caps at 120 code points (verbs.rs `digest`), which this 111-char prefix fits
STOP_HEAD = f'<call name="stop"> <reason>{STOP_REASON}</reason>'
REPORT = "[malformed call — nothing ran]"


def last_user(req: dict) -> str:
    """The last user message of a chat request, as text."""
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = ThinkingVllmMock(model=MODEL).start()
    mb = ThinkingVllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-foreign-cut-"))
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def turns_at_least(mock, n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = mock.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return mock.turns()

    def idle(bound: float = 90) -> bool:
        """Poll (bounded) until neither agent has a task in flight (`ps`)."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if hk.quiet(AI, BOSS):
                return True
            time.sleep(0.05)
        return False

    def history() -> list[dict]:
        h = hk.send(AI, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    def pairs(t: dict) -> list[tuple]:
        return [(x.get("role"), x.get("content")) for x in (t["body"].get("messages") or [])]

    def convo(t: dict) -> str:
        """A request's messages without the system block, as text."""
        return json.dumps([x for x in (t["body"].get("messages") or [])
                           if x.get("role") != "system"], ensure_ascii=False)

    def reasked(turns: list[dict], first: int, n: int) -> bool:
        """Requests first+1 .. first+n-1 are re-asks of request `first`: its messages
        unchanged, then one {assistant: attempt, user: report} pair per failed attempt."""
        if len(turns) < first + n:
            return False
        base = pairs(turns[first])
        for k in range(1, n):
            got = pairs(turns[first + k])
            if len(got) != len(base) + 2 * k or got[:len(base)] != base:
                return False
            for j in range(k):
                a, r = got[len(base) + 2 * j], got[len(base) + 2 * j + 1]
                if a[0] != "assistant" or "<invoke" not in str(a[1]) \
                        or r[0] != "user" or not str(r[1]).startswith(REPORT):
                    return False
        return True

    ask = (f'<call name="send"><routing><target_id>{AI}</target_id></routing>'
           '<payload><type>inquiry</type><text>go A</text></payload></call>')
    reflect = (f'Looking around.\n<call name="send">\n<routing><target_id>{root}</target_id>'
               '</routing>\n<payload><type>reflect</type>\n<tree>none</tree></payload>\n</call>')
    try:
        # AI first; its line on BOSS is written once BOSS exists
        ai_can = {"memory": ALL, ROOT: call("reflect"), PROVS[AI]: PROVIDER}
        for aid, mock, can in (
            (AI, m, ai_can),
            (BOSS, mb, {"memory": ALL, AI: call("inquiry")}),
        ):
            fields = dict(endpoint=mock.url, model=MODEL,
                          context_window=262144, memory_store_id="memory")
            # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it:
            # a `send` to a provider is refused (#521)
            pv = PROVS[aid]
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=pv,
                            kv_slots=4, kv_overflow="evict",
                            can={"memory": ALL},
                            **fields)
            need(rec.get("id") == pv, f"create {pv}: {str(rec)[:200]}")
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                            provider_id=pv, model=MODEL, max_tokens=8192,
                            context_window=262144, memory_store_id="memory",
                            can={**can, pv: PROVIDER})
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")
        r = hk.send(root, {"type": "update_agent", "id": AI,
                           "can": {**ai_can, BOSS: call("send")}})
        need(r.get("updated") is True, f"AI's line on BOSS: {str(r)[:200]}")

        # ── A · three failed attempts: the harness stops the turn ─────────────
        m.reply(*[FOREIGN] * 12)
        mb.reply(ask, DONE, DONE)
        out = hk.send(BOSS, {"type": "send", "text": "go A"})
        need(not out.get("error"), f"the send is accepted: {str(out)[:200]}")
        turns_at_least(m, ATTEMPTS)
        need(idle(), "A: the chains settle")
        time.sleep(1)
        turns = m.turns()
        need(len(turns) == ATTEMPTS,
             f"A: exactly {ATTEMPTS} requests, none after the harness stop: {len(turns)}")
        need(reasked(turns, 0, ATTEMPTS),
             "A: requests 2-3 are re-asks of request 1 inside the turn, one pair per failed "
             "attempt, each report the malformed-call report")
        need(all(str(p[1]).count("</invoke>") < 400 for t in turns for p in pairs(t)
                 if p[0] == "assistant"),
             "A: no attempt is read past the delta that held the foreign tag")
        hist = history()
        rows = [r for r in hist if r.get("role") == "assistant"]
        # a history head is capped (120 chars): it is the stop call's opening
        head = str(rows[0].get("head")).rstrip("…") if rows else ""
        need(len(rows) == 1 and len(head) >= 40
             and (head.startswith(STOP_HEAD) or STOP_HEAD.startswith(head)),
             f"A: the one stored assistant row is the harness's stop call: "
             f"{[str(r.get('head'))[:120] for r in rows]}")
        need(not any("<invoke" in str(r.get("head")) or REPORT in str(r.get("head"))
                     for r in hist),
             "A: no attempt, loop or report is stored")
        need(any(STOP_REASON in last_user(t["body"]) for t in mb.turns()),
             "A: the sender (BOSS) was told the stop reason: "
             f"{[last_user(t['body'])[:120] for t in mb.turns()]}")
        seen_a = {r.get("id") for r in hist}

        # ── B · nine retried turns in a row, then done: no stop ───────────────
        m.clear()
        mb.clear()
        m.reply(*[FOREIGN, FOREIGN, reflect] * 9, FOREIGN, FOREIGN, DONE)
        out = hk.send(AI, {"type": "send", "text": "go B"})
        need(not out.get("error"), f"B: the send is accepted: {str(out)[:200]}")
        turns_at_least(m, 30)
        need(idle(), "B: the chain settles")
        time.sleep(1)
        turns = m.turns()
        need(len(turns) == 30, f"B: exactly 30 requests (10 turns of 3 attempts): {len(turns)}")
        need(all(reasked(turns, 3 * k, 3) for k in range(10)),
             "B: in each turn requests 2-3 are re-asks of its first request")
        firsts = [convo(turns[3 * k]) for k in range(10) if len(turns) > 3 * k]
        need(all("<invoke" not in c and REPORT not in c for c in firsts),
             "B: no turn's first request holds a failed attempt or report of an earlier turn")
        need(all("[reply to " in last_user(turns[3 * k]["body"]) for k in range(1, 10)
                 if len(turns) > 3 * k),
             "B: each later turn starts from the reflect reply of the turn before")
        need(bool(firsts) and STOP_REASON in firsts[0],
             "B: request 1 shows A's stored stop call")
        new_b = [r for r in history() if r.get("id") not in seen_a]
        need(not any(str(r.get("head")).startswith('<call name="stop">') for r in new_b),
             "B: no stop call was stored")
        need(not any(STOP_REASON in json.dumps(t["body"]) for t in mb.turns()),
             "B: nobody was told of a stop")
        seen_b = seen_a | {r.get("id") for r in new_b}

        # ── C · a malformed answer after thinking: re-asked, not stored ───────
        m.clear()
        mb.clear()
        m.reply(f'<think>PLAN-{TOKEN} then invoke</think>Sending it.\n<invoke name="send">',
                f'<think>KEEP-{TOKEN}</think>Done here.\n{DONE}')
        out = hk.send(AI, {"type": "send", "text": "go C"})
        need(not out.get("error"), f"C: the send is accepted: {str(out)[:200]}")
        turns_at_least(m, 2)
        need(idle(), "C: the chain settles")
        turns = m.turns()
        need(len(turns) == 2, f"C: two requests: {len(turns)}")
        need(reasked(turns, 0, 2), "C: request 2 is the re-ask of request 1")
        if len(turns) > 1:
            msgs = json.dumps(turns[1]["body"].get("messages") or [])
            need(TOKEN not in msgs and "Sending it." in msgs,
                 "C: request 2 holds the failed attempt, not its thought")
        new_c = [r for r in history() if r.get("id") not in seen_b]
        need(any(r.get("role") == "assistant" and r.get("has_thinking")
                 and "Done here." in str(r.get("head")) for r in new_c),
             "C: the stored assistant row is the accepted attempt's, with its thinking")
        need(not any("Sending it." in str(r.get("head")) or REPORT in str(r.get("head"))
                     for r in new_c),
             "C: nothing of the failed attempt is stored")
    finally:
        hk.close()
        m.stop()
        mb.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
