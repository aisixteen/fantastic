"""agentic/docs_mechanics — a reflect says what an agent IS; `docs` says how to call it.

THE MECHANICS UNDER TEST (owner, 2026-10-05). A reflect carries no verbs; the
kernel's `docs` verb serves them in two tiers (oneliners: one signature per verb;
detailed: the whole text, for one verb); a reflect's tree can be cut with
`show_subtrees` / `ignore_subtrees`. The system prompt teaches all three — the
task below names none of them.

THE FIXTURE. A ~30-agent estate: four wings, rooms below them, and one VAULT five
levels down (`site / south_wing / basement / archive_room / vault`). The vault's
`open` takes `code:str`; only its DETAILED docs say how the code is made from the
`serial` its reflect shows (backwards, prefixed `V-`), so a model that never pulls
detailed docs for `open` cannot open it. Structural rooms are sealed file legs
with a `description` — readable in the tree, callable by nobody.

SCORED ON THE MODEL'S OWN CALLS (parsed from its turns) and on the FILE:
  1. task done        — `answer.md` on the desk holds the vault's secret;
  2. oneliners first  — a `docs` of the vault (oneliners) before its first `open`;
  3. detailed only for a used verb — every detailed pull names a verb, and that
                        verb is then called;
  4. a filtered reflect after the first look — `show_subtrees` / `ignore_subtrees`.
Reported, not scored: docs pulls by tier, reflects plain vs filtered, and the
share of reflect + docs replies in the conversation the agent holds.
"""

from __future__ import annotations

import asyncio
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

BK = _resolve_backend()
AID = "ai"
SERIAL = str(40000 + secrets.randbelow(50000))
SECRET = "amber-" + secrets.token_hex(3)

TASK = (
    "Somewhere in this kernel's tree there is a vault. Open it, then write the "
    "secret it gives you into `answer.md` through the `desk` file agent — the "
    "secret alone on one line. When the file is written, you are done — stop."
)

#: (id, parent, description) — the estate. `None` parent = the kernel root.
ROOMS = [
    ("site", None, "The estate. Every wing is below it."),
    ("north_wing", "site", "North wing: offices."),
    ("office_1", "north_wing", "Office of the steward."),
    ("office_2", "north_wing", "Office of the clerk."),
    ("office_3", "north_wing", "Empty office."),
    ("east_wing", "site", "East wing: laboratories."),
    ("lab_1", "east_wing", "Chemistry lab."),
    ("bench_a", "lab_1", "Bench A — glassware."),
    ("bench_b", "lab_1", "Bench B — reagents."),
    ("lab_2", "east_wing", "Physics lab."),
    ("west_wing", "site", "West wing: the library."),
    ("stacks", "west_wing", "The book stacks."),
    ("shelf_1", "stacks", "Shelf 1 — history."),
    ("shelf_2", "stacks", "Shelf 2 — maps."),
    ("shelf_3", "stacks", "Shelf 3 — letters."),
    ("south_wing", "site", "South wing: storage."),
    ("basement", "south_wing", "The basement."),
    ("archive_room", "basement", "Archive room — old records, and the vault."),
    ("cabinet_1", "archive_room", "Cabinet 1 — ledgers."),
    ("cabinet_2", "archive_room", "Cabinet 2 — deeds."),
]


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


def _is_vault(of: object) -> bool:
    return of in ("vault", "vault_probe.tools")


def _share(rows: list[dict]) -> tuple[int, int]:
    """(chars in reflect + docs reply blocks, chars in every row) of the held view."""
    import re

    tag = re.compile(r"^\[reply to \S+ · \S+ ([^\]\s]+)")
    total = rd = 0
    for r in rows:
        text = r.get("content") if isinstance(r.get("content"), str) else ""
        total += len(text)
        if r.get("role") != "user":
            continue
        for b in re.split(r"\n\n(?=\[reply to \S+ · )", text):
            m = tag.match(b)
            if m and m.group(1) in ("reflect", "docs"):
                rd += len(b)
    return rd, total


