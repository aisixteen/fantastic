"""kernel/autofold — autofold (#556) against a SCRIPTED model. NO MODEL.

A local OpenAI-compatible server plays the model; the kernel's own AI agent (on both runtimes: an
`ai_chat.tools` chat naming a `vllm_backend` provider) on
the PACKAGED rust host runs the chains, driven through `web_rest`,
next to a `file_bridge` agent (`files`) holding an 80 kB file.

  P1. the AI reads the file (thought, then a `read` call); the reply is whole in the
      next request; the AI answers `done`;
  P2. the next send: the read reply is its tag line and the default type's `<folded …/>`
      marker, and both thoughts are `<thought …/>` markers on the next user-side
      messages (none of their text); the AI then recalls row B by id and gets the body
      whole;
  P2b. map-once: the read reply's marker in that request is the type's template
      (`res/default/marker_folded_reply.txt`) filled with the stored row's facts, byte for
      byte, its unfold call naming row B;
  P3. after the next answer the recall reply is a `<folded … verb="recall" … recalled="id B"`
      marker; the store row B is still whole;
  P4. a malformed answer after thinking (a foreign `<invoke>` and 50 closers) is
      re-asked inside its turn (#766): request 2 is request 1's messages, then the
      attempt without its thought, read no further than the delta that held the
      foreign tag, and the report; the store holds neither;
  P5. the next send: no request holds the failed attempt, its thought or its report.
"""

from __future__ import annotations

import json
import math
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
from helpers.lines import ALL, PROVIDER
from helpers.verdict import verdict
from mocks.vllm import ThinkingVllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, FILES, PROV = "ai", "files", "ai_prov"
MODEL = "mock-scripted"
TOKEN = "E141-" + secrets.token_hex(4)
DONE = '<call name="done"></call>'
CUT = ('<think>CUT-' + TOKEN + '</think>Sending it.\n<invoke name="send">\n'
       + '</invoke>\n' * 50)


def last_user(req: dict) -> str:
    """The last user message of a chat request, as text."""
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""




#: the default type's marker templates (map-once)
TEMPLATES = (REPO / "src/lib/rust/crates/bundles/fantastic-ai-core/src/model_format/res/default")


def quote(s: str) -> str:
    """A value as one quoted attribute of the default type's notation."""
    return '"' + (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
                  .replace('"', "&quot;")) + '"'


def fill(name: str, own: str, facts: dict[str, str]) -> str:
    """A marker template filled as the map fills it: `{self}`, every fact as `{k}` and,
    quoted, as `{k_q}`, one pass; its trailing newline dropped."""
    text = (TEMPLATES / name).read_text()
    text = text[:-1] if text.endswith("\n") else text
    vals = {"self": own, "self_q": quote(own)}
    for k, v in facts.items():
        vals[k] = v
        vals[k + "_q"] = quote(v)
    out, i = [], 0
    while (j := text.find("{", i)) >= 0:
        out.append(text[i:j])
        end = text.find("}", j)
        key = text[j + 1:end] if end > j else ""
        if key in vals:
            out.append(vals[key])
            i = end + 1
        else:
            out.append("{")
            i = j + 1
    out.append(text[i:])
    return "".join(out)


def messages_of(turn: dict) -> list[dict]:
    return turn["body"].get("messages") or []


def blob(turn: dict) -> str:
    return json.dumps(messages_of(turn), ensure_ascii=False)


