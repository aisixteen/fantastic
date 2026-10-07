"""How to CALL an agent — the `docs` kernel verb, and the reflect subtree filters.

A reflect says what an agent IS (owner, 2026-10-05): id, sentence, state, grants,
tree. How to call it — verb signatures, emits, cancel, notes — is `docs`, pulled
per BUNDLE (two agents of one bundle share their docs), in two tiers:

  - `oneliners` (default) — `{verb: signature}`: each docstring's first paragraph
    up to its first top-level `. ` or ` — ` (never inside brackets, so an arg note
    like `(req — …)` stays whole);
  - `detailed` — `{verb: full docstring}`; without a `verb` filter, plus the
    bundle's `EMITS`, `CANCEL` and `NOTES` (module attributes, duck-typed like
    `RECORD_SCHEMA`).

Measured 2026-10-05 (rust e2e): docs were ~75% of a per-agent reflect and up to
59k chars of a test's transcript — the same text every time an agent of that kind
was reflected.
"""

from __future__ import annotations

import inspect
from typing import Any

from kernel._refusal import BAD_ARG, refusal

TIERS = ("oneliners", "detailed")


def full_doc(doc: str | None) -> str:
    """A verb's whole docstring, dedented."""
    return inspect.cleandoc(doc or "")


def signature(doc: str | None) -> str:
    """The ONELINER of a verb doc: its first paragraph (wrapped lines joined), cut
    at the first top-level `. ` or ` — `; a trailing `.` dropped."""
    para = full_doc(doc).split("\n\n", 1)[0]
    line = " ".join(p.strip() for p in para.splitlines())
    depth = 0
    for i, ch in enumerate(line):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth = max(0, depth - 1)
        elif depth == 0 and (line.startswith(". ", i) or line.startswith(" — ", i)):
            return line[:i]
    return line.rstrip(".")


def render(
    of: str,
    bundle: str | None,
    tier: str,
    verb: str | None,
    docs: dict[str, str],
    extras: dict[str, Any],
) -> dict:
    """The `docs` reply: `{of, bundle, tier, verbs, emits?, cancel?, notes?}`.
    `docs` maps verb → raw doc text; `extras` holds the bundle's EMITS / CANCEL /
    NOTES (absent ones omitted), served only on a detailed reply with no `verb`."""
    if verb is not None and verb not in docs:
        return refusal(
            BAD_ARG,
            f"docs: {of} has no verb {verb!r}",
            hint=f"its verbs: {', '.join(docs)}",
            arg="verb",
        )
    names = [verb] if verb is not None else list(docs)
    shape = full_doc if tier == "detailed" else signature
    out: dict[str, Any] = {
        "of": of,
        "bundle": bundle,
        "tier": tier,
        "verbs": {n: shape(docs[n]) for n in names},
    }
    if tier == "detailed" and verb is None:
        for key in ("emits", "cancel", "notes"):
            if extras.get(key) is not None:
                out[key] = extras[key]
    return out


def module_docs(mod) -> tuple[dict[str, str], dict[str, Any]]:
    """A bundle module's verb docs (from its `VERBS` table — a handler's docstring,
    or the doc text itself where the table maps a verb to its doc, as the e2e
    probes do) and its extras."""
    table = getattr(mod, "VERBS", None)
    docs = (
        {n: f if isinstance(f, str) else (f.__doc__ or "") for n, f in table.items()}
        if isinstance(table, dict)
        else {}
    )
    extras = {
        "emits": getattr(mod, "EMITS", None),
        "cancel": getattr(mod, "CANCEL", None),
        "notes": getattr(mod, "NOTES", None),
    }
    return docs, extras


# ─── reflect subtree filters ────────────────────────────────────


def subtree_filter(payload: dict, target, agents: dict) -> tuple[list, set] | dict:
    """Validate `show_subtrees` / `ignore_subtrees` on a reflect of `target`.

    Returns `(show, ignore)` — `show` the branch roots to answer with (empty: the
    whole subtree of `target`), `ignore` the ids pruned to a stub — or a refusal.
    Every id must name an agent in `target`'s subtree (`target` included). A shown id
    under another shown id is already inside that branch and is not repeated."""
    lists: dict[str, list[str]] = {}
    for arg in ("show_subtrees", "ignore_subtrees"):
        v = payload.get(arg)
        if v is None:
            lists[arg] = []
            continue
        if not isinstance(v, list) or not all(isinstance(x, str) and x for x in v):
            return refusal(
                BAD_ARG, f"reflect: {arg} must be a list of agent ids", arg=arg
            )
        for x in v:
            a = agents.get(x)
            if a is None or not _within(a, target):
                return refusal(
                    BAD_ARG,
                    f"reflect: {arg} names {x!r} — not an agent in {target.id}'s tree",
                    hint="reflect with tree='ids' lists the ids",
                    arg=arg,
                )
        lists[arg] = v
    shown_ids = set(lists["show_subtrees"])
    show = []
    for x in dict.fromkeys(lists["show_subtrees"]):
        a = agents[x]
        if not any(_within(a, agents[o]) for o in shown_ids if o != x):
            show.append(a)
    return show, set(lists["ignore_subtrees"])


def _within(agent, ancestor) -> bool:
    """Is `agent` `ancestor` or below it? The parent-chain walk the gate uses."""
    a = agent
    while a is not None:
        if a is ancestor:
            return True
        a = a.parent
    return False


def pruned_stub(agent) -> dict:
    """What an ignored branch leaves in the tree: its id and how many agents it
    holds (itself included), so the model knows something is there to open."""
    return {"id": agent.id, "pruned": len(agent._descendant_ids())}
