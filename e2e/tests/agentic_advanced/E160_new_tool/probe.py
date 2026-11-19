"""agentic_advanced/new_tool — the model writes a NEW agent and uses it as a tool (#727).

THE CLAIM. Tools are not a fixed set: given a need no agent meets, a model can author
an agent at runtime, grant it, and call its verb as a new tool.

THE FIXTURE. Nothing that computes a digest: the AI holds the kernel verbs that make
an agent (`reflect`, `docs`, `reload_bundle`, `create_agent`, `update_agent` on the
root), a file agent `bundles` serving the kernel's `.fantastic/bundles/` folder, and
the human (`user`). No exec, no `$subtree`, no `*` on the root. The task is the plain
need — a verb that computes a SHA-256 hex digest, which no agent has — and a token
minted at test time, so the digest cannot be known without running code.

THE ROUTE the free twin (`new_tool_twin`, E159) scripts: write a py_agent folder bundle
(`bundle.yaml` with `{doc, schema}` verbs + its program) through `bundles`,
`reload_bundle`, `create_agent`, grant both halves of the gate, `docs` the new agent
(part=schema, #719), call the new verb, answer with its value. The model is told none
of it.

SCORED (hard):
  1. RECORD   — an agent created during the run whose bundle is a py_agent folder
                bundle (`reflect bundles=all`: base py_agent, no error);
  2. VERB     — the model called a verb that bundle declares (beside the base's own),
                on that agent, and its reply carried the digest the harness computes;
  3. ANSWER   — the model's own turns carry the digest;
  4. GRANTS   — exact (`grant_faults`): the new agent admits the AI for that verb and
                the AI's `can_call` reaches it for that verb; nothing on the new agent
                grants `*` (beside its own `$self`) and nothing the run wrote into the
                AI's `can_call` grants `*`.
Reported, not scored: `reload_bundle`, a `docs` of the new agent before its first
call (and with `part=schema`), turns used.

`score` is the one scoring: the free twin runs it on its scripted run too, so a
perfect actor is proven to pass it.
"""

from __future__ import annotations

import asyncio
import hashlib
import os
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, verdict
from helpers.kernel import home, open_kernel

AID = "ai"
BUNDLES = "bundles"
TOKEN = "E160-" + secrets.token_hex(6)
DIGEST = hashlib.sha256(TOKEN.encode("utf-8")).hexdigest()

TASK = (
    "You need a verb that computes the SHA-256 hex digest of a text; no such agent "
    f"exists in this kernel. Get the SHA-256 of `{TOKEN}` through that verb and answer "
    "with the digest."
)


def _calls(transcript: list[str]) -> list[tuple[str, dict]]:
    """Every call the model made, in order, as (target, payload)."""
    from ai_core.tool_parse import extract_tool_calls

    out: list[tuple[str, dict]] = []
    for turn in transcript:
        for c in extract_tool_calls(turn or ""):
            a = c.get("arguments") or {}
            payload = a.get("payload") if isinstance(a.get("payload"), dict) else {}
            out.append((str(a.get("target_id") or ""), payload))
    return out


def _star(v) -> bool:
    return v == "*" or (isinstance(v, list) and "*" in v)


def grant_faults(before: dict, records: dict, ai: str, new: str, verb: str) -> list[str]:
    """What is wrong with the grants between `ai` and the agent it made, `new` — []
    when both halves of the gate name `verb` and no `*` was granted.

    `before`/`records` are `list_agents` by id, before and after the run.
    INGRESS: `new`'s `callable_by` admits `ai` (by id, or as `$parent` when `ai` is its
    parent) with a verb list holding `verb`. EGRESS: `ai`'s `can_call` reaches `new`
    (by id, or as `$subtree` when it is `ai`'s child) with a verb list holding `verb`.
    A fault: any `*` on `new`'s `callable_by` but its own `$self` baseline, and any
    `*` in `ai`'s `can_call` that was not there before the run. A `*` KEY is a
    literal id to the kernel (it admits nobody) and is a fault on either side.
    """
    out: list[str] = []
    rec, me = records.get(new) or {}, records.get(ai) or {}
    was = (before.get(ai) or {}).get("can_call") or {}
    child = rec.get("parent_id") == ai
    cb = rec.get("callable_by") or {}
    cc = me.get("can_call") or {}
    for k, v in cb.items():
        if k == "*" or (k != "$self" and _star(v)):
            out.append(f"{new}.callable_by[{k!r}] = {v!r}: a '*' grant")
    for k, v in cc.items():
        if (k == "*" or _star(v)) and was.get(k) != v:
            out.append(f"{ai}.can_call[{k!r}] = {v!r}: a '*' grant written in the run")
    admits = [cb[k] for k in (ai, "$parent" if child else None) if k and k in cb]
    if not any(isinstance(v, list) and verb in v for v in admits):
        out.append(f"{new}.callable_by does not admit {ai} for {verb!r}: {cb!r}")
    reach = [cc[k] for k in (new, "$subtree" if child else None) if k and k in cc]
    if not any(isinstance(v, list) and verb in v for v in reach):
        out.append(f"{ai}.can_call does not reach {new} for {verb!r}: {reach!r}")
    return out


