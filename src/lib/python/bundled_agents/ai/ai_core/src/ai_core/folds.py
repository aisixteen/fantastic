"""FOLDS — a compaction MARK, not a deletion.

A fold is an ordinary appended row that names the rows it collapses (`refs`), carries a
title (what the model reads every turn), a body (a short summary, pulled if it wants
more), and stays one descent from its members: `grep`/`pull` on a fold id expand to its
refs. Nothing is rewritten, nothing is lost, and the history becomes a tree of marks the
model walks down as far as it needs.

CHAINS ARE DERIVED, NEVER STORED. A chain is a root row plus everything reachable from it
through `parent`. A model attributes rows through interleaved chains from `parent` alone,
so the agent carries no thread concept — this module walks
the links when a projection needs to know what is closed.

A FOLD CARRIES ITS OWN PLACE. Its `anchor` is the smallest LEAF id it covers (a leaf is
a non-fold row; a member fold contributes its own anchor). Row ids come from the
kernel's one counter, so an anchor orders a fold in time without any of its members
present: the view array names folds without the rows they hide, and `context_forget`
prunes those rows from the store. A fold written before folds carried the field is
anchored by the `anchors` row `context_forget` recorded for it, or, while its members
are still stored, by deriving the same value from its refs.
"""

from __future__ import annotations

import json
from bisect import bisect_left

from ai_core.tool_parse import extract_tool_calls
from ai_core.constants import SUMMARY_INPUT_CAP  # noqa: F401 — re-exported seams

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
    refs: list[int],
    title: str,
    body: str,
    degraded: bool,
    *,
    id: int,
    anchor: int,
    lvl: int = 1,
    parent: int | None = None,
) -> dict:
    """A fold row. Its id is MINTED BY THE STORE (`ctx.mint_id()`) and passed in — a
    fold is a message like any other, never self-named. `anchor` is the smallest leaf
    id it covers (`member_anchor` over its members)."""
    fid = id
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
        "anchor": anchor,
        "content": f"{head}{note}\n{body}",
    }


def recorded_anchors(rows: list[dict]) -> dict[int, int]:
    """The anchors `context_forget` recorded: every `anchors` row in `rows`, in order,
    a later row winning per fold id."""
    out: dict[int, int] = {}
    for r in rows:
        if r.get("kind") != "anchors" or not isinstance(r.get("anchors"), dict):
            continue
        for k, v in r["anchors"].items():
            fid = _int(k)
            a = _int(v)
            if fid is not None and a is not None:
                out[fid] = a
    return out


def _int(x) -> int | None:
    if isinstance(x, bool):
        return None
    if isinstance(x, int):
        return x
    if isinstance(x, str) and x.isdigit():
        return int(x)
    return None


def anchor_of(
    fold: dict, rows: list[dict], recorded: dict[int, int], *, _seen=None
) -> int | None:
    """Where `fold` sits: its own `anchor` field; else the anchor `context_forget`
    recorded for it (`recorded`, from `recorded_anchors`); else the smallest leaf id
    among its refs present in `rows`, through member folds recursively. None when
    none of the three is known — such a fold is counted, never placed by guessing."""
    own = _int(fold.get("anchor"))
    if own is not None:
        return own
    fid = fold.get("id")
    if fid in recorded:
        return recorded[fid]
    by_id = {r.get("id"): r for r in rows if r.get("id") is not None}
    return _derive(fold, by_id, recorded, _seen or set())


def _derive(fold: dict, by_id: dict, recorded: dict[int, int], seen: set) -> int | None:
    fid = fold.get("id")
    if fid in seen:
        return None
    seen = seen | {fid}
    best = None
    for ref in fold.get("refs") or ():
        r = by_id.get(ref)
        if r is None:
            continue
        if is_fold(r):
            a = _int(r.get("anchor"))
            if a is None:
                a = recorded.get(ref)
            if a is None:
                a = _derive(r, by_id, recorded, seen)
        else:
            a = _int(ref)
        if a is not None and (best is None or a < best):
            best = a
    return best


