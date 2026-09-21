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
    got = (
        await seeded_kernel.send(
            [
                {
                    "target": store_agent,
                    "payload": {
                        "type": "read_stream",
                        "path": "docs/self-extension.md",
                    },
                }
            ]
        )
    )[0]["result"]
    assert isinstance(got.get("bytes"), (bytes, bytearray))
    assert b"author" in bytes(got["bytes"]).lower()


# ─── readme=true → short orientation ───────────────────────────


async def test_readme_true_is_short_orientation(kernel):
    """`readme=true` is the SHORT orientation, NOT the old monolith: it teaches
    the keyed doc tree and no longer inlines the demoted manual sections."""
    r = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": True}}]
        )
    )[0]["result"]
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
    r = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": "index"}}]
        )
    )[0]["result"]
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
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "readme": "self-extension"},
                }
            ]
        )
    )[0]["result"]
    doc = r["readme"]
    assert doc["key"] == "self-extension"
    assert "author" in doc["body"].lower()
    assert doc["tokens"] == Agent._est_tokens(doc["body"])
    assert "children" not in doc  # a leaf topic

    parent = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": "frontend"}}]
        )
    )[0]["result"]
    assert {c["key"] for c in parent["readme"]["children"]} == {
        "frontend/panel-recipe",
        "frontend/routing",
    }


async def test_no_doc_desc_is_truncated_mid_sentence(kernel):
    """`_read_desc` takes the FIRST `> ` line and only that one — so a `> ` descriptor
    wrapped across two lines silently renders as half a sentence in `readme=index`.

    Caught on 2026-08-06 by the rust port: a same-day edit here wrapped
    self-extension's descriptor, and the index started advertising "…whose persisted
    `code` is a". The reader is documented as one-line; the doc has to hold up its end,
    and this test is cheaper than remembering that."""
    idx = (
        (
            await kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {"type": "reflect", "readme": "index"},
                    }
                ]
            )
        )[0]["result"]
    )["readme"]

    def walk(entries):
        for e in entries:
            yield e
            yield from walk(e.get("children") or [])

    bad = [
        (e["key"], e["desc"])
        for e in walk(idx["index"])
        if e["desc"] and e["desc"][-1] not in ".!?)`"
    ]
    assert not bad, (
        f"descriptors that do not end a sentence (wrapped `> ` line?): {bad}"
    )


async def test_self_extension_DOC_states_the_execution_model(kernel):
    """The doc the pointer SENDS YOU TO must carry the CURRENT contract.

    The `code` field is gone. The doc must state the model that DOES work — a
    durable program is a FILE (written over streams) plus a command that runs it —
    plainly enough that a model builds instead of declaring the task impossible,
    and must NOT teach the recipe that is now refused at `create_agent`."""
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "readme": "self-extension"},
                }
            ]
        )
    )[0]["result"]
    body = r["readme"]["body"].lower()
    # The live model, stated plainly: a durable program is a FILE plus a command,
    # no `code` field, and the two live paths named.
    assert "no `code` field" in body or "there is no `code`" in body, (
        "must say the code field is gone"
    )
    assert "durable program is a file" in body, "must state the file+command model"
    assert "write_stream" in body, "must teach the stream verbs for files"
    assert "command" in body, "must point at the command runner"
    assert "code:'" not in body, "must NOT teach the dead inline-code recipe"


async def test_readme_nested_key_reachable(kernel):
    """A sub-doc is fetchable by its slashed key."""
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "readme": "frontend/panel-recipe"},
                }
            ]
        )
    )[0]["result"]
    assert r["readme"]["key"] == "frontend/panel-recipe"
    assert "panel" in r["readme"]["body"].lower()


async def test_readme_unknown_key_is_wellformed_error(kernel):
    """Unknown key → `{error, keys}` (never a crash / null); keys lists what IS
    available so the caller can recover."""
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "readme": "no-such-topic"},
                }
            ]
        )
    )[0]["result"]
    doc = r["readme"]
    assert "error" in doc
    assert "self-extension" in doc["keys"]
    assert "frontend/panel-recipe" in doc["keys"]  # flattened, includes tiers