def score(*, ai: str, digest: str, before: dict, records: dict, turns: list[str],
          held: list[dict], catalog: list[dict], base: set, schemas: dict) -> dict:
    """The four scored checks, from what the run left behind.

    `turns` are the model's own turns; `held` the AI's rows; `catalog` the
    `reflect bundles=all` rows; `base` py_agent's own verbs; `schemas` the verbs
    `docs part=schema` lists for each agent the run created. The pair scored is the
    first (agent, declared verb) the model called whose reply carried the digest —
    else the first one called — so a failed first attempt does not hide a later one.
    """
    calls = _calls(turns)
    folders = {b.get("name") for b in catalog
               if b.get("base") == "py_agent" and "error" not in b}
    made = [i for i, rec in records.items()
            if i not in before and rec.get("handler_module") in folders]
    declared = {n: sorted(set(schemas.get(n) or ()) - base) for n in made}
    tried = list(dict.fromkeys((t, p.get("type")) for t, p in calls
                               if t in declared and p.get("type") in declared[t]))

    def carried(n: str, v: str) -> bool:
        return any(r.get("role") == "user" and f"{n} {v}" in str(r.get("content"))
                   and digest in str(r.get("content")) for r in held)

    hit = next((nv for nv in tried if carried(*nv)), None)
    new, verb = hit or (tried[0] if tried else ((made[0] if made else ""), ""))
    faults = (grant_faults(before, records, ai, new, verb) if verb
              else ["no verb of it was called"])
    first = next((i for i, (t, p) in enumerate(calls) if t == new and p.get("type") == verb),
                 len(calls))
    out = {
        "calls": calls,
        "made": made,
        "declared": declared,
        "new": new,
        "verb": verb,
        "replied": hit is not None,
        "answered": digest in "\n".join(turns),
        "faults": faults,
        "docs_first": [p for i, (t, p) in enumerate(calls)
                       if i < first and p.get("type") == "docs" and p.get("of") == new],
        "reloads": [p.get("name") for t, p in calls if p.get("type") == "reload_bundle"],
    }
    out["ok"] = bool(made) and out["replied"] and out["answered"] and not faults
    return out


async def main():
    bk = _resolve_backend()
    root, core, close_kernel = await open_kernel(
        "new_tool_", reach=[BUNDLES, AID, "cli", "user"]
    )

    async def k(target: str, payload: dict) -> dict:
        r = (await root.send([{"target": target, "payload": payload}]))[0]["result"]
        return r if isinstance(r, dict) else {"_reply": r}

    # the kernel's bundles folder, served by a file agent (cwd is the kernel's dir)
    Path(".fantastic/bundles").mkdir(parents=True, exist_ok=True)
    for payload in (
        {
            "type": "create_agent",
            "parent_id": home(root),
            "handler_module": "file_bridge.tools",
            "id": BUNDLES,
            "root": ".fantastic/bundles",
            "allow": ["."],
            "ingress_rule": "allow_all_ingress",
            "description": "This kernel's bundles folder, .fantastic/bundles/.",
            "callable_by": {AID: "*", "rest": "*", "web": "*"},
        },
        {
            "type": "create_agent",
            "parent_id": home(root),
            "handler_module": "user_probe.tools",
            "id": "user",
            "answer": "use your own judgement",
            "description": "The human this kernel belongs to.",
            "can_call": {AID: "*"},
            "callable_by": {AID: "*", "rest": "*", "web": "*"},
        },
    ):
        r = await k(root.id, payload)
        assert not r.get("error"), f"create {payload['id']}: {r}"

    conn = await connect(
        root,
        core,
        bk,
        agent_id=AID,
        can_call={
            root.id: ["reflect", "docs", "reload_bundle", "create_agent", "update_agent"],
            BUNDLES: "*",
            "user": "*",
        },
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
    )

    async def agents() -> dict:
        r = await k("kernel", {"type": "list_agents"})
        return {a.get("id"): a for a in r.get("agents") or []}

    async def verbs_of(of: str) -> set:
        d = await k("kernel", {"type": "docs", "of": of, "part": "schema"})
        return set(d.get("schemas") or {})

    before = await agents()

    capped = await drive(conn, TASK, client_id="new_tool", via="user")
    turns = list(conn.proxy.transcript)
    held = (await k(AID, {"type": "rows"})).get("rows") or []
    records = await agents()
    cat = await k("kernel", {"type": "reflect", "tree": "none", "bundles": "all"})
    base = await verbs_of("py_agent")
    schemas = {i: await verbs_of(i) for i in records if i not in before}
    await conn.close()
    await close_kernel()

    s = score(ai=AID, digest=DIGEST, before=before, records=records, turns=turns,
              held=held, catalog=cat.get("bundles") or [], base=base, schemas=schemas)
    new, verb, faults = s["new"], s["verb"], s["faults"]
    print(f"\n  calls: {len(s['calls'])} · turns: {conn.turns} · cap: {capped or 'clean'}")
    print(f"  made from a folder bundle: {s['made']} · declared: {s['declared']}")
    print(f"  reload_bundle: {s['reloads']}")
    print(f"  docs of {new or '-'} before its first call: "
          f"{[p.get('part', 'all') for p in s['docs_first']] or 'none'}")
    print(f"  grants: {faults or 'exact'}")
    print(f"\n  1. a new agent of a py_agent folder bundle : {'OK' if s['made'] else 'NO'}")
    print(f"  2. its declared verb called, digest replied: {'OK' if s['replied'] else 'NO'}"
          f" ({new or '-'}.{verb or '-'})")
    print(f"  3. the digest in the model's answer        : {'OK' if s['answered'] else 'NO'}")
    print(f"  4. grants exact, no '*'                    : {'OK' if not faults else 'NO'}")
    verdict(
        s["ok"],
        f"wrote {new}, called {verb}, answered the digest, grants exact"
        if s["ok"]
        else f"made={s['made']} verb={verb or None} replied={s['replied']} "
        f"answered={s['answered']} grants={faults[:2]}",
    )


if __name__ == "__main__":
    asyncio.run(main())
