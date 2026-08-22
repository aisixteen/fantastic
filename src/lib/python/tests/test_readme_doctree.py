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

import re

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


async def test_no_doc_desc_is_truncated_mid_sentence(kernel):
    """`_read_desc` takes the FIRST `> ` line and only that one — so a `> ` descriptor
    wrapped across two lines silently renders as half a sentence in `readme=index`.

    Caught on 2026-08-06 by the rust port: a same-day edit here wrapped
    self-extension's descriptor, and the index started advertising "…whose persisted
    `code` is a". The reader is documented as one-line; the doc has to hold up its end,
    and this test is cheaper than remembering that."""
    idx = (await kernel.send("kernel", {"type": "reflect", "readme": "index"}))[
        "readme"
    ]

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
    """The doc the pointer SENDS YOU TO must carry the contract, not just the pointer.

    Measured 2026-08-06: the distilled `self_extension` was corrected to say the `code`
    is a script run top to bottom, but THIS doc — the one that string tells a model to
    read before writing `code` — still said only "the `code` IS the tool". `top to
    bottom` existed in three places repo-wide and none of them was here, so a model
    that DID follow the pointer still learned nothing about execution. gemma-4-12B
    authored `def run(payload): …` twice and shipped two inert agents.

    The reflect string is for the model that never reads the manual; this is for the
    one that does. Both have to say it."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": "self-extension"})
    body = r["readme"]["body"].lower()
    assert "top to bottom" in body, "the doc must state HOW the code runs"
    assert "def " in body, "the doc must warn that a bare def does nothing"
    assert "module level" in body, "the doc must say where the work goes"


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


async def test_root_reflect_full_carries_readmes_and_docs(kernel):
    """`detail='full'` carries `readmes` (the index) + a `docs` one-liner — the
    orientation document, asked for explicitly."""
    r = await kernel.send("kernel", {"type": "reflect", "detail": "full"})
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
    theoretical: on 2026-08-05 claude-opus-5 was given only the distilled reflect,
    read `more`, understood that it could author durable tools — and then invented
    `handler_module:"local_runner.script"`, a module that does not exist, because
    nothing had ever told it the real one. The executor's actual module name appeared
    ZERO times in a 38 KB session. It built nothing.

    So the rule is sharper than "keep the pointer": an affordance you cannot NAME is
    not discoverable. The prose may leave; the executor's name may not.

    A SECOND fact joined it on 2026-08-06, for the same reason and with the same kind
    of evidence. The line used to say only "the `code` IS the tool" — which reads as
    "the code DEFINES the tool", so the callable is a function inside it.
    gemma-4-12B authored `def run(payload): ...` and sent `{type:'run'}` expecting a
    call, TWICE in separate runs. Both agents were inert (one was even valid Python),
    nothing was ever written, and `queued:true` never appeared because no job stayed
    alive. It had planned the structure correctly and lost on the contract.

    The split is by WHO READS WHAT: opus-5 opened `readme='self-extension'` and got
    this right; gemma never opened it and had only this string. A fact the readme owns
    is a fact weak models do not have — so an affordance whose CONTRACT you do not
    know is not usable, and the execution model stays here too."""
    r = await kernel.send("kernel", {"type": "reflect"})
    # the bulk frame is still gone — the diet holds
    assert "readmes" not in r and "docs" not in r

    se = r.get("self_extension")
    assert se, "distilled reflect must still carry a self_extension pointer"
    assert len(se) < 700, (
        f"pointer must stay a pointer, not the prose ({len(se)} chars)"
    )
    # THE POINT: a real, registered executor module, not a hint that one exists.
    m = re.search(r"handler_module:'([a-z_]+\.[a-z_]+)'", se)
    assert m, f"the pointer must NAME the executor; got {se!r}"
    # It must be an executor this build actually INSTALLED — derived from the live
    # entry-point registry, not a hand-written const. Advertising a module that is not
    # installed is the same defect as naming none: on 2026-08-05 opus-5, given a name it
    # could not use, authored `local_runner.script` and built nothing.
    installed = {b["handler_module"] for b in Agent._available_bundles()}
    assert m.group(1) in installed, (
        f"pointer names {m.group(1)!r}, which is not installed. installed={sorted(installed)}"
    )
    assert "create_agent" in se, "must show how to author, not just that you may"
    # THE SECOND POINT: the EXECUTION MODEL, not only the affordance and its name.
    # `def run(): ...` is what a model writes when told the code "is the tool".
    low = se.lower()
    assert "script" in low, f"must say the code is a SCRIPT; got {se!r}"
    assert "top to bottom" in low, f"must say HOW it runs; got {se!r}"
    assert "def" in low, f"must warn that a bare def does nothing; got {se!r}"
    # THE THIRD POINT, same class as the first: the fields WITHOUT WHICH NOTHING RUNS.
    # The advertised executor is an io leg with an interpreter ALLOWLIST, so a
    # `create_agent` carrying only `code` produces an agent that is created, reported
    # created, and inert — no interpreter to spawn, and a sealed gate that denies the
    # `boot` whose reply `create_agent` discards. Unguessable + never mentioned is
    # exactly what cost the 2026-08-05 run when the executor itself was unnamed.
    assert "interpreters" in se or "interpreter" in se, (
        f"must name the interpreter field the executor cannot run without; got {se!r}"
    )
    assert "ingress_rule" in se, (
        f"must name the gate, or the authored agent is silently sealed; got {se!r}"
    )

    more = r["more"]
    assert "readme:'index'" in more, "must name the escalation payload"
    assert "detail:'full'" in more, "must name the full view"
    assert "self-extension" in more and "durable tools" in more


async def test_full_reflect_advertises_only_installed_executors(kernel):
    """Both directions, derived: every executor NAMED is installed, and every known
    executor that IS installed gets named. python was the last runtime whose list was
    hand-written — rust (`reflect.rs::registered_executors`) and swift
    (`SystemVerbs.swift::scriptExecutorsHere`) intersect with their registries, so this
    pins the same property here rather than leaving one runtime able to drift.

    The scan is a HANDLER-MODULE shape, not an executor NAME. It used to be
    `[a-z_]+_runtime\\.tools`, which encodes the naming convention of the bundle that
    happened to hold the role: when `exec_bridge.tools` took it over, that regex matched
    nothing, `named` came out empty, and the test went green on a kernel advertising an
    executor it had never checked. Same scar as the probes' `"runtime" in
    handler_module` — assert the property (named ⇔ installed), never the name."""
    from kernel_state.tools import _EXECUTORS

    r = await kernel.send("kernel", {"type": "reflect", "detail": "full"})
    se = r["self_extension"]
    installed = {b["handler_module"] for b in Agent._available_bundles()}

    named = set(re.findall(r"\b([a-z_]+\.tools)\b", se))
    assert named <= installed, (
        f"advertised but not installed: {sorted(named - installed)}"
    )
    expected = {m for m in _EXECUTORS if m in installed}
    assert named == expected, f"named={sorted(named)} expected={sorted(expected)}"
    for module in named:
        assert _EXECUTORS[module] in se, f"{module} named without its receive shape"


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
