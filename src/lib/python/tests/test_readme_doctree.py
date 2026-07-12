"""Keyed, tiered readme doc tree (reflect self-description v2).

The monolithic root readme is replaced by a SHORT `readme.md` orientation +
a `docs/` topic tree. `reflect` gains a polymorphic `readme` value:
  - `readme=true`   → the short orientation (regression sentinel).
  - `readme="index"` → a token-annotated topic tree `{key, desc, tokens, children}`.
  - `readme="<key>"` → one topic's body + its sub-docs' index.
  - unknown key      → `{error, keys:[…]}` (reply stays well-formed).
The ROOT reflect always carries `readmes` (the index) + `docs` (a one-liner) —
the manual's map is in front of a caller from the first round-trip.

Tests use the `kernel` fixture (its root seeds `.fantastic/readme.md` + `docs/`
on disk via `boot_root`→`write_record`).
"""

from __future__ import annotations

from pathlib import Path

from _testkit import persist

from ai_core.context import CHARS_PER_TOKEN
from kernel._agent import Agent

# ─── token method (must match ai_core) ─────────────────────────


def test_est_tokens_is_ai_core_ceil_div():
    """Advertised cost == the estimate the model actually pays: ceil(chars/4),
    the same constant ai_core.context uses."""
    assert Agent._CHARS_PER_TOKEN == CHARS_PER_TOKEN
    assert Agent._est_tokens("x" * 400) == 100  # 400/4
    assert Agent._est_tokens("x" * 401) == 101  # ceil
    assert Agent._est_tokens("") == 0


def test_read_desc_prefers_quote_line():
    assert Agent._read_desc("> the descriptor\n\n# Title\nbody") == "the descriptor"
    assert Agent._read_desc("# Title\n\nfirst prose line") == "first prose line"
    assert Agent._read_desc("# only a heading") == ""


# ─── docs seeded on disk ───────────────────────────────────────


def test_docs_tree_seeded_on_disk(kernel):
    """The bootstrap seeds the `docs/` topic tree (nested tiers) beside the
    root orientation readme."""
    docs = Path(kernel._root_path) / "docs"
    assert (docs / "self-extension.md").exists()
    assert (docs / "reflect.md").exists()
    # a nested tier
    assert (docs / "frontend" / "panel-recipe.md").exists()


async def test_docs_seeded_via_store(seeded_kernel, store_agent):
    """When a persistence provider is wired, the docs tree seeds THROUGH it too
    (copy-if-missing) — a freshly created keyed-doc agent gets its own tree."""
    persist(seeded_kernel)
    got = await seeded_kernel.send(
        store_agent, {"type": "read_stream", "path": "docs/self-extension.md"}
    )
    assert isinstance(got.get("bytes"), (bytes, bytearray))
    assert b"author" in bytes(got["bytes"]).lower()


# ─── readme=true → short orientation ───────────────────────────


async def test_readme_true_is_short_orientation(kernel):
    """`readme=true` is the SHORT orientation, NOT the old monolith: it teaches
    the keyed doc tree and no longer inlines the demoted manual sections."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": True})
    body = r["readme"]
    assert isinstance(body, str)
    assert "Fantastic kernel" in body
    assert "keyed doc tree" in body
    assert "readme=index" in body
    # the demoted sections are gone from the orientation (they are docs now)
    assert "## Transports" not in body
    assert "## Streams" not in body
    assert len(body) < 3000  # was ~20k as one dump


# ─── readme=index → token-annotated tree ───────────────────────


async def test_readme_index_shape_and_nesting(kernel):
    """`readme=index` → `{index:[{key,desc,tokens,children?}]}`; entries carry NO
    body; nesting is preserved (frontend → panel-recipe/routing)."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": "index"})
    idx = r["readme"]["index"]
    assert isinstance(idx, list) and idx
    by_key = {e["key"]: e for e in idx}
    assert "self-extension" in by_key
    for e in idx:
        assert set(e).issubset({"key", "desc", "tokens", "children"})
        assert isinstance(e["desc"], str) and e["desc"]
        assert isinstance(e["tokens"], int) and e["tokens"] > 0
        assert "body" not in e
    fe = by_key["frontend"]
    child_keys = {c["key"] for c in fe["children"]}
    assert child_keys == {"frontend/panel-recipe", "frontend/routing"}


# ─── readme=<key> → body + children index ──────────────────────


async def test_readme_key_returns_body_and_children(kernel):
    """`readme=<key>` → the topic body + its immediate sub-docs' index."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": "self-extension"})
    doc = r["readme"]
    assert doc["key"] == "self-extension"
    assert "author" in doc["body"].lower()
    assert doc["tokens"] == Agent._est_tokens(doc["body"])
    assert "children" not in doc  # a leaf topic

    parent = await kernel.send("kernel", {"type": "reflect", "readme": "frontend"})
    assert {c["key"] for c in parent["readme"]["children"]} == {
        "frontend/panel-recipe",
        "frontend/routing",
    }


async def test_readme_nested_key_reachable(kernel):
    """A sub-doc is fetchable by its slashed key."""
    r = await kernel.send(
        "kernel", {"type": "reflect", "readme": "frontend/panel-recipe"}
    )
    assert r["readme"]["key"] == "frontend/panel-recipe"
    assert "panel" in r["readme"]["body"].lower()


async def test_readme_unknown_key_is_wellformed_error(kernel):
    """Unknown key → `{error, keys}` (never a crash / null); keys lists what IS
    available so the caller can recover."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": "no-such-topic"})
    doc = r["readme"]
    assert "error" in doc
    assert "self-extension" in doc["keys"]
    assert "frontend/panel-recipe" in doc["keys"]  # flattened, includes tiers


async def test_readme_key_cannot_escape_docs_dir(kernel):
    """A `..`-escaping key is refused (clamped), not served."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": "../readme"})
    assert "error" in r["readme"]


# ─── always-on root frame (readmes + docs) ─────────────────────


async def test_root_reflect_carries_readmes_and_docs(kernel):
    """The root reflect always carries `readmes` (the index) + a `docs` one-liner,
    so the map is present from the first round-trip — no monolith dump."""
    r = await kernel.send("kernel", {"type": "reflect"})
    assert isinstance(r["readmes"], list) and r["readmes"]
    keys = {e["key"] for e in r["readmes"]}
    assert "self-extension" in keys
    assert isinstance(r["docs"], str)
    assert "readme=index" in r["docs"]


# ─── non-root bundle: keyed-fetchable, no root frame ───────────


async def test_non_root_bundle_no_frame_but_keyed(seeded_kernel):
    """A non-root bundle carries neither `readmes` nor `docs` (root-only), but IS
    keyed-fetchable: `readme=true` → its single readme; a keyless bundle's
    `readme=index` → empty; an unknown key → `{error, keys:[]}`."""
    rec = await seeded_kernel.send(
        "kernel", {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    fid = rec["id"]
    persist(seeded_kernel)
    r = await seeded_kernel.send(fid, {"type": "reflect"})
    assert "readmes" not in r and "docs" not in r
    full = await seeded_kernel.send(fid, {"type": "reflect", "readme": True})
    assert isinstance(full["readme"], str) and full["readme"]
    idx = await seeded_kernel.send(fid, {"type": "reflect", "readme": "index"})
    assert idx["readme"] == {"index": []}  # no docs/ tree
    miss = await seeded_kernel.send(fid, {"type": "reflect", "readme": "streams"})
    assert miss["readme"]["error"] and miss["readme"]["keys"] == []
