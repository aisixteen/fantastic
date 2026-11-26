"""agentic_advanced/new_tool_twin — an AI writes a NEW agent and uses it as a tool. NO MODEL.

The free twin of `new_tool` (E160): a local OpenAI-compatible server
(`e2e/mocks/vllm.py`) plays the model from a fixed script, and the kernel's own
`ai_chat.tools` chat, completing through a `vllm_backend` provider, runs the chain on
the PACKAGED rust host, every test call through `web_rest`. The script is the call
sequence a model has to find on its own in E160, one answer per request:

  1. three `write`s to the `bundles` file agent (rooted at `.fantastic/bundles/`):
     `sha_tool/bundle.yaml` (`base: py_agent`, one verb `sha256` in the
     `{doc, schema}` form), `sha_tool/agent.py` (the program), and the reference
     helper `fantastic_agent.py` beside it;
  2. `kernel` `reload_bundle name=sha_tool` — the folder written after boot loads
     (the AI's line on the root names the root loader's verb);
  3. `kernel` `create_agent handler_module=sha_tool parent_id=ai id=sha`, with no
     line field: the AI's `create` line on itself names `sha_tool`, and the new
     agent is in its tree, reached by position;
  4. `kernel` `update_agent id=ai` — the AI widening its OWN `can` with a line on
     `sha`: refused, `not_allowed` naming `can` (an agent never writes its own `can`);
  5. `sha` `reflect part=schema` (#719);
  6. `sha` `sha256 text=<token>` — the new verb, called;
  7. the answer, carrying the digest, and `done`.

CHECKS — each request carries the reply its answer asked for, and:
  · the files landed verbatim; the folder is listed (`reflect rest bundles=all`) as
    a py_agent bundle with no error;
  · the agent record exists (`reflect`): handler_module `sha_tool`, parent `ai`;
  · the self-widening is `not_allowed` naming `can`, and the AI's `can` is unchanged;
  · the verb is declared (`reflect sha part=schema`: `sha256` takes `text`), and the
    reflect reply the AI got carries that schema;
  · the verb was called and its reply carries the SHA-256 the harness computes;
  · the AI's stored answer carries the digest; no request follows `done`;
  · the lines are exact (E160's `grant_faults`): the AI's `can` is the one it
    started with, the new agent's `can` holds no `*`, and it is under the AI; the
    same scorer flags a `*` planted on `sha` and a widened line on the AI;
  · E160's own `score` passes this run — the model test's scoring is proven passable
    by a perfect actor, and a free run catches it drifting.
"""

from __future__ import annotations

import hashlib
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
from helpers.host.wire import WireCtx
from helpers.lines import ALL, PROVIDER, ROOT, call as line_call, create as line_create
from helpers.verdict import verdict
from mocks.vllm import VllmMock
from tests.agentic_advanced.E160_new_tool.probe import folder_verbs, grant_faults, score

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, BUNDLES, PROV = "ai", "bundles", "ai_prov"
BUNDLE, NEW, VERB = "sha_tool", "sha", "sha256"
MODEL = "mock-scripted"
TOKEN = "E162-" + secrets.token_hex(6)
DIGEST = hashlib.sha256(TOKEN.encode("utf-8")).hexdigest()
HELPER = (REPO / "src/lib/python/bundled_agents/io/py_agent/src/py_agent"
          / "fantastic_agent.py")

BUNDLE_YAML = f"""base: py_agent
sentence: Computes the SHA-256 hex digest of a text.
command: [python3, agent.py]
record:
  inherit_env: [PATH]
verbs:
  {VERB}:
    doc: "The SHA-256 hex digest of `text` (UTF-8). Returns {{{VERB}}}."
    schema: {{type: object, properties: {{text: {{type: string}}}}, required: [text]}}
"""

AGENT_PY = f"""import hashlib

import fantastic_agent as fa


def {VERB}(p):
    return {{"{VERB}": hashlib.sha256(str(p.get("text", "")).encode("utf-8")).hexdigest()}}


fa.serve({{"{VERB}": {VERB}}})
"""


def xml(v) -> str:
    """A value in the call grammar: a map's keys are tags, a list is `<item>`s."""
    if isinstance(v, dict):
        return "".join(f"<{k}>{xml(x)}</{k}>" for k, x in v.items())
    if isinstance(v, list):
        return "".join(f"<item>{xml(x)}</item>" for x in v)
    return str(v)


