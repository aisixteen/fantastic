"""kernel/fold_cache — the fold cycle under `caching` (#556, #571) against a SCRIPTED model. NO MODEL.

PART B, `caching: provider_cache`: the whole cycle. Autofold (`Caching::folds`,
caching.rs:85-88; Rust history_view.rs:522-603, Python ai_core/autofold.py:146-192):

  {tag line}
  <folded id="{row id}" verb="{verb}" from="{agent}" tokens="{x}" head="{line}"/> unfold: <call …recall {row id}…>
  <folded id="{row id}" verb="recall" from="{agent}" recalled="id {N}" tokens="{x}"/> unfold: …

(the default type's markers, `res/default/marker_folded_*.txt`, map-once)

over AUTOFOLD_AT = 16000 tokens (64,000 chars); only an ANSWERED row folds. `recall` by
id: Rust verbs.rs:741, Python core.py:4651. `grep` (query required, optional ids/limit/
width): Rust verbs.rs:60 + retrieval.rs:83-238, Python core.py:4784; it returns
{matches:[{id,line,text}], total, truncated}. The cache mark (#571): `mark()` Rust
caching.rs:182-219, Python ai_core/caching.py:117-149, set only when `caching.marks()`
(Rust agent_loop.rs:252-256, Python core.py:3542), carried by `render_for_model`
(Rust agent_loop.rs:120-134, Python core.py:523-533). The anthropic plug forwards it (Rust fantastic-anthropic-backend lib.rs; Python
anthropic_backend/provider.py `to_wire`/`body`): a marked row's last block carries
cache_control {"type":"ephemeral"}, a marked system message becomes `system: [{…, cache_control}]`.
The vllm and ollama plugs strip the flag from the wire (#669), so Part B runs the marked chat
(`ai_chat.tools`) on an `anthropic_backend` provider and the Anthropic mock (the key is a mock
key in the creds store's env backend; only that provider holds a `get` line on `creds`).

A test-only folder bundle `bigdoc` (written at runtime, as E117) supplies one verb doc over
64,000 chars, read by `reflect bigdoc_1 part=docs tier=detailed` on an instance (no stock docs
reply is that large). A `reflect` reply never folds (history_view.rs `block_folds`: whole
state, superseded instead): it stays whole in every later request, while the read reply and
the recall reply fold.

The AIs reach `files`, `memory` and `bigdoc_1` by their own `can` lines; an AI with no
line on `creds` gets the unknown answer from it.
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
from helpers.lines import ALL, GET, PROVIDER, call as line_call
from helpers.verdict import verdict
from mocks.anthropic import AnthropicMock
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
HELPER = (REPO / "src/lib/python/bundled_agents/io/py_agent/src/py_agent"
          / "fantastic_agent.py")
AI_A, AI_B, FILES = "ai_a", "ai_b", "files"
PROV_A, PROV_B = "prov_a", "prov_b"
MODEL = "mock-scripted"
TOKEN = "E149-" + secrets.token_hex(4)
NEEDLE = f"NEEDLE-{TOKEN}"
DOCWORD = f"DOCWORD-{TOKEN}"
BUNDLE, VERB = "bigdoc", "survey"
BIGDOC = f"{BUNDLE}_1"  # the instance whose docs the AIs reflect
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
        "verbs": {VERB: {"doc": DOC, "schema": {"type": "object", "properties": {}}}},
    }, sort_keys=False))


# ── wire readers: one per plug ────────────────────────────────────────────────
def msgs(one_pass: dict) -> list[dict]:
    return one_pass["body"].get("messages") or []


def content_text(c) -> str:
    if isinstance(c, str):
        return c
    return "".join(b.get("text", "") for b in c or [] if isinstance(b, dict))


def texts(one_pass: dict) -> list[str]:
    """Every text the request carries."""
    out: list[str] = []
    out.append(content_text(one_pass["body"].get("system")))  # anthropic: `system` (string or blocks)
    out += [content_text(m.get("content")) for m in msgs(one_pass)]
    return out


def full(one_pass: dict) -> str:
    return "\n".join(texts(one_pass))


def last_user(one_pass: dict) -> str:
    for m in reversed(msgs(one_pass)):
        if m.get("role") == "user":
            return content_text(m.get("content"))
    return ""


def marked(m: dict) -> bool:
    c = m.get("content")
    return isinstance(c, list) and any(
        isinstance(b, dict) and b.get("cache_control") == EPH for b in c)


def system_marked(one_pass: dict) -> bool:
    s = one_pass["body"].get("system")
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
    os.environ[KEY_VAR] = KEY  # the creds store reads it at boot (env backend)
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    mA = VllmMock(model=MODEL).start()
    mB = AnthropicMock(model=MODEL).start()  # the marked chat: the anthropic wire shows cache_control
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def passes_at_least(m, n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.passes()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.passes()

    def idle(aid: str, bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if hk.quiet(aid):
                return True
            time.sleep(0.05)
        return False

    def history(aid: str) -> list[dict]:
        h = hk.send(aid, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    def run(m, aid: str, label: str, text: str, want: int) -> list[dict]:
        out = hk.send(aid, {"type": "send", "text": text})
        need(not out.get("error"), f"{label}: the send is accepted: {str(out)[:200]}")
        passes_at_least(m, want)
        need(idle(aid), f"{label}: the chain settles")
        time.sleep(0.5)
        passes = m.passes()
        need(len(passes) == want, f"{label}: {want} requests: {len(passes)}")
        return passes

    def big_rows(aid: str) -> list[dict]:
        return [r for r in history(aid)
                if r.get("role") == "user" and int(r.get("bytes") or 0) > 64000]

    def call(target: str, payload: str) -> str:
        return (f'<call name="send"><routing><target_id>{target}</target_id></routing>'
                f'<payload>{payload}</payload></call>')

    # the TEXT of the docs (`part` docs) of the bigdoc instance; the default `all` also
    # carries every verb's schema (#719)
    docs_detailed = call(BIGDOC, "<type>reflect</type><tier>detailed</tier><part>docs</part>")
    read = call(FILES, "<type>read</type><path>big.txt</path>")
    allb: list[dict] = []
    anchors: list[tuple[str, str, int]] = []  # (label, anchor, index in allb of first sight)
    try:
        # ── wiring ────────────────────────────────────────────────────────────
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."])
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module=BUNDLE, id=BIGDOC)
        need(rec.get("id") == BIGDOC, f"create {BIGDOC} (the bigdoc bundle): {str(rec)[:200]}")
        # a call line on `survey`: a reflect documents only the verbs its caller holds,
        # and `reflect` passes on any hold
        can = {"memory": ALL, FILES: ALL, BIGDOC: line_call(VERB)}
        mB.require_key(KEY)
        r = hk.send(root, {"type": "update_agent", "id": "creds",
                           "refs": {"anthropic": {"host": mB.host,
                                                  "env": {"api_key": KEY_VAR}}}})
        need(r.get("updated") is True, f"creds: the ref: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV_A,
                        endpoint=mA.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV_A, f"create {PROV_A}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="anthropic_backend.tools", id=PROV_B,
                        endpoint=mB.url, credentials={"store": "creds", "ref": "anthropic"},
                        model=MODEL, context_window=262144,
                        memory_store_id="memory", can={"memory": ALL, "creds": GET})
        need(rec.get("id") == PROV_B, f"create {PROV_B}: {str(rec)[:200]}")
        for aid, prov, mode in ((AI_A, PROV_A, "none"), (AI_B, PROV_B, "provider_cache")):
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                            provider_id=prov, model=MODEL, max_tokens=8192,
                            context_window=262144, memory_store_id="memory",
                            caching=mode, can={**can, prov: PROVIDER})
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        # ══ A0 · a key read with no line on the store: the unknown answer ═══════
        mA.reply(call("creds", "<type>get</type><ref>anthropic</ref>"), f"Noted.\n{DONE}")
        passes = run(mA, AI_A, "A0", "read the key", 2)
        reply = last_user(passes[1]) if len(passes) > 1 else ""
        # the reply as the model reads it: one tag per field (tool_parse/render.rs)
        need(" creds get" in reply and "<error>no agent 'creds'</error>" in reply
             and "<reason>unknown_agent</reason>" in reply
             and not any(f"<{k}>" in reply for k in ("side", "hint", "ids"))
             and KEY not in reply,
             f"A0: {AI_A} (no line on creds) gets the unknown answer, never the key: {reply[:300]!r}")

        # ══ PART B · caching: provider_cache ══════════════════════════════════
        def collect(passes: list[dict]) -> None:
            allb.extend(passes)

        def first_seen(label: str, pattern: str, passes: list[dict], base: int) -> str:
            """The anchor of `pattern` in request 1 of `passes`; remembered for B4."""
            m = re.search(pattern, full(passes[0])) if passes else None
            need(m is not None, f"{label}: request 1 shows the anchor /{pattern[:60]}…/")
            if m:
                anchors.append((label, m.group(0), base))
                return m.group(0)
            return ""

        # B1a · the read reply is whole the pass it arrives
        mB.reply(read, f"Got it.\n{DONE}")
        passes = run(mB, AI_B, "B1a", "read big.txt", 2)
        base = len(allb)
        collect(passes)
        need(len(passes) > 1 and "L100 " in last_user(passes[1]) and NEEDLE in last_user(passes[1]),
             "B1a: request 2 carries the whole read reply")
        big = big_rows(AI_B)
        need(len(big) == 1, f"B1a: one large user row: {[r.get('bytes') for r in history(AI_B)]}")
        if not big:
            raise SystemExit(verdict(False, f"{RUNTIME}: no large row to fold"))
        b = big[0]["id"]
        whole_marks = [("B1a req2", base + 1)] if len(passes) > 1 else []
        plain_marks = [("B1a req1", base)]

        # B1b · the read reply is its anchor; a large docs reply is whole in its pass
        mB.clear()
        mB.reply(docs_detailed, f"Noted.\n{DONE}")
        passes = run(mB, AI_B, "B1b", "docs", 2)
        base = len(allb)
        collect(passes)
        anchor_read = first_seen(
            "B1b", rf'<folded id="{b}" verb="read" from="files" tokens="\d+" head="[^"]*"/>[^\n]*',
            passes, base)
        need(bool(passes) and "L100 " not in full(passes[0]), "B1b: the read reply is folded")
        last = last_user(passes[1]) if len(passes) > 1 else ""
        need(f" {BIGDOC} reflect" in last and "<tier>detailed</tier>" in last and "S090 " in last,
             f"B1b: request 2 carries the whole reflect part=docs reply of {BIGDOC}: {last[:200]!r}")
        big = [r for r in big_rows(AI_B) if r["id"] != b]
        need(len(big) == 1, f"B1b: one new large user row: {[r.get('bytes') for r in history(AI_B)]}")
        if not big:
            raise SystemExit(verdict(False, f"{RUNTIME}: no docs row to fold"))
        d = big[0]["id"]
        # a reflect never folds, so nothing in this request will fold: the last is marked
        plain_marks.append(("B1b req2", base + 1))

        # B2 · recall by row id gives the docs body back
        mB.clear()
        mB.reply(call(AI_B, f"<type>recall</type><id>{d}</id>"), f"Noted.\n{DONE}")
        passes = run(mB, AI_B, "B2", "recall", 2)
        base = len(allb)
        collect(passes)
        need(bool(passes) and "S090 " in full(passes[0]) and f'<folded id="{d}"' not in full(passes[0]),
             "B2: the reflect reply stays whole (a reflect never folds)")
        last = last_user(passes[1]) if len(passes) > 1 else ""
        need(f" {AI_B} recall" in last and "S090 " in last,
             f"B2: request 2 carries the recalled row whole: {last[:200]!r}")
        whole_marks.append(("B2 req2", base + 1))

        # B3 · grep finds a word inside a folded body; the body stays folded
        mB.clear()
        mB.reply(call(AI_B, f"<type>grep</type><query>{NEEDLE}</query>"), f"Noted.\n{DONE}")
        passes = run(mB, AI_B, "B3", "grep", 2)
        base = len(allb)
        collect(passes)
        first_seen("B3", rf'<folded id="\d+" verb="recall" from="{AI_B}" recalled="id {d}" tokens="\d+"/>[^\n]*',
                   passes, base)
        need(bool(passes) and full(passes[0]).count("S090 ") == 1,
             "B3: the recall reply refolds (only the whole reflect reply still carries the body)")
        need(any(r.get("id") == d and int(r.get("bytes") or 0) > 64000 for r in history(AI_B)),
             "B3: the store row is still whole")
        last = last_user(passes[1]) if len(passes) > 1 else ""
        need(f" {AI_B} grep" in last and f"L050 {NEEDLE}" in last,
             f"B3: the grep hit is inside the folded body: {last[:300]!r}")
        need(len(passes) > 1 and anchor_read in full(passes[1]) and "L100 " not in full(passes[1]),
             "B3: the read body is still folded in the view")
        plain_marks.append(("B3 req2", base + 1))

        # B4 · the cache holds
        mB.clear()
        mB.reply(DONE)
        passes = run(mB, AI_B, "B4", "again", 1)
        base = len(allb)
        collect(passes)
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
