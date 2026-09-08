"""FOLDS — a compaction MARK, not a deletion.

A fold is an ordinary appended row that names the rows it collapses (`refs`), carries a
title (what the model reads every turn), a body (a short summary, pulled if it wants
more), and stays one descent from its members: `grep`/`pull` on a fold id expand to its
refs. Nothing is rewritten, nothing is lost, and the history becomes a tree of marks the
model walks down as far as it needs.

CHAINS ARE DERIVED, NEVER STORED. A chain is a root row plus everything reachable from it
through `parent`. Measured 2026-09-06: the model attributed rows 5/5 through 16 interleaved
chains from `parent` alone, so the agent carries no thread concept — this module walks
the links when a projection needs to know what is closed.
"""

from __future__ import annotations

import json
import secrets

from ai_core.tool_parse import extract_tool_calls

FOLD_TAG = "[fold "


def chains(rows: list[dict]) -> list[list[int]]:
    """Row indices grouped by root, in root arrival order. A row whose parent is unknown
    (absent, or pointing outside `rows`) is its own root."""
    by_id = {r.get("id"): i for i, r in enumerate(rows) if r.get("id")}
    root_of: dict[int, int] = {}

    def root(i: int) -> int:
        if i in root_of:
            return root_of[i]
        seen = [i]
        cur = i
        while True:
            par = rows[cur].get("parent")
            j = by_id.get(par) if par else None
            if j is None or j in seen:
                break
            seen.append(j)
            cur = j
        for k in seen:
            root_of[k] = cur
        return cur

    groups: dict[int, list[int]] = {}
    for i in range(len(rows)):
        groups.setdefault(root(i), []).append(i)
    return [groups[k] for k in sorted(groups)]


def is_open(rows: list[dict], chain: list[int]) -> bool:
    """A chain is OPEN while its last row is an assistant turn that made a `send` — a
    reply is still owed. A cancelled or failed call settles as a reply row, so a cancel
    closes a chain the same way an answer does."""
    last = rows[chain[-1]]
    if last.get("role") != "assistant":
        return False
    return any(
        c.get("name") == "send" for c in extract_tool_calls(last.get("content") or "")
    )


def is_fold(row: dict) -> bool:
    return row.get("kind") == "fold"


def covered(rows: list[dict]) -> set[str]:
    """Every id named by any fold's refs — transitively, since a fold may ref folds."""
    out: set[str] = set()
    for r in rows:
        if is_fold(r):
            out.update(r.get("refs") or ())
    return out


def make_fold(
    self_id: str,
    refs: list[str],
    title: str,
    body: str,
    degraded: bool,
    *,
    lvl: int = 1,
    parent: str | None = None,
) -> dict:
    fid = f"{self_id}:{secrets.token_hex(4)}"
    title = clean_title(title)
    head = f'{FOLD_TAG}{fid} · {len(refs)} refs · L{lvl}] "{title}"'
    note = "  (summary unavailable — refs and titles only)" if degraded else ""
    return {
        "role": "user",
        "id": fid,
        # A fold sits where its members sat: its parent is its first member's parent,
        # so it stays inside the chain it collapsed instead of becoming a root.
        "parent": parent,
        "kind": "fold",
        "refs": list(refs),
        "title": title,
        "body": body,
        "level": lvl,  # 1 = rows, 2 = a fold among its members, …
        "content": f"{head}{note}\n{body}",
    }


def render(rows: list[dict]) -> list[dict]:
    """The view with folded rows hidden and each fold sitting where its first ref was.
    Deterministic in `rows`, so a view renders the same bytes until a new fold lands."""
    cov = covered(rows)
    first_pos: dict[str, int] = {}
    for i, r in enumerate(rows):
        rid = r.get("id")
        if rid in cov and rid not in first_pos:
            first_pos[rid] = i
    place: dict[int, list[dict]] = {}
    for r in rows:
        if is_fold(r):
            refs = r.get("refs") or []
            at = min((first_pos[x] for x in refs if x in first_pos), default=None)
            if at is not None and r.get("id") not in cov:
                place.setdefault(at, []).append(r)
    out: list[dict] = []
    for i, r in enumerate(rows):
        for f in place.get(i, ()):
            out.append(f)
        rid = r.get("id")
        if rid in cov or is_fold(r):
            continue  # hidden, or a fold rendered at its first ref (or itself folded)
        out.append(r)
    # a fold whose refs are all absent from the view (already hidden by another fold)
    # is itself covered or unplaceable; nothing to do — it stays out of the view.
    return out


def expand_ids(rows: list[dict], ids: list[str]) -> list[str]:
    """Replace fold ids with their refs, recursively — the descent path."""
    folds = {r.get("id"): r for r in rows if is_fold(r)}
    out: list[str] = []
    stack = list(ids)
    seen: set[str] = set()
    while stack:
        x = stack.pop(0)
        if x in seen:
            continue
        seen.add(x)
        if x in folds:
            stack = list(folds[x].get("refs") or []) + stack
        else:
            out.append(x)
    return out


def closed_prefix(rows: list[dict], chain: list[int], keep_tail: int) -> list[int]:
    """The foldable PREFIX of an open chain: everything but the last `keep_tail` rows,
    cut back so it ends on a reply (never between a call and its answer). A superlong
    chain never closes, so this is how it still folds — covers (1) of the edge cases."""
    if len(chain) <= keep_tail + 1:
        return []
    span = chain[: len(chain) - keep_tail]
    while span and rows[span[-1]].get("role") == "assistant":
        span = span[:-1]
    return span if len(span) >= 2 else []


def fold_share(view: list[dict]) -> float:
    """Fraction of the view's bytes that are fold rows — the trigger for folding folds."""
    total = sum(len(str(r.get("content", ""))) for r in view) or 1
    return sum(len(str(r.get("content", ""))) for r in view if is_fold(r)) / total


def level(rows: list[dict], fold_row: dict) -> int:
    """1 for a fold of rows, 2 for a fold of folds, and so on."""
    by_id = {r.get("id"): r for r in rows if r.get("id")}
    best = 0
    for ref in fold_row.get("refs") or ():
        r = by_id.get(ref)
        if r is not None and is_fold(r):
            best = max(best, level(rows, r))
    return best + 1


SUMMARY_INPUT_CAP = (
    24_000  # chars handed to the summarizer per fold — never a 60k reply
)


def chain_text(rows: list[dict], chain: list[int]) -> str:
    """What the summarizer reads. CAPPED per row and in total: a chain with big inline
    rows would otherwise make the fold call itself the reply that does not fit."""
    per = max(400, SUMMARY_INPUT_CAP // max(1, len(chain)))
    items = []
    for i in chain:
        c = str(rows[i].get("content", ""))
        if len(c) > per:
            c = c[: per // 2] + f" …[{len(c) - per} chars elided]… " + c[-per // 2 :]
        items.append(
            {"role": rows[i].get("role"), "id": rows[i].get("id"), "content": c}
        )
    return json.dumps(items, ensure_ascii=False, default=str)[:SUMMARY_INPUT_CAP]


def clean_title(t: str) -> str:
    """A fold title lives inside the tag; it may not close it or quote itself."""
    return (
        " ".join(str(t).replace("]", ")").replace('"', "'").split())[:140]
        or "(untitled)"
    )
