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
  4. GRANTS   — exact (`grant_faults`, shared with the twin): the new agent admits the
                AI for that verb and the AI's `can_call` reaches it for that verb, and
                neither side grants `*`.
Reported, not scored: `reload_bundle`, a `docs` of the new agent before its first
call (and with `part=schema`), turns used.
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
from tests.agentic_advanced.E159_new_tool_twin.probe import grant_faults

BK = _resolve_backend()
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


async def main():
    root, core, close_kernel = await open_kernel(
        "new_tool_", reach=[BUNDLES, AID, "kernel_store", "cli", "user"]
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
        BK,
        agent_id=AID,
        can_call={
            root.id: ["reflect", "docs", "reload_bundle", "create_agent", "update_agent"],
            BUNDLES: "*",
            "user": "*",
        },
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
    )
    before = {a.get("id") for a in (await k("kernel", {"type": "list_agents"})).get("agents") or []}

    capped = await drive(conn, TASK, client_id="new_tool", via="user")
    calls = _calls(conn.proxy.transcript)
    said = "\n".join(conn.proxy.transcript)
    held = (await k(AID, {"type": "rows"})).get("rows") or []
    records = {a.get("id"): a for a in (await k("kernel", {"type": "list_agents"})).get("agents") or []}
    cat = await k("kernel", {"type": "reflect", "tree": "none", "bundles": "all"})
    folders = {
        b.get("name"): b
        for b in cat.get("bundles") or []
        if b.get("base") == "py_agent" and "error" not in b
    }
    base = set(((await k("kernel", {"type": "docs", "of": "py_agent", "part": "schema"}))
                .get("schemas") or {}))

    # the agents the run made from a py_agent folder bundle
    made = [i for i, rec in records.items()
            if i not in before and rec.get("handler_module") in folders]
    best: tuple[str, str] | None = None
    declared: dict[str, list[str]] = {}
    for new in made:
        d = await k("kernel", {"type": "docs", "of": new, "part": "schema"})
        declared[new] = sorted(set(d.get("schemas") or {}) - base)
        for t, p in calls:
            if t == new and p.get("type") in declared[new]:
                best = best or (new, p["type"])
    await conn.close()
    await close_kernel()

    new, verb = best or (made[0] if made else "", "")
    replied = bool(verb) and any(
        r.get("role") == "user" and f"{new} {verb}" in str(r.get("content"))
        and DIGEST in str(r.get("content"))
        for r in held
    )
    answered = DIGEST in said
    faults = grant_faults(records, AID, new, verb) if verb else ["no verb of it was called"]
    first = next((i for i, (t, p) in enumerate(calls) if t == new and p.get("type") == verb),
                 len(calls))
    docs_first = [p for i, (t, p) in enumerate(calls)
                  if i < first and p.get("type") == "docs" and p.get("of") == new]
    reloads = [p.get("name") for t, p in calls if p.get("type") == "reload_bundle"]

    print(f"\n  calls: {len(calls)} · turns: {conn.turns} · cap: {capped or 'clean'}")
    print(f"  made from a folder bundle: {made} · declared: {declared}")
    print(f"  reload_bundle: {reloads}")
    print(f"  docs of {new or '-'} before its first call: "
          f"{[p.get('part', 'all') for p in docs_first] or 'none'}")
    print(f"  grants: {faults or 'exact'}")
    print(f"\n  1. a new agent of a py_agent folder bundle : {'OK' if made else 'NO'}")
    print(f"  2. its declared verb called, digest replied: {'OK' if replied else 'NO'}"
          f" ({new or '-'}.{verb or '-'})")
    print(f"  3. the digest in the model's answer        : {'OK' if answered else 'NO'}")
    print(f"  4. grants exact, no '*'                    : {'OK' if not faults else 'NO'}")
    ok = bool(made) and replied and answered and not faults
    verdict(
        ok,
        f"wrote {new}, called {verb}, answered the digest, grants exact"
        if ok
        else f"made={made} verb={verb or None} replied={replied} answered={answered} "
        f"grants={faults[:2]}",
    )


if __name__ == "__main__":
    asyncio.run(main())