async def test_readme_key_cannot_escape_docs_dir(kernel):
    """A `..`-escaping key is refused (clamped), not served."""
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "readme": "../readme"},
                }
            ]
        )
    )[0]["result"]
    assert "error" in r["readme"]


# ─── always-on root frame (readmes + docs) ─────────────────────


async def test_root_reflect_full_carries_readmes_and_docs(kernel):
    """`detail='full'` carries `readmes` (the index) + a `docs` one-liner — the
    orientation document, asked for explicitly."""
    r = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    assert isinstance(r["readmes"], list) and r["readmes"]
    keys = {e["key"] for e in r["readmes"]}
    assert "self-extension" in keys
    assert isinstance(r["docs"], str)
    assert "keyed doc tree" in r["docs"] and "readme" in r["docs"]


async def test_distilled_default_keeps_self_extension_ACTIONABLE(kernel):
    """THE SAFETY PROPERTY OF THE DIET — strengthened by measurement.

    The distilled default omits the inlined doc frame (`readmes` + `docs` + the long
    self_extension prose — ~1,267 tok of manual that `readme='index'` serves on
    demand). That is only sound while the reply leaves self-extension **actionable**.

    This test used to assert `self_extension not in r`, on the theory that naming the
    affordance in `more` was enough. It is not, and the failure is measured, not
    theoretical: a model given only the distilled reflect, which understood it could
    build a durable tool but was not told the SHAPE, invented a recipe that could not
    run and built nothing (2026-08-05/06, opus-5 and gemma-4-12B in separate runs).

    So the rule is sharper than "keep the pointer": a contract you do not know is not
    usable, and the weak models are exactly the ones that never follow the pointer to
    read it. The prose may leave; the SHAPE of a durable program may not — the
    distilled string must name the live paths (write a FILE over `write_stream`, run
    it with a `command`) and must not resurrect the withdrawn `code` field."""
    r = (await kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    # the bulk frame is still gone — the diet holds
    assert "readmes" not in r and "docs" not in r

    se = r.get("self_extension")
    assert se, "distilled reflect must still carry a self_extension pointer"
    assert len(se) < 900, (
        f"pointer must stay a pointer, not the prose ({len(se)} chars)"
    )
    # It must not resurrect the dead recipe, and it must name the live paths.
    assert "code:'" not in se, "pointer must not teach the withdrawn `code` field"
    assert "write_stream" in se, "pointer must name the stream write path"


async def test_full_reflect_names_no_dead_authoring_recipe(kernel):
    """Self-authoring is withdrawn, so the full reflect must not advertise the old
    `code`/interpreter/shim recipe — a model following a dead recipe burns messages
    on a `create_agent` now refused. Replaces the old advertised-executors test,
    whose invariant is moot while nothing is advertised."""
    r = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    se = r["self_extension"]
    for dead in ("code:'", "code=", "interpreters:", "shim:"):
        assert dead not in se, (
            f"full reflect still teaches the withdrawn recipe: {dead!r}"
        )
    assert "write_stream" in se and "command" in se.lower()


async def test_non_root_bundle_no_frame_but_keyed(seeded_kernel):
    """A non-root bundle carries neither `readmes` nor `docs` (root-only), but IS
    keyed-fetchable: `readme=true` → its single readme; a keyless bundle's
    `readme=index` → empty; an unknown key → `{error, keys:[]}`."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel",
                        "handler_module": "file_bridge.tools",
                    },
                }
            ]
        )
    )[0]["result"]
    fid = rec["id"]
    persist(seeded_kernel)
    r = (await seeded_kernel.send([{"target": fid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert "readmes" not in r and "docs" not in r
    full = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "reflect", "readme": True}}]
        )
    )[0]["result"]
    assert isinstance(full["readme"], str) and full["readme"]
    idx = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "reflect", "readme": "index"}}]
        )
    )[0]["result"]
    assert idx["readme"] == {"index": []}  # no docs/ tree
    miss = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "reflect", "readme": "streams"}}]
        )
    )[0]["result"]
    assert miss["readme"]["error"] and miss["readme"]["keys"] == []
