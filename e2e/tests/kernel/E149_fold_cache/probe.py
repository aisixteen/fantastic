"""kernel/fold_cache — the fold cycle under `caching` (#556, #571) against a SCRIPTED model. NO MODEL.

PART A, `caching: cleanup`, a scripted vllm plug on both lanes. The docs reply is whole in
its own turn; in the NEXT request it is the stub main emits, after the block's own tag line:

  [docs of={of} tier={tier} — dropped from your view; held this run: send yourself
   <type>grep</type> <ids><item>{pid}</item></ids> <query>{verb}</query>]

  Rust `docs_dropped` fantastic-ai-core/src/history_view.rs:126-133, `drop_read_docs`
  :202-245 (applied only when `caching.cleans()`, assembly.rs:672, caching.rs:72-75).
  Python `_DOCS_DROPPED` ai_core/core.py:1502-1505, `drop_read_docs` :1556, call :2934.

PART B, `caching: provider_cache`: the whole cycle. Autofold (`Caching::folds`,
caching.rs:85-88; Rust history_view.rs:522-603, Python ai_core/autofold.py:146-192):

  [folded · {verb} reply from {agent} · ~{x} tokens · head: "{line}" · get: {row id}]
  [folded · recall reply from {agent} · id {N} · ~{x} tokens · get: {row id}]

over AUTOFOLD_AT = 16000 tokens (64,000 chars); only an ANSWERED row folds. `recall` by
id: Rust verbs.rs:741, Python core.py:4651. `grep` (query required, optional ids/limit/
width): Rust verbs.rs:60 + retrieval.rs:83-238, Python core.py:4784; it returns
{matches:[{id,line,text}], total, truncated}. The cache mark (#571): `mark()` Rust
caching.rs:182-219, Python ai_core/caching.py:117-149, set only when `caching.marks()`
(Rust agent_loop.rs:252-256, Python core.py:3542), carried by `render_for_model`
(Rust agent_loop.rs:120-134, Python core.py:523-533). The plugs forward it:
  rust vllm (fantastic-vllm-backend lib.rs:309-326 `one_system_message`): messages are
    cloned verbatim, so a row carries `"cache_mark": true`; all system messages merge
    into one new {role, content}, so the system mark does not reach this wire;
  python anthropic (anthropic_backend/provider.py:386-428 `to_wire`/`turn_content`,
    :532-548 `body`): a marked row's last block carries cache_control {"type":"ephemeral"},
    a marked system message becomes `system: [{…, cache_control}]`.
Rust runs the chat on `ai_chat.tools` + vllm plug (provider_cache=true on the chat); Python's
`vllm_backend` refuses provider_cache, so Part B uses `anthropic_backend` on the Anthropic mock.

A test-only folder bundle `bigdoc` (written at runtime, as E117) supplies one verb doc over
64,000 chars; no stock docs reply is that large.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

import yaml
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.anthropic import AnthropicMock
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
HELPER = (REPO / "src/lib/python/bundled_agents/io/py_agent/src/py_agent"
          / "fantastic_agent.py")
AI_A, AI_B, FILES = "ai_a", "ai_b", "files"
PROV_A, PROV_B = "prov_a", "prov_b"  # rust only
MODEL = "mock-scripted"
TOKEN = "E149-" + secrets.token_hex(4)
NEEDLE = f"NEEDLE-{TOKEN}"
DOCWORD = f"DOCWORD-{TOKEN}"
BUNDLE, VERB = "bigdoc", "survey"
DONE = '<call name="done"></call>'
KEY = "mock-e2e-" + secrets.token_hex(12)
KEY_VAR = "E2E_MOCK_KEY"
EPH = {"type": "ephemeral"}
DOC = (f"args: none ({DOCWORD}). Returns the survey.\n\n"
       + "".join(f"S{n:03d} " + "y" * 775 + "\n" for n in range(1, 91)))


def write_bundle(work: Path) -> None:
    folder = work / ".fantastic" / "bundles" / BUNDLE
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copy(HELPER, folder / "fantastic_agent.py")
    (folder / "agent.py").write_text(
        "import fantastic_agent as fa\n\n"
        "def survey(p):\n    return {'ok': True}\n\n"
        "fa.serve({'survey': survey})\n")
    (folder / "bundle.yaml").write_text(yaml.safe_dump({
        "base": "py_agent",
        "sentence": "The big-doc test agent.",
        "command": [sys.executable, "agent.py"],
        "verbs": {VERB: DOC},
    }, sort_keys=False))


# ── wire readers: one per plug ────────────────────────────────────────────────
def msgs(turn: dict) -> list[dict]:
    return turn["body"].get("messages") or []


def content_text(c) -> str:
    if isinstance(c, str):
        return c
    return "".join(b.get("text", "") for b in c or [] if isinstance(b, dict))


def texts(turn: dict) -> list[str]:
    """Every text the request carries."""
    out: list[str] = []
    if RUNTIME == "python":  # anthropic: `system` (string or blocks) + message content
        out.append(content_text(turn["body"].get("system")))
    out += [content_text(m.get("content")) for m in msgs(turn)]
    return out


def full(turn: dict) -> str:
    return "\n".join(texts(turn))


def last_user(turn: dict) -> str:
    for m in reversed(msgs(turn)):
        if m.get("role") == "user":
            return content_text(m.get("content"))
    return ""


def marked(m: dict) -> bool:
    if RUNTIME == "python":
        c = m.get("content")
        return isinstance(c, list) and any(
            isinstance(b, dict) and b.get("cache_control") == EPH for b in c)
    return m.get("cache_mark") is True


def system_marked(turn: dict) -> bool:
    s = turn["body"].get("system")
    return isinstance(s, list) and any(
        isinstance(b, dict) and b.get("cache_control") == EPH for b in s)


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-fold-cache-"))
    (work / "files").mkdir()
    # 100 lines of 800 chars (80 kB): the `read` reply is over 64,000. Line 50 holds NEEDLE.
    lines = []
    for n in range(1, 101):
        head = f"L{n:03d} " + (f"{NEEDLE} " if n == 50 else "")
        lines.append(head + "x" * (800 - len(head)) + "\n")
    (work / "files" / "big.txt").write_text("".join(lines))
    write_bundle(work)
    if RUNTIME == "python":
        os.environ[KEY_VAR] = KEY
    hk = HostKernel(spec, work,
                    reach=[AI_A, AI_B, FILES, PROV_A, PROV_B, "creds", "memory"]).boot()
    root = hk.root_id
    mA = VllmMock(model=MODEL).start()
    mB = (AnthropicMock(model=MODEL) if RUNTIME == "python" else VllmMock(model=MODEL)).start()
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

    def run(m, aid: str, label: str, text: str, want: int) -> list[dict]:
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

    def call(target: str, payload: str) -> str:
        return (f'<call name="send"><routing><target_id>{target}</target_id></routing>'
                f'<payload>{payload}</payload></call>')

    docs = call("kernel", f"<type>docs</type><of>{BUNDLE}</of>")
    docs_detailed = call("kernel", f"<type>docs</type><of>{BUNDLE}</of><tier>detailed</tier>")
    read = call(FILES, "<type>read</type><path>big.txt</path>")
    allb: list[dict] = []
    anchors: list[tuple[str, str, int]] = []  # (label, anchor, index in allb of first sight)
    try:
        # ── wiring ────────────────────────────────────────────────────────────
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), AI_A: "*", AI_B: "*",
                                           PROV_A: "*", PROV_B: "*"}})
        need(r.get("updated") is True, f"memory accepts the AIs: {str(r)[:200]}")
        r = hk.send(root, {"type": "update_agent", "id": root,
                           "callable_by": {**callable_by(root), AI_A: ["docs"], AI_B: ["docs"]}})
        need(r.get("updated") is True, f"the root accepts the AIs' docs: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."], ingress_rule="allow_all_ingress",
                        callable_by={"$self": "*", "$parent": "*", AI_A: "*", AI_B: "*"})
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        can = {"$self": "*", "memory": "*", FILES: "*", root: ["docs"]}
        by = {"$self": "*", "$parent": "*", "rest": "*"}
        if RUNTIME == "rust":
            for prov, m in ((PROV_A, mA), (PROV_B, mB)):
                rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=prov,
                                endpoint=m.url, model=MODEL, max_tokens=8192,
                                context_window=262144, kv_slots=4, kv_overflow="evict",
                                memory_store_id="memory", can_call={"$self": "*", "memory": "*"},
                                callable_by={"$self": "*", "$parent": "*",
                                             AI_A: "*", AI_B: "*"})
                need(rec.get("id") == prov, f"create {prov}: {str(rec)[:200]}")
            for aid, prov, mode in ((AI_A, PROV_A, "cleanup"), (AI_B, PROV_B, "provider_cache")):
                rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                                provider_id=prov, model=MODEL, max_tokens=8192,
                                context_window=262144, memory_store_id="memory",
                                caching=mode, can_call={**can, prov: ["complete", "watch", "reflect"]},
                                callable_by=by)
                need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")
        else:
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=AI_A,
                            endpoint=mA.url, model=MODEL, max_tokens=8192,
                            context_window=262144, kv_slots=4, kv_overflow="evict",
                            memory_store_id="memory", caching="cleanup",
                            can_call=can, callable_by=by)
            need(rec.get("id") == AI_A, f"create {AI_A}: {str(rec)[:200]}")
            mB.require_key(KEY)
            r = hk.send(root, {"type": "update_agent", "id": "creds",
                               "refs": {"anthropic": {"host": mB.host,
                                                      "env": {"api_key": KEY_VAR}}},
                               "callable_by": {**callable_by("creds"), AI_B: ["get"]}})
            need(r.get("updated") is True, f"creds: the ref: {str(r)[:200]}")
            rec = hk.create(parent_id="rest", handler_module="anthropic_backend.tools", id=AI_B,
                            endpoint=mB.url, credentials={"store": "creds", "ref": "anthropic"},
                            model=MODEL, max_tokens=8192, context_window=262144,
                            memory_store_id="memory", caching="provider_cache",
                            can_call={**can, "creds": ["get"]}, callable_by=by)
            need(rec.get("id") == AI_B, f"create {AI_B}: {str(rec)[:200]}")

        # ══ PART A · caching: cleanup ═════════════════════════════════════════
        # A1 · the docs reply is whole in its own turn
        mA.reply(docs, f"Noted.\n{DONE}")
        turns = run(mA, AI_A, "A1", "docs", 2)
        last = last_user(turns[1]) if len(turns) > 1 else ""
        need(all(s in last for s in (" · kernel docs", f"<of>{BUNDLE}</of>",
                                      "<tier>oneliners</tier>", DOCWORD)),
             f"A1: request 2 carries the whole docs reply: {last[:200]!r}")
        got = re.search(r"\[reply to (\S+) · kernel docs( · ↑\d+)?\]", last)
        need(got is not None, "A1: the docs block names its row")
        p = got.group(1) if got else "?"
        tag = got.group(0) if got else "?"  # `[reply to P · kernel docs · ↑N]`

        # A2 · the next request holds the stub, not the body
        mA.clear()
        mA.reply(DONE)
        turns = run(mA, AI_A, "A2", "next", 1)
        seen = full(turns[0]) if turns else ""
        stub = (f"{tag}\n[docs of={BUNDLE} tier=oneliners — dropped "
                f"from your view; held this run: send yourself <type>grep</type> "
                f"<ids><item>{p}</item></ids> <query>{VERB}</query>]")
        need(stub in seen, f"A2: the docs reply is its stub; docs block near: "
             f"{seen[max(0, seen.find('kernel docs') - 40):][:420]!r}")
        need(DOCWORD not in seen, "A2: the docs body is out of the view")

        # ══ PART B · caching: provider_cache ══════════════════════════════════
        def collect(turns: list[dict]) -> None:
            allb.extend(turns)

        def first_seen(label: str, pattern: str, turns: list[dict], base: int) -> str:
            """The anchor of `pattern` in request 1 of `turns`; remembered for B4."""
            m = re.search(pattern, full(turns[0])) if turns else None
            need(m is not None, f"{label}: request 1 shows the anchor /{pattern[:60]}…/")
            if m:
                anchors.append((label, m.group(0), base))
                return m.group(0)
            return ""

        # B1a · the read reply is whole the turn it arrives
        mB.reply(read, f"Got it.\n{DONE}")
        turns = run(mB, AI_B, "B1a", "read big.txt", 2)
        base = len(allb)
        collect(turns)
        need(len(turns) > 1 and "L100 " in last_user(turns[1]) and NEEDLE in last_user(turns[1]),
             "B1a: request 2 carries the whole read reply")
        big = big_rows(AI_B)
        need(len(big) == 1, f"B1a: one large user row: {[r.get('bytes') for r in history(AI_B)]}")
        if not big:
            raise SystemExit(verdict(False, f"{RUNTIME}: no large row to fold"))
        b = big[0]["id"]
        whole_marks = [("B1a req2", base + 1)] if len(turns) > 1 else []
        plain_marks = [("B1a req1", base)]

        # B1b · the read reply is its anchor; a large docs reply is whole in its turn
        mB.clear()
        mB.reply(docs_detailed, f"Noted.\n{DONE}")
        turns = run(mB, AI_B, "B1b", "docs", 2)
        base = len(allb)
        collect(turns)
        anchor_read = first_seen(
            "B1b", rf'\[folded · read reply from files · ~\d+ tokens · head: "[^"]*" · get: {b}\]',
            turns, base)
        need(bool(turns) and "L100 " not in full(turns[0]), "B1b: the read reply is folded")
        last = last_user(turns[1]) if len(turns) > 1 else ""
        need(f"<of>{BUNDLE}</of>" in last and "<tier>detailed</tier>" in last and "S090 " in last,
             f"B1b: request 2 carries the whole docs reply: {last[:200]!r}")
        big = [r for r in big_rows(AI_B) if r["id"] != b]
        need(len(big) == 1, f"B1b: one new large user row: {[r.get('bytes') for r in history(AI_B)]}")
        if not big:
            raise SystemExit(verdict(False, f"{RUNTIME}: no docs row to fold"))
        d = big[0]["id"]
        whole_marks.append(("B1b req2", base + 1))

        # B2 · recall by row id gives the docs body back
        mB.clear()
        mB.reply(call(AI_B, f"<type>recall</type><id>{d}</id>"), f"Noted.\n{DONE}")
        turns = run(mB, AI_B, "B2", "recall", 2)
        base = len(allb)
        collect(turns)
        anchor_docs = first_seen(
            "B2", rf'\[folded · docs reply from kernel · ~\d+ tokens · head: "[^"]*" · get: {d}\]',
            turns, base)
        need(bool(turns) and "S090 " not in full(turns[0]), "B2: the docs reply is folded")
        last = last_user(turns[1]) if len(turns) > 1 else ""
        need(f" {AI_B} recall" in last and "S090 " in last,
             f"B2: request 2 carries the recalled row whole: {last[:200]!r}")
        whole_marks.append(("B2 req2", base + 1))

        # B3 · grep finds a word inside a folded body; the body stays folded
        mB.clear()
        mB.reply(call(AI_B, f"<type>grep</type><query>{NEEDLE}</query>"), f"Noted.\n{DONE}")
        turns = run(mB, AI_B, "B3", "grep", 2)
        base = len(allb)
        collect(turns)
        first_seen("B3", rf"\[folded · recall reply from {AI_B} · id {d} · ~\d+ tokens · get: \d+\]",
                   turns, base)
        need(bool(turns) and "S090 " not in full(turns[0]), "B3: the recall reply refolds")
        need(any(r.get("id") == d and int(r.get("bytes") or 0) > 64000 for r in history(AI_B)),
             "B3: the store row is still whole")
        last = last_user(turns[1]) if len(turns) > 1 else ""
        need(f" {AI_B} grep" in last and f"L050 {NEEDLE}" in last,
             f"B3: the grep hit is inside the folded body: {last[:300]!r}")
        need(len(turns) > 1 and anchor_read in full(turns[1]) and "L100 " not in full(turns[1]),
             "B3: the read body is still folded in the view")
        plain_marks.append(("B3 req2", base + 1))

        # B4 · the cache holds
        mB.clear()
        mB.reply(DONE)
        turns = run(mB, AI_B, "B4", "again", 1)
        base = len(allb)
        collect(turns)
        plain_marks.append(("B4 req1", base))
        for label, anchor, at in anchors:
            if not anchor:
                continue
            later = allb[at + 1:]
            miss = [i for i, t in enumerate(later) if anchor not in full(t)]
            need(not miss and bool(later),
                 f"B4: the {label} anchor is byte-identical in the {len(later)} later requests"
                 f" (missing in {miss})")
        need(all(any(marked(m) for m in msgs(t)) for t in allb),
             f"B4(a): every one of {len(allb)} requests carries a cache mark: "
             f"{[sum(marked(m) for m in msgs(t)) for t in allb]}")
        if RUNTIME == "python":
            need(all(system_marked(t) for t in allb),
                 f"B4(a): every request marks the system block: {[system_marked(t) for t in allb]}")
        for label, i in whole_marks:
            ms = msgs(allb[i])
            need(len(ms) >= 2 and not marked(ms[-1]) and marked(ms[-2]),
                 f"B4(b): {label}: the mark sits before the whole body: "
                 f"{[marked(m) for m in ms][-4:]}")
        for label, i in plain_marks:
            ms = msgs(allb[i])
            need(bool(ms) and marked(ms[-1]),
                 f"B4(c): {label}: the last message is marked: {[marked(m) for m in ms][-4:]}")
    finally:
        hk.close()
        mA.stop()
        mB.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