def anchors_of(rows: list[dict]) -> dict[int, int]:
    """fold id → anchor for every fold in `rows` whose anchor is known. `rows` are the
    agent's HEAD rows (`anchors` rows included), never only the body being rendered."""
    recorded = recorded_anchors(rows)
    by_id = {r.get("id"): r for r in rows if r.get("id") is not None}
    out: dict[int, int] = {}
    for r in rows:
        if not is_fold(r):
            continue
        a = _int(r.get("anchor"))
        if a is None:
            a = recorded.get(r.get("id"))
        if a is None:
            a = _derive(r, by_id, recorded, set())
        if a is not None:
            out[r["id"]] = a
    return out


def member_anchor(rows: list[dict], idxs: list[int], anchors: dict[int, int]) -> int:
    """The anchor a new fold over `rows[i] for i in idxs` carries: the smallest leaf id,
    a member fold contributing its anchor. A member fold with no known anchor cannot
    be in the view, so it cannot be a member: that is a programming error."""
    best = None
    for i in idxs:
        r = rows[i]
        if is_fold(r):
            a = _int(r.get("anchor"))
            if a is None:
                a = anchors.get(r.get("id"))
            if a is None:
                raise ValueError(f"fold: member fold {r.get('id')} has no anchor")
        else:
            a = _int(r.get("id"))
            if a is None:
                raise ValueError(f"fold: member row {r.get('id')!r} has no int id")
        if best is None or a < best:
            best = a
    if best is None:
        raise ValueError("fold: a fold needs at least one member")
    return best


def _placed(rows: list[dict], anchors: dict[int, int]) -> tuple[dict, int]:
    """(row index → [(anchor, fold)], unanchored). Each fold not covered by another
    fold in `rows` sits immediately before the first non-fold row whose id is at or
    past its anchor, or at the end when there is none."""
    cov = covered(rows)
    # first_at(a) = the smallest row index whose id is >= a: ids sorted ascending,
    # each paired with the smallest index among the ids at or above it.
    eligible = sorted(
        (x["id"], i)
        for i, x in enumerate(rows)
        if not is_fold(x) and _int(x.get("id")) is not None
    )
    keys = [e[0] for e in eligible]
    least = [len(rows)] * (len(eligible) + 1)
    for j in range(len(eligible) - 1, -1, -1):
        least[j] = min(eligible[j][1], least[j + 1])
    place: dict[int, list[tuple[int, dict]]] = {}
    unanchored = 0
    for r in rows:
        if not is_fold(r) or r.get("id") in cov:
            continue
        a = _int(r.get("anchor"))
        if a is None:
            a = anchors.get(r.get("id"))
        if a is None:
            unanchored += 1
            continue
        place.setdefault(least[bisect_left(keys, a)], []).append((a, r))
    return place, unanchored


def render(rows: list[dict], anchors: dict[int, int]) -> list[dict]:
    """The view with folded rows hidden and each fold sitting at its anchor (`anchors`
    from `anchors_of` over the HEAD — the members need not be in `rows`). A fold
    covered by another fold in `rows` stays hidden; a fold whose anchor is unknown is
    left out (`unanchored` counts it). Deterministic in `rows`, so a view renders the
    same bytes until a new fold lands."""
    cov = covered(rows)
    place, _ = _placed(rows, anchors)
    out: list[dict] = []
    for i, r in enumerate(rows):
        for _, f in sorted(place.get(i, ()), key=lambda p: p[0]):
            out.append(f)
        if r.get("id") in cov or is_fold(r):
            continue  # hidden, or a fold rendered at its anchor (or itself folded)
        out.append(r)
    for _, f in sorted(place.get(len(rows), ()), key=lambda p: p[0]):
        out.append(f)
    return out


def unanchored(rows: list[dict], anchors: dict[int, int]) -> int:
    """How many folds `render` leaves out because no anchor is known for them."""
    return _placed(rows, anchors)[1]


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


def level(rows: list[dict], fold_row: dict) -> int:
    """1 for a fold of rows, 2 for a fold of folds, and so on."""
    by_id = {r.get("id"): r for r in rows if r.get("id")}
    best = 0
    for ref in fold_row.get("refs") or ():
        r = by_id.get(ref)
        if r is not None and is_fold(r):
            best = max(best, level(rows, r))
    return best + 1


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
