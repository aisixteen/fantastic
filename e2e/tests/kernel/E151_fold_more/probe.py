"""kernel/fold_more — the rest of the fold cycle (#556, #532) against a SCRIPTED model. NO MODEL.

Rust lane only. Scripted vllm plugs play the model for four `ai_chat.tools` chats (each names
its own `vllm_backend` provider) on the PACKAGED host, next to a `file_bridge` agent (`files`,
an 80 kB file and a folder of 350 files) and an `exec_bridge` agent (`ex`). Every fold check
asserts BOTH halves: the body whole in the request that carries it, the anchor in the next.
`context_window` is 262144, so every body below (over 64,000 chars = AUTOFOLD_AT 16000 tokens,
under the inline cap of about 127,000) is delivered inline.

  T  `provider_cache`, a thinking model.
     T1  the model thinks, reads, thinks, answers; T2 the model recalls its FIRST thought by
         id: request 1 shows the thought as `[thought ~N tokens · id P]` with none of its text,
         request 2 carries the recall reply with the thought WHOLE; T3 the recall reply is
         `[folded · recall reply from … · id P · ~N tokens · get: R]` and the thought text is
         nowhere in the request; the store still holds it (a `recall` by the person).
     T4  across all those requests: the messages up to the LAST cache mark of a request are
         byte-equal in the next request, and a request never has fewer messages than the one
         before it (the property that makes the provider's cache hit, through a turn that
         produces a thought and the turns after it).
  V  `provider_cache` (the default), other verbs: the reply of an `exec_bridge` `run` (500 lines,
     the verb's own cap) and of a `file_bridge` `list` (350 entries) each go through the same
     cycle as the `read` reply of E141: whole in its own turn, its
     `[folded · {verb} reply from {agent} · ~N tokens · head: "…" · get: ID]` anchor in the
     next with the body gone, the store row whole, `recall` by id gives it whole and the
     recall reply refolds.
     `history` is NOT covered here: its reply is a digest paged to what fits inline (the
     `fit_reply` cap, about 25,000 tokens), so reaching 16,000 tokens takes about 450 rows,
     over 200 scripted exchanges; the verb-blind rule is the one the `fantastic-ai-core`
     unit `a_large_reply_is_whole_the_turn_it_arrives_and_an_anchor_from_the_next` holds.
  N  `caching: none`, the raw mode: no fold, no anchor, no cache mark. The large read reply,
     and a recall reply after it, stay whole in every later request, and the store is as it
     was.
  U  a person's fold, the free twin of E72_user_fold: after three turns the person (the test,
     over `web_rest`) sends `context_fold` for the two rows of the second turn with a given
     summary. A fold row lands (`[fold ID · 2 refs · L1] "title"`), the next request has it in
     place of those rows, a large read reply is whole in its turn and its anchor in the next
     while the fold line stays, and the folded rows are still in the store (`recall`,
     `history {fold}`).
"""

from __future__ import annotations

import json
import os
import re
import secrets
import shlex
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import oslayer
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import ThinkingVllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
FILES, EXEC = "files", "ex"
AI_T, AI_V, AI_N, AI_U = "ai_t", "ai_v", "ai_n", "ai_u"
AIS = [AI_T, AI_V, AI_N, AI_U]
MODEL = "mock-scripted"
TOKEN = "E151-" + secrets.token_hex(4)
DONE = '<call name="done"></call>'
PY = sys.executable
ALLOWED = [re.escape(shlex.quote(PY)) + r" -c .*"]
# 350 files, 130-char names: the stored `list` row is about 68,000 bytes (600 files made
# 117,000), past the 64,000 fold line, and the body stays under the inline cap (600 files made
# a 204,000-char body, which is held, not inlined).
MANY, MANY_N, LAST_NAME = "many", 350, "n0349-"
# 500 lines of 170 chars (85,000): `run` keeps the last 500 (the cap of `tail`), past the 64,000
# fold line with margin.
RUN_PROGRAM = "for i in range(500): print('X%04d ' % i + 'x' * 164)"
LAST_LINE = "X0499 "


def msgs(turn: dict) -> list[dict]:
    return turn["body"].get("messages") or []