def call(target: str, verb: str, **args) -> str:
    body = "".join(f"<{k}>{xml(v)}</{k}>" for k, v in args.items())
    return (f'<call name="send">\n<routing><target_id>{target}</target_id></routing>\n'
            f"<payload><type>{verb}</type>{body}</payload>\n</call>")


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
    m = VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-new-tool-twin-"))
    (work / ".fantastic" / "bundles").mkdir(parents=True)
    # `reload_bundle` is the ROOT loader's verb: the legs' line on the root names it,
    # so `rest` can hand the AI that line. Every agent here is under `rest`.
    hk = HostKernel(spec, work, root_verbs=["reload_bundle"]).boot()
    root = hk.root_id
    ctx = WireCtx(hk)
    bad: list[str] = []
    helper = HELPER.read_text()

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def short(x) -> str:
        return json.dumps(x, default=str)[:300]

    def records() -> dict:
        """Every record `rest` sees, whole for its own tree."""
        return {i: ctx.record(i) for i in ctx.agent_ids()}

    def idle(bound: float = 60) -> bool:
        """Poll (bounded) until the AI has no task in flight (`ps`)."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def turns_at_least(n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if len(m.turns()) >= n:
                break
            time.sleep(0.05)
        return m.turns()

    can = {"memory": ALL, ROOT: line_call("reflect", "reload_bundle"),
           BUNDLES: ALL, PROV: PROVIDER}
    # its `create` line on ITSELF: the one folder bundle the script writes (O15), every
    # field; written once the AI exists
    own = {AI: line_create(what={BUNDLE: {"*": "*"}})}
    files = {f"{BUNDLE}/bundle.yaml": BUNDLE_YAML, f"{BUNDLE}/agent.py": AGENT_PY,
             f"{BUNDLE}/fantastic_agent.py": helper}
    script = [
        # 1 · the bundle folder, three files at once
        "Writing the new agent's bundle.\n" + "\n".join(
            call(BUNDLES, "write", path=p, bytes=b) for p, b in files.items()),
        # 2 · load the folder written after boot
        call("kernel", "reload_bundle", name=BUNDLE),
        # 3 · an instance UNDER the AI, no line field: its tree, reached by position
        call("kernel", "create_agent", handler_module=BUNDLE, parent_id=AI, id=NEW),
        # 4 · the AI widening its OWN can: refused (`not_allowed` naming `can`)
        call("kernel", "update_agent", id=AI,
             can={**can, **own, NEW: line_call(VERB)}),
        # 5 · how to call it (#719)
        call(NEW, "reflect", part="schema"),
        # 6 · the new verb
        call(NEW, VERB, text=TOKEN),
        # 7 · the answer
        f"The SHA-256 of {TOKEN} is {DIGEST}.\n<call name=\"done\"></call>",
    ]
    try:
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=BUNDLES,
                        root=".fantastic/bundles", allow=["."])
        need(rec.get("id") == BUNDLES, f"create {BUNDLES}: {short(rec)}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV, f"create {PROV}: {short(rec)}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192, context_window=262144,
                        memory_store_id="memory", can=can)
        need(rec.get("id") == AI, f"create {AI}: {short(rec)}")
        r = hk.send(root, {"type": "update_agent", "id": AI, "can": {**can, **own}})
        need(r.get("updated") is True, f"the AI's create line on itself: {short(r)}")
        before = records()
        need(NEW not in before, f"no agent {NEW!r} before the run")
        need((before.get(AI) or {}).get("can") == {**can, **own},
             f"the AI holds exactly its lines: {short((before.get(AI) or {}).get('can'))}")

        m.reply(*script)
        out = hk.send(AI, {"type": "send",
                           "text": f"You need a verb that computes the SHA-256 hex digest "
                                   f"of a text; no such agent exists. Answer with the "
                                   f"SHA-256 of {TOKEN}."})
        need(not out.get("error"), f"the send is accepted: {short(out)}")
        turns_at_least(len(script))
        need(idle(), "the chain settles (no AI task left in `ps`)")
        turns = m.turns()
        for i, t in enumerate(turns):
            print(f"  ── request {i + 1}: last user turn ──\n"
                  + "\n".join("     " + ln for ln in last_user(t["body"])[:400].splitlines()),
                  flush=True)
        need(len(turns) == len(script),
             f"one request per scripted answer, none after `done`: {len(turns)} for {len(script)}")
        u = [last_user(t["body"]) for t in turns] + [""] * len(script)

        # 1 · the files
        for p, b in files.items():
            f = work / ".fantastic" / "bundles" / p
            need(f.is_file() and f.read_text() == b, f"{p} landed verbatim")
        need(u[1].count(f"{BUNDLES} write") == 3,
             f"request 2 carries the three write replies: {u[1][:200]!r}")
        # 2 · the reload
        need("kernel reload_bundle" in u[2] and f"<reloaded>{BUNDLE}</reloaded>" in u[2],
             f"request 3 carries the reload reply: {u[2][:200]!r}")
        # `rest`'s own tree: the whole catalog (a `call` line on the root shows none)
        cat = hk.send("rest", {"type": "reflect", "tree": "none", "bundles": "all"})
        row = next((b for b in cat.get("bundles") or [] if b.get("name") == BUNDLE), {})
        need(row.get("base") == "py_agent" and "error" not in row,
             f"the folder is listed as a py_agent bundle: {short(row)}")
        # 3 · the record
        recs = records()
        new = recs.get(NEW) or {}
        need(new.get("handler_module") == BUNDLE and new.get("parent_id") == AI,
             f"the agent record exists: {short(new)}")
        need("kernel create_agent" in u[3] and f"<id>{NEW}</id>" in u[3],
             f"request 4 carries the create reply: {u[3][:200]!r}")
        # 4 · the self-widening: `not_allowed` naming `can`, nothing written
        need("kernel update_agent" in u[4] and "<reason>not_allowed</reason>" in u[4]
             and "<arg>can</arg>" in u[4] and "<updated>" not in u[4],
             f"request 5 carries not_allowed naming can for {AI}: {u[4][:200]!r}")
        need((recs.get(AI) or {}).get("can") == (before.get(AI) or {}).get("can"),
             f"the AI's own can is unchanged: {short((recs.get(AI) or {}).get('can'))}")
        faults = grant_faults(before, recs, AI, NEW, VERB)
        need(not faults, f"the lines are exact, no '*': {faults}")
        # the scorer is not vacuous: a '*' planted on the new agent, and a line the AI
        # widened its own can with, are both faults
        held = recs.get(NEW) or {}
        planted = {**recs, NEW: {**held, "can": {**(held.get("can") or {}), "memory": ALL}}}
        need(bool(grant_faults(before, planted, AI, NEW, VERB)),
             f"grant_faults flags {NEW}.can['memory'] = {ALL!r}")
        me = recs.get(AI) or {}
        widened = {**recs, AI: {**me, "can": {**(me.get("can") or {}), NEW: line_call(VERB)}}}
        need(bool(grant_faults(before, widened, AI, NEW, VERB)),
             f"grant_faults flags {AI}.can widened with {NEW}: {line_call(VERB)!r}")
        # 5 · the verb is declared
        d = hk.send(NEW, {"type": "reflect", "part": "schema"})
        sch = (d.get("schemas") or {}).get(VERB) or {}
        need("text" in (sch.get("properties") or {}),
             f"reflect part=schema declares {VERB}(text): {short(d)}")
        need(f"{NEW} reflect" in u[5] and f"<{VERB}>" in u[5] and "<text>" in u[5],
             f"request 6 carries the reflect reply with the verb's schema: {u[5][:200]!r}")
        # 6 · the verb was called; its answer is the program's
        need(f"{NEW} {VERB}" in u[6] and DIGEST in u[6],
             f"request 7 carries the new verb's reply with the digest: {u[6][:200]!r}")
        # 7 · the answer
        rows = (hk.send(AI, {"type": "rows"}) or {}).get("rows") or []
        need(any(r.get("role") == "assistant" and DIGEST in str(r.get("content"))
                 for r in rows),
             f"the AI's stored answer carries the digest ({len(rows)} rows)")
        # E160's scoring, on this run
        def verbs_of(of: str) -> set:
            return set(hk.send(of, {"type": "reflect", "part": "schema"})
                       .get("schemas") or {})

        s = score(ai=AI, digest=DIGEST, before=before, records=recs,
                  turns=[str(r.get("content")) for r in rows if r.get("role") == "assistant"],
                  held=rows, catalog=cat.get("bundles") or [],
                  bundle_verbs=folder_verbs(cat.get("bundles") or [], work),
                  schemas={i: verbs_of(i) for i in recs if i not in before})
        need(s["ok"] and (s["new"], s["verb"]) == (NEW, VERB) and s["reloads"] == [BUNDLE]
             and [p.get("part") for p in s["docs_first"]] == ["schema"],
             f"E160's score passes the scripted run: "
             f"{short({k: v for k, v in s.items() if k != 'calls'})}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