async def main():
    root, core, close_kernel = await open_kernel(
        "docs_mech_", reach=["desk", "vault", "kernel_store", AID, "cli", "user"]
    )
    for rid, parent, desc in ROOMS:
        r = (
            await root.send(
                [
                    {
                        "target": root.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": parent or home(root),
                            "handler_module": "file_bridge.tools",
                            "id": rid,
                            "root": ".",
                            "allow": [],
                            "description": desc,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert not r.get("error"), f"create {rid}: {r}"
    for payload in (
        {
            "type": "create_agent",
            "parent_id": "archive_room",
            "handler_module": "vault_probe.tools",
            "id": "vault",
            "serial": SERIAL,
            "secret": SECRET,
            "callable_by": {AID: "*", "rest": "*"},
        },
        {
            "type": "create_agent",
            "parent_id": home(root),
            "handler_module": "file_bridge.tools",
            "allow": ["."],
            "id": "desk",
            "root": ".",
            "ingress_rule": "allow_all_ingress",
            "description": "The desk: write answers here.",
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
        r = (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]
        assert not r.get("error"), f"create {payload['id']}: {r}"

    conn = await connect(
        root,
        core,
        BK,
        agent_id=AID,
        can_call={root.id: "*", "vault": "*", "desk": "*", "cli": "*", "user": "*"},
        callable_by={"user": "*"},
        limits=Limits.from_env(os.environ),
    )
    capped = await drive(conn, TASK, client_id="docs", via="user")
    calls = _calls(conn.proxy.transcript)
    held = (await root.send([{"target": AID, "payload": {"type": "rows"}}]))[0][
        "result"
    ]
    rows = (held or {}).get("rows") or []
    await conn.close()
    content = ""
    try:
        content = Path("answer.md").read_text()
    except OSError:
        pass
    await close_kernel()

    def is_docs(p):
        return p.get("type") == "docs"

    opens = [
        i for i, (t, p) in enumerate(calls) if t == "vault" and p.get("type") == "open"
    ]
    first_open = opens[0] if opens else len(calls)
    docs = [(i, p) for i, (t, p) in enumerate(calls) if is_docs(p)]
    one = [(i, p) for i, p in docs if p.get("tier") in (None, "oneliners")]
    det = [(i, p) for i, p in docs if p.get("tier") == "detailed"]
    reflects = [(i, p) for i, (t, p) in enumerate(calls) if p.get("type") == "reflect"]
    filtered = [
        (i, p)
        for i, p in reflects
        if p.get("show_subtrees") or p.get("ignore_subtrees")
    ]

    done = SECRET in content
    oneliners_first = any(i < first_open and _is_vault(p.get("of")) for i, p in one)
    used = {(t, p.get("type")) for t, p in calls}

    def _used(p):
        of, verb = p.get("of"), p.get("verb")
        target = (
            "vault"
            if _is_vault(of)
            else ("kernel" if of in ("kernel", root.id) else of)
        )
        return bool(verb) and any(
            (t, v) == (target, verb)
            or (target == "kernel" and t == root.id and v == verb)
            for t, v in used
        )

    detailed_ok = bool(det) and all(_used(p) for _, p in det)
    first_reflect = reflects[0][0] if reflects else None
    filtered_after = any(
        first_reflect is not None and i > first_reflect for i, _ in filtered
    )
    rd, total = _share(rows)

    print(f"\n  calls: {len(calls)} · opens: {len(opens)} · cap: {capped or 'clean'}")
    print(
        f"  docs pulls: {len(one)} oneliners, {len(det)} detailed "
        f"({[(p.get('of'), p.get('verb')) for _, p in det]})"
    )
    print(
        f"  reflects: {len(reflects)} ({len(filtered)} filtered: "
        f"{[{k: v for k, v in p.items() if k.endswith('subtrees')} for _, p in filtered]})"
    )
    print(
        f"  reflect+docs share of the held conversation: "
        f"{rd}/{total} chars = {rd / total:.0%}"
        if total
        else "  (no rows held)"
    )
    print(f"\n  1. task done (secret in answer.md)          : {'OK' if done else 'NO'}")
    print(
        f"  2. oneliner docs before the first open       : {'OK' if oneliners_first else 'NO'}"
    )
    print(
        f"  3. detailed only for a verb then used        : {'OK' if detailed_ok else 'NO'}"
    )
    print(
        f"  4. a filtered reflect after the first look   : {'OK' if filtered_after else 'NO'}"
    )
    ok = done and oneliners_first and detailed_ok and filtered_after
    verdict(
        ok,
        "found the vault, read its docs by tier, cut the tree"
        if ok
        else f"done={done} oneliners_first={oneliners_first} "
        f"detailed_ok={detailed_ok} filtered_after={filtered_after}",
    )


if __name__ == "__main__":
    asyncio.run(main())