def content_text(c) -> str:
    if isinstance(c, str):
        return c
    return "".join(b.get("text", "") for b in c or [] if isinstance(b, dict))


def last_user(turn: dict) -> str:
    for m in reversed(msgs(turn)):
        if m.get("role") == "user":
            return content_text(m.get("content"))
    return ""


def blob(turn: dict) -> str:
    return json.dumps(msgs(turn), ensure_ascii=False)


def unmarked(ms: list[dict]) -> list[dict]:
    """The messages without the `cache_mark` flag: the mark moves on, the bytes must not."""
    return [{k: v for k, v in m.items() if k != "cache_mark"} for m in ms]


def marked(m: dict) -> bool:
    return m.get("cache_mark") is True


def last_mark(ms: list[dict]) -> int:
    return max((i for i, m in enumerate(ms) if marked(m)), default=-1)


def peek(what: str, text: str) -> None:
    """On a failed check: the size, head and tail of the text that was searched."""
    print(f"     [{what}] {len(text)} chars · head {text[:240]!r} · tail {text[-240:]!r}", flush=True)


def call(target: str, payload: str) -> str:
    return (f'<call name="send"><routing><target_id>{target}</target_id></routing>'
            f'<payload>{payload}</payload></call>')


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-fold-more-"))
    (work / "files" / MANY).mkdir(parents=True)
    (work / "run").mkdir()
    # 100 lines of 800 characters (80 kB): the `read` reply is over 64,000.
    (work / "files" / "big.txt").write_text(
        "".join(f"L{n:03d} " + "x" * 795 + "\n" for n in range(1, 101)))
    for n in range(MANY_N):
        (work / "files" / MANY / (f"n{n:04d}-" + "a" * 124)).write_text("")
    hk = HostKernel(spec, work, reach=[*AIS, FILES, EXEC, "memory"]
                    + [f"prov_{a}" for a in AIS]).boot()
    root = hk.root_id
    mocks = {a: ThinkingVllmMock(model=MODEL).start() for a in AIS}
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def callable_by(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        rec = next(a for a in r.get("agents") or [] if a.get("id") == aid)
        return dict(rec.get("callable_by") or {})

    def turns_at_least(m, n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.turns()

    def idle(aid: str, bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == aid for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def history(aid: str) -> list[dict]:
        h = hk.send(aid, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    def run(aid: str, label: str, text: str, want: int, *replies: str) -> list[dict]:
        """One send of `text` to `aid`, its mock scripted with `replies`; `want` requests."""
        m = mocks[aid]
        m.clear()
        m.reply(*replies)
        out = hk.send(aid, {"type": "send", "text": text})
        need(not out.get("error"), f"{label}: the send is accepted: {str(out)[:200]}")
        turns_at_least(m, want)
        need(idle(aid), f"{label}: the chain settles")
        time.sleep(0.5)
        turns = m.turns()
        need(len(turns) == want, f"{label}: {want} requests: {len(turns)}")
        return turns

    def big_rows(aid: str) -> list[dict]:
        return [r for r in history(aid)
                if r.get("role") == "user" and int(r.get("bytes") or 0) > 64000]

    def new_big(aid: str, known: set, label: str) -> int:
        fresh = [r for r in big_rows(aid) if r["id"] not in known]
        need(len(fresh) == 1, f"{label}: one new large user row: "
             f"{[r.get('bytes') for r in history(aid)]}")
        if not fresh:
            raise SystemExit(verdict(False, f"{RUNTIME}: {label}: no large row to fold"))
        known.add(fresh[0]["id"])
        return fresh[0]["id"]

    read = call(FILES, "<type>read</type><path>big.txt</path>")
    allt: list[dict] = []
    try:
        # ── wiring ────────────────────────────────────────────────────────────
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"),
                                           **{a: "*" for a in AIS},
                                           **{f"prov_{a}": "*" for a in AIS}}})
        need(r.get("updated") is True, f"memory accepts the chats: {str(r)[:200]}")
        users = {a: "*" for a in AIS}
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."], ingress_rule="allow_all_ingress",
                        callable_by={"$self": "*", "$parent": "*", **users})
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="exec_bridge.tools", id=EXEC,
                        ingress_rule="allow_all_ingress", cwd=str(work / "run"),
                        env=oslayer.child_env(), allowed=ALLOWED,
                        callable_by={"$self": "*", "$parent": "*", **users})
        need(rec.get("id") == EXEC, f"create {EXEC}: {str(rec)[:200]}")
        can = {"$self": "*", "memory": "*", FILES: "*", EXEC: "*"}
        by = {"$self": "*", "$parent": "*", "rest": "*"}
        for aid, caching in ((AI_T, "provider_cache"), (AI_V, None),
                             (AI_N, "none"), (AI_U, None)):
            prov = f"prov_{aid}"
            # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=prov,
                            endpoint=mocks[aid].url, model=MODEL, max_tokens=8192,
                            context_window=262144, kv_slots=4, kv_overflow="evict",
                            memory_store_id="memory",
                            can_call={"$self": "*", "memory": "*"},
                            callable_by={"$self": "*", "$parent": "*", aid: "*"})
            need(rec.get("id") == prov, f"create {prov}: {str(rec)[:200]}")
            extra = {"caching": caching} if caching else {}
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                            provider_id=prov, model=MODEL, max_tokens=8192,
                            context_window=262144, memory_store_id="memory", **extra,
                            can_call={**can, prov: ["complete", "watch"]}, callable_by=by)
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        # ══ T · provider_cache, a thinking model: a thought by id ═════════════
        # T1 · two thoughts; the read reply is whole the turn it arrives
        turns = run(AI_T, "T1", "read big.txt", 2,
                    f"<think>PLAN-{TOKEN}</think>Reading.\n{read}",
                    f"<think>SEEN-{TOKEN}</think>Got it.\n{DONE}")
        allt += turns
        need(len(turns) > 1 and "L100 " in last_user(turns[1]),
             "T1: request 2 carries the whole read reply")
        thoughts = sorted(r["id"] for r in history(AI_T)
                          if r.get("role") == "assistant" and r.get("has_thinking"))
        need(len(thoughts) >= 2, f"T1: two assistant rows hold a thought: {thoughts}")
        if not thoughts:
            raise SystemExit(verdict(False, f"{RUNTIME}: no thought row to recall"))
        p = thoughts[0]

        # T2 · the thought is an anchor; recall by id gives it whole
        turns = run(AI_T, "T2", "next", 2,
                    call(AI_T, f"<type>recall</type><id>{p}</id>"), f"Noted.\n{DONE}")
        allt += turns
        seen = blob(turns[0]) if turns else ""
        bt = new_big(AI_T, set(), "T2")
        need(re.search(rf'\[folded · read reply from {FILES} · ~\d+ tokens · '
                       rf'head: \\?"[^"]*\\?" · get: {bt}\]', seen) is not None
             and "L100 " not in seen, "T2: request 1 shows the read reply as its anchor, body gone")
        need(re.search(rf"\[thought ~\d+ tokens · id {p}\]", seen) is not None,
             "T2: request 1 shows the thought as its anchor")
        need(f"PLAN-{TOKEN}" not in seen and f"SEEN-{TOKEN}" not in seen,
             "T2: request 1 holds no thought text")
        last = last_user(turns[1]) if len(turns) > 1 else ""
        need(f" {AI_T} recall" in last and f"PLAN-{TOKEN}" in last,
             f"T2: request 2 carries the recall reply with the thought whole: {last[:300]!r}")

        # T3 · the recall reply folds after the next answer; the thought is still stored
        turns = run(AI_T, "T3", "again", 1, DONE)
        allt += turns
        seen = blob(turns[0]) if turns else ""
        need(re.search(rf"\[folded · recall reply from {AI_T} · id {p} · ~\d+ tokens · "
                       rf"get: \d+\]", seen) is not None,
             "T3: request 1 shows the recall reply as its anchor")
        need(f"PLAN-{TOKEN}" not in seen, "T3: the thought text is nowhere in the request")
        got = hk.send(AI_T, {"type": "recall", "id": p})
        rows = got.get("messages") or []
        need(bool(rows) and f"PLAN-{TOKEN}" in str(rows[0].get("thinking")),
             f"T3: the store row still holds the thought: {str(got)[:200]}")

        # T4 · the bytes before the cache mark never change, and the history only grows
        need(len(allt) == 5, f"T4: five requests were collected: {len(allt)}")
        need(all(last_mark(msgs(t)) >= 0 for t in allt),
             f"T4: every request carries a cache mark: "
             f"{[last_mark(msgs(t)) for t in allt]}")
        for k, (a, b) in enumerate(zip(allt, allt[1:])):
            i = last_mark(msgs(a))
            need(i >= 0 and unmarked(msgs(a))[:i + 1] == unmarked(msgs(b))[:i + 1],
                 f"T4: request {k + 1}: the {i + 1} messages up to its last mark are "
                 f"byte-equal in request {k + 2}")
            need(len(msgs(b)) >= len(msgs(a)),
                 f"T4: request {k + 2} has no fewer messages than request {k + 1}: "
                 f"{len(msgs(a))} then {len(msgs(b))}")

        # ══ V · provider_cache, the large replies of other verbs ═════════════════════
        known: set = set()

        def cycle(label: str, send_text: str, the_call: str, whole: str,
                  verb: str, agent: str) -> None:
            """whole, anchor, store, recall by id, refold: for the reply of `verb`."""
            turns = run(AI_V, f"{label}a", send_text, 2, the_call, f"Noted.\n{DONE}")
            if len(turns) > 1 and whole not in last_user(turns[1]):
                peek(f"{label}a request 2 last user", last_user(turns[1]))
            need(len(turns) > 1 and whole in last_user(turns[1]),
                 f"{label}a: request 2 carries the whole {verb} reply")
            b = new_big(AI_V, known, f"{label}a")
            turns = run(AI_V, f"{label}b", "next", 2,
                        call(AI_V, f"<type>recall</type><id>{b}</id>"), f"Noted.\n{DONE}")
            seen = blob(turns[0]) if turns else ""
            need(re.search(rf'\[folded · {verb} reply from {agent} · ~\d+ tokens · '
                           rf'head: \\?"[^"]*\\?" · get: {b}\]', seen) is not None,
                 f"{label}b: request 1 shows the {verb} reply as its anchor")
            need(whole not in seen, f"{label}b: request 1 holds no {verb} body")
            last = last_user(turns[1]) if len(turns) > 1 else ""
            need(f" {AI_V} recall" in last and whole in last,
                 f"{label}b: request 2 carries the recalled row whole")
            turns = run(AI_V, f"{label}c", "again", 1, DONE)
            seen = blob(turns[0]) if turns else ""
            need(re.search(rf"\[folded · recall reply from {AI_V} · id {b} · ~\d+ tokens · "
                           rf"get: \d+\]", seen) is not None,
                 f"{label}c: request 1 shows the recall reply as its anchor")
            need(whole not in seen, f"{label}c: the {verb} body is nowhere in the request")
            need(any(r.get("id") == b and int(r.get("bytes") or 0) > 64000
                     for r in history(AI_V)), f"{label}c: the store row is still whole")
            # the recall reply of this cycle is a large row too: not the next cycle's reply
            known.update(r["id"] for r in big_rows(AI_V))

        # an exec run: `-c PROGRAM` with the interpreter allowed by the record
        run_call = call(EXEC, f"<type>run</type><command>{PY}</command>"
                              f"<args><item>-c</item><item>{RUN_PROGRAM}</item></args>"
                              "<tail>500</tail>")
        cycle("V1", "run the program", run_call, LAST_LINE, "run", EXEC)
        cycle("V2", "list the folder",
              call(FILES, f"<type>list</type><path>{MANY}</path>"), LAST_NAME, "list", FILES)

        # ══ N · caching none: nothing folds, nothing is marked ════════════════
        allnone: list[dict] = []
        known_n: set = set()
        turns = run(AI_N, "N1", "read big.txt", 2,
                    f"<think>PLAN-{TOKEN}</think>Reading.\n{read}",
                    f"<think>SEEN-{TOKEN}</think>Got it.\n{DONE}")
        allnone += turns
        need(len(turns) > 1 and "L100 " in last_user(turns[1]),
             "N1: request 2 carries the whole read reply")
        b = new_big(AI_N, known_n, "N1")
        turns = run(AI_N, "N2", "next", 2,
                    call(AI_N, f"<type>recall</type><id>{b}</id>"), f"Noted.\n{DONE}")
        allnone += turns
        seen = blob(turns[0]) if turns else ""
        need("L100 " in seen and "[folded ·" not in seen,
             "N2: request 1 holds the read reply whole, no fold anchor")
        need("[thought ~" not in seen, "N2: request 1 shows no thought anchor")
        last = last_user(turns[1]) if len(turns) > 1 else ""
        need(f" {AI_N} recall" in last and "L100 " in last,
             "N2: request 2 carries the recalled row whole")
        turns = run(AI_N, "N3", "again", 1, DONE)
        allnone += turns
        seen = blob(turns[0]) if turns else ""
        need(seen.count("L100 ") >= 2 and "[folded ·" not in seen and "[thought ~" not in seen,
             f"N3: the read reply and the recall reply are both whole: "
             f"{seen.count('L100 ')} bodies, no anchor")
        need(not any(marked(m) for t in allnone for m in msgs(t)),
             f"N3: none of the {len(allnone)} requests carries a cache mark")
        need(any(r.get("id") == b and int(r.get("bytes") or 0) > 64000
                 for r in history(AI_N)), "N3: the store row is whole")
        print(f"  info N: thought text in the last request: {f'PLAN-{TOKEN}' in seen}")

        # ══ U · a person's fold ═══════════════════════════════════════════════
        for k in range(3):
            run(AI_U, f"U{k}", f"task step {k}", 1, f"step {k} noted.\n{DONE}")
        rows = history(AI_U)
        users_k = [r for r in rows if r.get("role") == "user"
                   and "task step 1" in str(r.get("head"))]
        need(len(users_k) == 1, f"U: the row of step 1 is found: {len(users_k)}")
        if not users_k:
            raise SystemExit(verdict(False, f"{RUNTIME}: no row to fold"))
        first = users_k[0]["id"]
        after = [r for r in rows if r["id"] > first and r.get("role") == "assistant"]
        last_id = after[0]["id"] if after else first
        title = f"TITLE-{TOKEN}"
        out = hk.send(AI_U, {"type": "context_fold", "from": first, "to": last_id,
                             "summary": f"{title}\n\nbody of the span"})
        fold = out.get("folded")
        need(isinstance(fold, int) and (out.get("rows") or 0) >= 2
             and out.get("summarized") is True, f"U: the person's fold lands: {str(out)[:200]}")
        turns = run(AI_U, "U-read", "read big.txt", 2, read, f"Got it.\n{DONE}")
        seen = blob(turns[0]) if turns else ""
        need(f"[fold {fold} · 2 refs · L1]" in seen and title in seen,
             f"U: request 1 has the fold line in place of its rows: {seen[:300]!r}")
        need("task step 1" not in seen and "step 1 noted" not in seen
             and "task step 0" in seen and "task step 2" in seen,
             "U: request 1 holds the rows around the range and none of the folded ones")
        need(len(turns) > 1 and "L100 " in last_user(turns[1]),
             "U: request 2 carries the whole read reply")
        b = new_big(AI_U, set(), "U-read")
        turns = run(AI_U, "U-next", "next", 1, DONE)
        seen = blob(turns[0]) if turns else ""
        need(re.search(rf'\[folded · read reply from {FILES} · ~\d+ tokens · '
                       rf'head: \\?"[^"]*\\?" · get: {b}\]', seen) is not None
             and "L100 " not in seen, "U: request 1 shows the read reply as its anchor")
        need(f"[fold {fold} · 2 refs · L1]" in seen and "task step 1" not in seen,
             "U: the fold line still stands in place of its rows")
        got = hk.send(AI_U, {"type": "recall", "id": first})
        need("task step 1" in str(got.get("messages")),
             f"U: the folded row is still in the store: {str(got)[:200]}")
        got = hk.send(AI_U, {"type": "history", "fold": fold})
        need(first in [r.get("id") for r in got.get("rows") or []],
             f"U: history {{fold}} lists its members: {str(got)[:200]}")
    finally:
        hk.close()
        for m in mocks.values():
            m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