def user_texts(turn: dict) -> list[str]:
    return [str(m.get("content")) for m in messages_of(turn) if m.get("role") == "user"]


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = ThinkingVllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-autofold-"))
    (work / "files").mkdir()
    # 100 lines of 800 characters: the `read` reply (default 200 lines) is over 64,000.
    (work / "files" / "big.txt").write_text(
        "".join(f"L{n:03d} " + "x" * 795 + "\n" for n in range(1, 101)))
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def turns_at_least(n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.turns()

    def idle(bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def history() -> list[dict]:
        h = hk.send(AI, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    def run(label: str, text: str, want: int) -> list[dict]:
        out = hk.send(AI, {"type": "send", "text": text})
        need(not out.get("error"), f"{label}: the send is accepted: {str(out)[:200]}")
        turns_at_least(want)
        need(idle(), f"{label}: the chain settles")
        time.sleep(0.5)
        turns = m.turns()
        need(len(turns) == want, f"{label}: {want} requests: {len(turns)}")
        return turns

    read = (f'<call name="send"><routing><target_id>{FILES}</target_id></routing>'
            '<payload><type>read</type><path>big.txt</path></payload></call>')
    try:
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."])
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        can = {"memory": ALL, FILES: ALL}
        # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it:
        # a `send` to a provider is refused (#521)
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can={**can, PROV: PROVIDER})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        # ── P1 · the read reply is whole the turn it arrives ──────────────────
        m.reply(f"<think>PLAN-{TOKEN}</think>Reading.\n{read}",
                f'<think>SEEN-{TOKEN}</think>Got it.\n{DONE}')
        turns = run("P1", "read big.txt", 2)
        need(len(turns) > 1 and "L100 " in user_texts(turns[1])[-1],
             "P1: request 2 carries the whole read reply")
        big = [r for r in history() if r.get("role") == "user"
               and int(r.get("bytes") or 0) > 64000]
        need(len(big) == 1, f"P1: the store holds the read reply whole: "
             f"{[r.get('bytes') for r in history()]}")
        if not big:
            raise SystemExit(verdict(False, f"{RUNTIME}: no large row to fold"))
        b = big[0]["id"]

        # ── P2 · folded in the next request; recall by id gives it back ───────
        m.clear()
        m.reply(f'<call name="send"><routing><target_id>{AI}</target_id></routing>'
                f'<payload><type>recall</type><id>{b}</id></payload></call>',
                f'Noted.\n{DONE}')
        turns = run("P2", "next", 2)
        seen = blob(turns[0]) if turns else ""
        users = user_texts(turns[0]) if turns else []
        need(any(f'<folded id="{b}" verb="read" from="{FILES}"' in t for t in users),
             "P2: request 1 shows the read reply as its marker")
        need(sum(t.count("<thought id=") for t in users) == 2,
             "P2: request 1 shows both thoughts as markers on user-side messages")
        need(all("<thought id=" not in str(x.get("content"))
                 for x in (messages_of(turns[0]) if turns else [])
                 if x.get("role") == "assistant"),
             "P2: no assistant message holds a marker")
        need(f"PLAN-{TOKEN}" not in seen and f"SEEN-{TOKEN}" not in seen and "L100 " not in seen,
             "P2: request 1 holds no thought text and no body")
        last = user_texts(turns[1])[-1] if len(turns) > 1 else ""
        need(" ai recall" in last and "L100 " in last,
             "P2: request 2 carries the recalled row whole")

        # ── P2b · the marker is the type's template (map-once) ────────────────
        held = hk.send(AI, {"type": "rows", "limit": 500}).get("rows") or []
        row = next((r for r in held if r.get("id") == b), {})
        content = str(row.get("content") or "")
        tag_line, _, rest = content.partition("\n")
        want = fill("marker_folded_reply.txt", AI, {
            "tag_line": tag_line, "id": str(b), "agent": FILES, "verb": "read",
            "tokens": str(math.ceil(len(rest) / 4)),
            "head": rest.split("\n", 1)[0].strip()[:120], "what": "", "kept": "",
        })
        need(bool(content) and any(want in t for t in users),
             f"P2b: the read reply's marker is the template, byte for byte: {want[:160]!r}")
        need(f"<id>{b}</id></payload></call>" in want,
             "P2b: its unfold call is `recall` with row B's id")

        # ── P3 · the recall reply folds after the next answer ─────────────────
        m.clear()
        m.reply(DONE)
        turns = run("P3", "again", 1)
        seen = blob(turns[0]) if turns else ""
        need(any(f'verb="recall" from="{AI}" recalled="id {b}"' in t
                 for t in user_texts(turns[0]) if turns),
             "P3: the recall reply is its marker")
        need("L100 " not in seen, "P3: the body is nowhere in the request")
        need(any(r.get("id") == b and int(r.get("bytes") or 0) > 64000 for r in history()),
             "P3: the store row is still whole")

        # ── P4 · a malformed answer is re-asked inside its turn (#766) ────────
        m.clear()
        m.reply(CUT, DONE)
        turns = run("P4", "cut it", 2)
        pairs = [[(x.get("role"), x.get("content")) for x in messages_of(t)] for t in turns]
        pairs += [[], []]
        need(len(pairs[1]) == len(pairs[0]) + 2 and pairs[1][:-2] == pairs[0],
             "P4: request 2 is the re-ask: request 1's messages unchanged, then one pair")
        att = str(pairs[1][-2][1]) if len(pairs[1]) >= 2 else ""
        rpt = str(pairs[1][-1][1]) if len(pairs[1]) >= 2 else ""
        need(len(pairs[1]) >= 2 and pairs[1][-2][0] == "assistant"
             and att.startswith('Sending it.\n<invoke name="send">'),
             f"P4: the re-ask pair holds the attempt as written: {att[:120]!r}")
        need(att.count("</invoke>") < 50,
             f"P4: the attempt is read no further than the foreign tag's delta: "
             f"{att.count('</invoke>')} of 50 closers")
        need(pairs[1][-1][0] == "user" and rpt.startswith("[malformed call — nothing ran]"),
             f"P4: the re-ask's last user message is the report: {rpt[:120]!r}")
        need(f"CUT-{TOKEN}" not in (blob(turns[1]) if len(turns) > 1 else ""),
             "P4: the failed attempt's thought is not in the re-ask")
        rows = [r for r in history() if r.get("role") == "assistant"]
        need(not any("<invoke" in str(r.get("head")) or "Sending it." in str(r.get("head"))
                     for r in rows)
             and not any("[malformed call" in str(r.get("head")) for r in history()),
             "P4: the store holds neither the failed attempt nor its report")

        # ── P5 · the failed attempt is in no later request ────────────────────
        m.clear()
        m.reply(DONE)
        turns = run("P5", "after", 1)
        seen = json.dumps([x for x in (messages_of(turns[0]) if turns else [])
                           if x.get("role") != "system"], ensure_ascii=False)
        need("Sending it." not in seen and "<invoke" not in seen
             and "[malformed call" not in seen and f"CUT-{TOKEN}" not in seen,
             "P5: the next request holds no byte of the failed attempt, its thought or report")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
