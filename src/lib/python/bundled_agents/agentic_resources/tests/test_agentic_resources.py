"""Unit tests — the Python twin of the rust crate's `tests.rs`; every test is named
`test_<rust test>`. Driven through `kernel.send`, as production is. A `caller.tools`
agent stands for any agent that makes calls (a lead, a worker)."""

from __future__ import annotations

import asyncio
import types
from pathlib import Path

import pytest

from _testkit import boot_root, persist
from agentic_resources import cards, tools
from kernel import register_unlisted
from kernel._bundles import bundle_enabled


#: Every `send` a caller agent received: (agent, text).
SENT: list = []


async def _caller_handler(id, payload, kernel):
    """Make the call it is told to (`call {target, payload}`) AS ITSELF, so the gate
    and `spawn`'s caller see a real agent. Every other verb answers ok."""
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "a caller"}
    if verb == "call":
        out = await kernel.send(
            [{"target": payload["target"], "payload": payload["payload"]}]
        )
        return out[0]["result"]
    if verb == "send":
        SENT.append((id, payload.get("text")))
    return {"ok": True, "verb": verb}


register_unlisted("caller.tools", types.SimpleNamespace(handler=_caller_handler))


#: Child ids that are paused, and ids whose `status` is refused (the stub keeps them
#: in the process, which stands for the persisted pause row).
PAUSED: list = []
REFUSE_STATUS: list = []


async def _pausable_handler(id, payload, kernel):
    """Stands for ai_core's pause: `pause`/`resume` flip a flag, `status` replies
    `{paused}`, and `resume` asks the registry first (`get_count {resume}`), a `limit`
    refusal passing through, as ai_core does. Everything else is a caller."""
    verb = payload.get("type")
    if verb == "status":
        if id in REFUSE_STATUS:
            return {"error": "status: refused"}
        return {"paused": id in PAUSED}
    if verb == "pause":
        if id not in PAUSED:
            PAUSED.append(id)
        return {"paused": True, "held": 0}
    if verb == "resume":
        meta = kernel._meta
        reg = meta.get("spawned_by")
        if id in PAUSED and reg:
            r = (
                await kernel.send(
                    [
                        {
                            "target": reg,
                            "payload": {
                                "type": "get_count",
                                "role": meta.get("role"),
                                "resume": id,
                            },
                        }
                    ]
                )
            )[0]["result"]
            if r.get("error") == "limit":
                return r
        if id in PAUSED:
            PAUSED.remove(id)
        return {"resumed": True, "released": 0}
    return await _caller_handler(id, payload, kernel)


register_unlisted("pausable.tools", types.SimpleNamespace(handler=_pausable_handler))


async def _forkable_handler(id, payload, kernel):
    """Stands for ai_core without the dependency: `fork {at}` makes a SIBLING with the
    source's record minus `callable_by`/`delete_lock`. `at` 99 is refused `cuts_turn`,
    98 `no_such_row`."""
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "send":
        SENT.append((id, payload.get("text")))
        return {"ok": True, "verb": "send"}
    if verb != "fork":
        return None
    at = payload.get("at")
    if at in (99, 98):
        reason = "cuts_turn" if at == 99 else "no_such_row"
        return {
            "error": f"fork: refused ({reason})",
            "reason": reason,
            "hint": "a hint",
            "at": at,
        }
    meta = dict(kernel._meta)
    for k in ("callable_by", "delete_lock"):
        meta.pop(k, None)
    meta["forked_from"] = id
    meta.update(
        {
            "type": "create_agent",
            "handler_module": "forkable.tools",
            "parent_id": kernel.parent.id,
        }
    )
    made = await kernel.parent._verb_create_agent(meta)
    return {"id": made["id"], "copied": 3, "at": at}


register_unlisted("forkable.tools", types.SimpleNamespace(handler=_forkable_handler))

def _picky_validate(meta, record):
    """A tool whose record refuses every callable_by edit while it carries
    `refuse_grants` (stands for a bundle whose `validate_meta` says no)."""
    if "callable_by" in meta and record.get("refuse_grants") is True:
        return "callable_by refused"
    return None


register_unlisted(
    "picky.tools",
    types.SimpleNamespace(handler=_caller_handler, validate_meta=_picky_validate),
)

WRITER = "---\nname: writer\npurpose: Writes the first draft of a text.\ntier: fast\nmodel: from-the-card\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n---\nYou write clearly and briefly.\n"
REVIEWER = "---\nname: reviewer\npurpose: Reads a draft and says what to fix.\ntier: fast\n---\nYou review drafts.\n"
SUBLEAD = "---\nname: sublead\npurpose: Runs a branch of the work.\ntier: fast\ngrants:\n  $registry: [roles, spawn, get_count, report]\n  notes: [read]\n---\nYou run one branch.\n"


@pytest.fixture(autouse=True)
def _on(monkeypatch):
    monkeypatch.setenv("FANTASTIC_AGENTIC_RESOURCES", "1")


@pytest.fixture(autouse=True)
def _clean_pause_state():
    PAUSED.clear()
    REFUSE_STATUS.clear()


def write_cards(d: Path, files: list[tuple[str, str]]) -> None:
    d.mkdir(parents=True, exist_ok=True)
    for n, t in files:
        (d / n).write_text(t, encoding="utf-8")


async def send(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def create(kernel, **payload):
    r = await send(
        kernel,
        "kernel_state",
        {"type": "create_agent", "parent_id": "kernel_state", **payload},
    )
    assert "error" not in r, r
    return r


class Rig:
    def __init__(self, kernel, reg, callers, dir):
        self.kernel, self.reg, self.callers, self.dir = kernel, reg, callers, dir

    async def op(self, payload):
        return await send(self.kernel, self.reg, payload)

    async def call(self, who, target, payload):
        return await send(
            self.kernel, who, {"type": "call", "target": target, "payload": payload}
        )

    async def spawn_as(self, who, **payload):
        return await self.call(who, self.reg, {"type": "spawn", **payload})

    def meta(self, aid):
        return self.kernel.ctx.agents[aid]._meta

    async def count(self, role=None):
        p = {"type": "get_count"}
        if role is not None:
            p["role"] = role
        return await self.op(p)


async def rig(kernel, tmp_path, callers, files, tier_handler="caller.tools"):
    d = tmp_path / "roles"
    write_cards(d, files)
    reg = "ar_reg"
    for c in callers:
        await create(
            kernel,
            handler_module="caller.tools",
            id=c,
            can_call={reg: "*", "notes": "*"},
        )
    tier = {"handler_module": tier_handler, "model": "m1", "endpoint": "http://x"}
    await create(
        kernel,
        handler_module="agentic_resources.tools",
        id=reg,
        role_dirs=[str(d)],
        tiers={"fast": dict(tier), "lead": dict(tier)},
        callable_by={c: "*" for c in callers},
    )
    return Rig(kernel, reg, list(callers), d)


# ── cards ───────────────────────────────────────────────────────────


def test_card_parses_header_and_directive():
    c = cards.parse_card(WRITER, "writer.md")
    assert c["name"] == "writer"
    assert c["purpose"] == "Writes the first draft of a text."
    assert c["tier"] == "fast"
    assert c["grants"]["notes"] == ["read"]
    assert c["grants"]["$registry"] == ["roles", "report"]
    assert c["directive"] == "You write clearly and briefly."


def test_bad_header_refused_with_reason(tmp_path):
    def why(text, f):
        with pytest.raises(ValueError) as e:
            cards.parse_card(text, f)
        return str(e.value)

    assert why("no header here", "a.md") == "card a.md: missing header"
    no_tier = "---\nname: x\npurpose: p\n---\nbody\n"
    assert why(no_tier, "b.md") == "card b.md: tier required"
    bad_grants = "---\nname: x\npurpose: p\ntier: fast\ngrants: [roles]\n---\nbody\n"
    assert (
        why(bad_grants, "c.md")
        == 'card c.md: grants must be a map of "*" or verb lists'
    )
    bad_verbs = "---\nname: x\npurpose: p\ntier: fast\ngrants:\n  notes: 5\n---\nbody\n"
    assert "grants must be a map" in why(bad_verbs, "d.md")
    write_cards(
        tmp_path,
        [("a.md", "no header"), ("b.md", no_tier), ("w1.md", WRITER), ("w2.md", WRITER)],
    )
    loaded, errors = cards.load_dir([str(tmp_path)])
    assert [c["name"] for c in loaded] == ["project_lead", "writer"]
    assert "card a.md: missing header" in errors
    assert "card b.md: tier required" in errors
    assert "card w2.md: duplicate role writer" in errors


async def test_roles_lists_every_card_with_purpose(kernel, tmp_path):
    r = await rig(
        kernel,
        tmp_path,
        [],
        [("writer.md", WRITER), ("reviewer.md", REVIEWER), ("bad.md", "x")],
    )
    out = await r.op({"type": "roles"})
    by = {x["name"]: x for x in out["roles"]}
    assert by["writer"]["purpose"] == "Writes the first draft of a text."
    assert by["reviewer"]["purpose"] == "Reads a draft and says what to fix."
    assert "project_lead" in by, out
    assert out["errors"] == ["card bad.md: missing header"]
    d = await send(kernel, "kernel", {"type": "docs", "of": r.reg, "tier": "detailed"})
    doc = d["verbs"]["roles"]
    assert "writer: Writes the first draft of a text." in doc, d
    assert "project_lead: " in doc
    write_cards(
        r.dir,
        [("extra.md", "---\nname: extra\npurpose: Added later.\ntier: fast\n---\nd\n")],
    )
    again = await r.op({"type": "roles"})
    assert any(x["name"] == "extra" for x in again["roles"])


async def test_role_returns_full_card_and_unknown_is_not_found(kernel, tmp_path):
    r = await rig(kernel, tmp_path, [], [("writer.md", WRITER)])
    c = await r.op({"type": "role", "name": "writer"})
    assert c["name"] == "writer" and c["tier"] == "fast"
    assert c["directive"] == "You write clearly and briefly."
    assert c["grants"]["notes"] == ["read"]
    n = await r.op({"type": "role", "name": "ghost"})
    assert n["reason"] == "not_found", n


# ── spawn ───────────────────────────────────────────────────────────


async def test_spawn_creates_child_with_directive_tier_grants(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    out = await r.spawn_as("lead", role="writer", name="scribe")
    assert out["role"] == "writer" and out["tier"] == "fast", out
    a = kernel.ctx.agents[out["id"]]
    assert a.parent.id == "lead", "a child of the caller"
    m = a._meta
    assert m["system_prompt"] == "You write clearly and briefly."
    assert m["model"] == "m1", "the tier's model, never the card's"
    assert m["endpoint"] == "http://x"
    assert a.handler_module == "caller.tools"
    assert m["spawned_by"] == r.reg and m["role"] == "writer" and m["name"] == "scribe"
    assert m["can_call"]["notes"] == ["read"]
    assert m["can_call"][r.reg] == ["roles", "report", "get_count"]


async def test_spawn_opens_the_tiers_memory_store_to_the_child(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await create(kernel, handler_module="caller.tools", id="mem")
    kernel.update(
        r.reg, tiers={"fast": {"handler_module": "caller.tools", "memory_store_id": "mem"}}
    )
    cid = (await r.spawn_as("lead", role="writer"))["id"]
    assert r.meta(cid)["can_call"]["mem"] == "*", "the child may use its store"
    assert r.meta("mem")["callable_by"][cid] == "*", "and the store accepts the child"


async def test_spawn_unknown_tier_refused(kernel, tmp_path):
    card = "---\nname: odd\npurpose: Odd.\ntier: nowhere\n---\nd\n"
    r = await rig(kernel, tmp_path, ["lead"], [("odd.md", card)])
    out = await r.spawn_as("lead", role="odd")
    assert out["error"] == "unknown_tier", out
    assert "'nowhere'" in out["reason"]
    assert (await r.spawn_as("lead", role="ghost"))["reason"] == "not_found"
    assert (await r.op({"type": "fleet"}))["roles"] == {}


async def test_per_role_limit_refuses_past_cap(kernel, tmp_path):
    r = await rig(
        kernel, tmp_path, ["lead"], [("writer.md", WRITER), ("reviewer.md", REVIEWER)]
    )
    assert (await r.op({"type": "set_limit", "role": "writer", "max": 2}))["ok"] is True
    for _ in range(2):
        assert "id" in await r.spawn_as("lead", role="writer")
    out = await r.spawn_as("lead", role="writer")
    assert out["error"] == "limit", out
    assert out["scope"] == "role" and out["role"] == "writer"
    assert out["running"] == 2 and out["max"] == 2
    assert "writer" in out["reason"]
    assert "id" in await r.spawn_as("lead", role="reviewer")


async def test_total_limit_refuses_past_cap(kernel, tmp_path):
    r = await rig(
        kernel, tmp_path, ["lead"], [("writer.md", WRITER), ("reviewer.md", REVIEWER)]
    )
    await r.op({"type": "set_limit", "max": 2})
    assert "id" in await r.spawn_as("lead", role="writer")
    assert "id" in await r.spawn_as("lead", role="reviewer")
    out = await r.spawn_as("lead", role="writer")
    assert out["error"] == "limit" and out["scope"] == "total", out
    assert (out["running"], out["max"]) == (2, 2)
    assert await r.count() == {"running": 2, "max": 2}


async def test_set_limit_null_clears(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "role": "writer", "max": 1})
    await r.spawn_as("lead", role="writer")
    assert (await r.spawn_as("lead", role="writer"))["error"] == "limit"
    assert (await r.op({"type": "set_limit", "role": "writer", "max": None}))["ok"] is True
    assert await r.count("writer") == {"running": 1, "max": None}
    assert "id" in await r.spawn_as("lead", role="writer")
    for bad in (-1, 1.5, "2", True):
        out = await r.op({"type": "set_limit", "max": bad})
        assert out["reason"] == "bad_arg", (bad, out)
    assert (await r.op({"type": "set_limit"}))["reason"] == "missing_arg"


async def test_get_count_after_spawn_delete_restart(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "role": "writer", "max": 5})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    await r.spawn_as("lead", role="writer")
    assert await r.count("writer") == {"running": 2, "max": 5}
    gone = await send(kernel, "kernel_state", {"type": "delete_agent", "id": a})
    assert gone["deleted"] is True, gone
    assert (await r.count("writer"))["running"] == 1
    # a restart: the tree is rebuilt from the persisted records
    persist(kernel)
    k2 = boot_root()
    after = await send(k2, r.reg, {"type": "get_count", "role": "writer"})
    assert after == {"running": 1, "max": 5}, "rebuilt from the records"
    assert (await send(k2, r.reg, {"type": "get_count"}))["running"] == 1


async def test_fleet_groups_by_role_with_ids(kernel, tmp_path):
    r = await rig(
        kernel, tmp_path, ["lead"], [("writer.md", WRITER), ("reviewer.md", REVIEWER)]
    )
    w1 = (await r.spawn_as("lead", role="writer"))["id"]
    w2 = (await r.spawn_as("lead", role="writer"))["id"]
    v1 = (await r.spawn_as("lead", role="reviewer"))["id"]
    await create(kernel, handler_module="caller.tools", id="stray", role="writer")
    f = await r.op({"type": "fleet"})
    assert sorted(x["id"] for x in f["roles"]["writer"]) == sorted([w1, w2]), f
    assert [x["id"] for x in f["roles"]["reviewer"]] == [v1]
    assert set(f["roles"]["writer"][0]) == {"id"}


def test_disabled_by_default(monkeypatch):
    monkeypatch.delenv("FANTASTIC_AGENTIC_RESOURCES", raising=False)
    assert not bundle_enabled("agentic_resources.tools")
    for off in ("0", "", "no"):
        monkeypatch.setenv("FANTASTIC_AGENTIC_RESOURCES", off)
        assert not bundle_enabled("agentic_resources.tools")
    for on in ("1", "TRUE", "yes", "on"):
        monkeypatch.setenv("FANTASTIC_AGENTIC_RESOURCES", on)
        assert bundle_enabled("agentic_resources.tools")
    assert bundle_enabled("scheduler.tools"), "other bundles are never gated"


async def test_disabled_without_registry_handler_or_config(kernel, monkeypatch):
    # flag off: the handler is not in the kernel's catalog, so the agent is unknown
    monkeypatch.delenv("FANTASTIC_AGENTIC_RESOURCES", raising=False)
    await create(kernel, handler_module="agentic_resources.tools", id="ar_off")
    out = await send(kernel, "ar_off", {"type": "roles"})
    assert "cannot be imported in this runtime" in out["error"], out
    # flag on, but no config on the record: every verb refuses `disabled`
    monkeypatch.setenv("FANTASTIC_AGENTIC_RESOURCES", "1")
    await create(kernel, handler_module="agentic_resources.tools", id="ar_on")
    for v in ("roles", "role", "spawn", "report", "fleet", "set_limit", "get_count"):
        out = await send(kernel, "ar_on", {"type": v})
        assert out == {"error": "agentic_resources is disabled"}, v


# ── the extended surface ────────────────────────────────────────────


async def test_spawn_grants_parent_pause_resume_stop_both_ends(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead", "sib"], [("writer.md", REVIEWER)])
    cid = (await r.spawn_as("lead", role="reviewer"))["id"]
    want = ["pause", "resume", "stop"]
    assert r.meta("lead")["can_call"][cid] == want
    assert r.meta(cid)["callable_by"]["lead"] == want
    assert sorted(r.meta(cid)["callable_by"]) == sorted(["$self", "lead"])
    for verb in want:
        out = await r.call("lead", cid, {"type": verb})
        assert out["verb"] == verb, out
    assert (await r.call("lead", cid, {"type": "send"}))["reason"] == "unauthorized"
    assert (await r.call("sib", cid, {"type": "pause"}))["reason"] == "unauthorized"


async def test_spawn_task_string_is_first_message_and_object_refused(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    before = len(kernel.ctx.agents)
    bad = await r.spawn_as("lead", role="writer", task={"label": "draft intro"})
    assert bad["reason"] == "bad_arg", bad
    assert len(kernel.ctx.agents) == before, "nothing was created"
    c = await r.spawn_as("lead", role="writer", task="do it")
    assert "task" not in r.meta(c["id"])
    # the string is the child's first message, delivered without holding the spawn
    for _ in range(100):
        if (c["id"], "do it") in SENT:
            break
        await asyncio.sleep(0.02)
    assert (c["id"], "do it") in SENT


async def test_report_messages_parent_and_stores_nothing(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead", "other"], [("writer.md", WRITER)])
    inbox = kernel.ctx.ensure_inbox("lead")
    cid = (await r.spawn_as("lead", role="writer"))["id"]
    rep = await r.call(
        cid, r.reg, {"type": "report", "state": "done", "result": {"words": 120}}
    )
    assert rep == {"ok": True}, rep
    assert "task" not in r.meta(cid), "nothing is stored"
    got = None
    while not inbox.empty():
        item = inbox.get_nowait()
        if isinstance(item, dict) and item.get("type") == "report":
            got = item
    assert got == {
        "type": "report",
        "from": cid,
        "state": "done",
        "result": {"words": 120},
    }
    no = await r.call("other", r.reg, {"type": "report", "state": "done"})
    assert no["error"] == "not_spawned", no
    assert (await r.op({"type": "report", "state": "done"}))["error"] == "not_spawned"
    bad = await r.call(cid, r.reg, {"type": "report", "state": "finished"})
    assert bad["reason"] == "bad_arg", bad
    assert "task" not in r.meta(cid)


async def test_delegated_limits_bound_subtree(kernel, tmp_path):
    r = await rig(
        kernel, tmp_path, ["boss"], [("writer.md", WRITER), ("sublead.md", SUBLEAD)]
    )
    sub = (await r.spawn_as("boss", role="sublead", limits={"total": 2}))["id"]
    assert r.meta(sub)["limits"] == {"total": 2}
    big = await r.spawn_as(sub, role="sublead", limits={"total": 5})
    assert (big["error"], big["scope"]) == ("limit", "request"), big
    ok = await r.spawn_as(sub, role="sublead", limits={"total": 1})
    assert "id" in ok, ok
    assert "id" in await r.spawn_as(sub, role="writer")
    third = await r.spawn_as(sub, role="writer")
    assert third["error"] == "limit" and third["scope"] == "subtree_total", third
    assert (third["running"], third["max"]) == (2, 2)
    blocked = await r.spawn_as(ok["id"], role="writer")
    assert blocked["error"] == "limit", blocked
    assert "id" in await r.spawn_as("boss", role="writer")


async def settle(r, running):
    """Poll until `get_count` (total) reports `running` (the pause on a report is detached)."""
    for _ in range(100):
        if (await r.count())["running"] == running:
            return
        await asyncio.sleep(0.02)


async def pr(kernel, tmp_path, callers, files):
    return await rig(kernel, tmp_path, callers, files, "pausable.tools")


async def deleted(r, aid):
    return await send(r.kernel, "kernel_state", {"type": "delete_agent", "id": aid})


async def test_get_count_counts_active_agents(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "max": 2})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    await r.spawn_as("lead", role="writer")
    assert await r.count() == {"running": 2, "max": 2}
    third = await r.spawn_as("lead", role="writer")
    assert third["error"] == "limit", third
    # a done report frees the slot (the reporter is paused)
    await r.call(a, r.reg, {"type": "report", "state": "done"})
    await settle(r, 1)
    assert (await r.count())["running"] == 1
    assert "id" in await r.spawn_as("lead", role="writer")
    assert (await r.count())["running"] == 2
    # delete_agent of the paused one changes nothing more
    assert (await deleted(r, a))["deleted"] is True
    assert (await r.count())["running"] == 2


async def test_no_auto_cleanup_on_done(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    a = (await r.spawn_as("lead", role="writer"))["id"]
    b = (await r.spawn_as("lead", role="writer"))["id"]
    before = len(kernel.ctx.agents)
    await r.call(a, r.reg, {"type": "report", "state": "done", "result": "ok"})
    await r.call(b, r.reg, {"type": "report", "state": "failed", "result": "no"})
    await settle(r, 0)
    assert len(kernel.ctx.agents) == before, "nothing was deleted"
    m = r.meta(a)
    assert (m["spawned_by"], m["role"]) == (r.reg, "writer")
    assert "task" not in m
    assert (await r.count())["running"] == 0, "both are paused"
    f = await r.op({"type": "fleet"})
    assert len(f["roles"]["writer"]) == 2, f
    assert (await deleted(r, a))["deleted"] is True
    assert len(kernel.ctx.agents) == before - 1


async def test_pause_frees_a_slot(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "max": 2})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    await r.spawn_as("lead", role="writer")
    assert (await r.spawn_as("lead", role="writer"))["error"] == "limit"
    p = await r.call("lead", a, {"type": "pause"})
    assert p["paused"] is True, p
    assert await r.count() == {"running": 1, "max": 2}
    assert "id" in await r.spawn_as("lead", role="writer")


async def test_resume_takes_the_slot_back_and_is_refused_when_full(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "max": 2})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    await r.spawn_as("lead", role="writer")
    await r.call("lead", a, {"type": "pause"})
    c = (await r.spawn_as("lead", role="writer"))["id"]
    assert (await r.count())["running"] == 2
    no = await r.call("lead", a, {"type": "resume"})
    assert no["error"] == "limit", no
    assert no["reason"], no
    assert a in PAUSED, "still paused"
    assert (await r.count())["running"] == 2
    await r.call("lead", c, {"type": "pause"})
    ok = await r.call("lead", a, {"type": "resume"})
    assert ok["resumed"] is True, ok
    assert await r.count() == {"running": 2, "max": 2}


async def test_done_report_frees_the_slot(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    a = (await r.spawn_as("lead", role="writer"))["id"]
    b = (await r.spawn_as("lead", role="writer"))["id"]
    # these states free nothing
    for state in ("waiting", "running", "blocked", "paused"):
        rep = await r.call(a, r.reg, {"type": "report", "state": state})
        assert rep == {"ok": True}, rep
    await asyncio.sleep(0.15)
    assert (await r.count())["running"] == 2
    rep = await r.call(a, r.reg, {"type": "report", "state": "done"})
    assert rep == {"ok": True}, rep
    await settle(r, 1)
    assert (await r.count())["running"] == 1
    await r.call(b, r.reg, {"type": "report", "state": "failed"})
    await settle(r, 0)
    assert (await r.count())["running"] == 0


async def test_done_agent_frees_without_task_registry(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    assert not any(
        "task_registry" in str(a._meta.get("handler_module"))
        for a in kernel.ctx.agents.values()
    ), "no task_registry in this kernel"
    await r.op({"type": "set_limit", "max": 1})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    assert (await r.spawn_as("lead", role="writer"))["error"] == "limit"
    await r.call(a, r.reg, {"type": "report", "state": "done"})
    await settle(r, 0)
    assert "id" in await r.spawn_as("lead", role="writer")


async def test_per_role_and_subtree_quota_count_active_only(kernel, tmp_path):
    r = await pr(
        kernel, tmp_path, ["boss"], [("writer.md", WRITER), ("sublead.md", SUBLEAD)]
    )
    await r.op({"type": "set_limit", "role": "writer", "max": 1})
    sub = (await r.spawn_as("boss", role="sublead", limits={"total": 1}))["id"]
    w1 = (await r.spawn_as(sub, role="writer"))["id"]
    assert (await r.spawn_as(sub, role="writer"))["scope"] == "role"
    # pausing the leaf frees the role slot and the sub-lead's subtree slot
    await r.call(sub, w1, {"type": "pause"})
    w2 = (await r.spawn_as(sub, role="writer"))["id"]
    # the role cap is full again: resume is refused `role`; lift it, then `subtree_total`
    no = await r.call(sub, w1, {"type": "resume"})
    assert (no["error"], no["scope"]) == ("limit", "role"), no
    await r.op({"type": "set_limit", "role": "writer", "max": None})
    no = await r.call(sub, w1, {"type": "resume"})
    assert (no["error"], no["scope"]) == ("limit", "subtree_total"), no
    assert no["lead"] == sub
    await r.call(sub, w2, {"type": "pause"})
    ok = await r.call(sub, w1, {"type": "resume"})
    assert ok["resumed"] is True, ok


async def test_restart_keeps_the_active_counts(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "max": 3})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    await r.spawn_as("lead", role="writer")
    await r.call("lead", a, {"type": "pause"})
    before = await r.count()
    assert before == {"running": 1, "max": 3}
    persist(kernel)
    k2 = boot_root()
    after = await send(k2, r.reg, {"type": "get_count"})
    assert after == before, "rebuilt from the records and the pause state"
    assert (await send(k2, r.reg, {"type": "get_count", "role": "writer"}))[
        "running"
    ] == 1


async def test_status_failure_counts_as_active(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "max": 1})
    a = (await r.spawn_as("lead", role="writer"))["id"]
    REFUSE_STATUS.append(a)
    # even paused, an agent that will not say counts (fail closed)
    PAUSED.append(a)
    assert (await r.count())["running"] == 1
    assert (await r.spawn_as("lead", role="writer"))["error"] == "limit"


async def test_fleet_lists_paused_agents(kernel, tmp_path):
    r = await pr(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    a = (await r.spawn_as("lead", role="writer"))["id"]
    await r.call("lead", a, {"type": "pause"})
    assert (await r.count())["running"] == 0
    f = await r.op({"type": "fleet"})
    assert f["roles"]["writer"] == [{"id": a}], f


async def test_project_lead_card_ships(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["boss"], [])
    c = await r.op({"type": "role", "name": "project_lead"})
    text = f"{c['purpose']} {c['directive']}".lower()
    for want in (
        "roles", "get_count", "spawn", "await_set", "report", "never clean up",
        "fork_points",
    ):
        assert want in text, (want, text)
    for code in ("code", "compile", "repository", "git ", "test suite", "pull request"):
        assert code not in text, f"not code-oriented, found {code!r}"
    for v in ("roles", "role", "spawn", "get_count", "fleet", "report"):
        assert v in c["grants"][cards.REGISTRY_TOKEN], v
    assert "id" in await r.spawn_as("boss", role="project_lead")


def test_readme_is_rust_verbatim():
    here = Path(tools.__file__).parent
    rust = (
        Path(__file__).resolve().parents[5]
        / "lib/rust/crates/bundles/fantastic-agentic-resources"
    )
    assert (here / "readme.md").read_text() == (rust / "src/readme.md").read_text()
    assert (here / "cards/project_lead.md").read_text() == (
        rust / "cards/project_lead.md"
    ).read_text()


# ── spawn from a fork point ─────────────────────────────────────────


async def fork_rig(kernel, tmp_path, callers, files):
    """A rig whose tier carries a `memory_store_id`, plus a source agent that stands
    for an agent with history: its own prompt, store, grants and stale registry keys."""
    r = await rig(kernel, tmp_path, callers, files)
    tier = {
        "handler_module": "caller.tools",
        "model": "m1",
        "endpoint": "http://x",
        "memory_store_id": "tier_store",
    }
    kernel.update(r.reg, tiers={"fast": dict(tier), "lead": dict(tier)})
    await create(
        kernel,
        handler_module="forkable.tools",
        id="src",
        system_prompt="SOURCE PROMPT",
        model="src-model",
        memory_store_id="src_store",
        can_call={"src_store": "*", "secret_agent": "*"},
        spawned_by="stale_reg",
        role="old_role",
        tier="old",
        task={"label": "old task", "state": "done", "waits_on": [], "result": None},
        limits={"total": 9},
        lead="stale_lead",
    )
    for c in r.callers:
        grant_fork(kernel, c, "src")
    return r, "src"


def grant_fork(kernel, who, src):
    def edit(m):
        tools._grant_verbs(m, src, ["fork"])
        return True

    _, err = kernel.ctx.edit_can_call(who, edit)
    assert err is None, err


def forks_of(kernel, src):
    return [
        a.id for a in kernel.ctx.agents.values() if a._meta.get("forked_from") == src
    ]


def frm(src, at):
    return {"agent": src, "at": at}


async def test_spawn_from_forks_and_applies_the_role_card(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    out = await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)})
    assert out["role"] == "writer", out
    assert out["tier"] == "fast"
    assert out["from"] == frm(src, 5)
    assert out["copied"] == 3
    cid = out["id"]
    assert forks_of(kernel, src) == [cid], "made by fork"
    m = r.meta(cid)
    assert m["system_prompt"] == "You write clearly and briefly."
    assert m["role"] == "writer"
    assert m["tier"] == "fast"
    assert m["spawned_by"] == r.reg
    assert m["model"] == "m1", "the tier's model, not the source's"
    assert m["lead"] == "lead"
    assert m["can_call"]["notes"] == ["read"]
    assert m["can_call"][r.reg] == ["roles", "report", "get_count"]
    assert r.meta(r.reg)["callable_by"][cid] == ["roles", "report", "get_count"]
    named = await r.spawn_as("lead", role="writer", name="scribe", **{"from": frm(src, 5)})
    assert r.meta(named["id"])["name"] == "scribe"


async def test_spawn_from_keeps_the_source_store_and_drops_its_other_grants(
    kernel, tmp_path
):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    cid = (await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)}))["id"]
    m = r.meta(cid)
    assert m["memory_store_id"] == "src_store", "not the tier's"
    assert m["can_call"]["src_store"] == "*"
    assert "secret_agent" not in m["can_call"], m["can_call"]
    assert "tier_store" not in m["can_call"]
    assert len(m["can_call"]) == 3, "store, notes, registry"


async def test_spawn_from_drops_the_copied_task_and_overwrites_limits(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    bare = (await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)}))["id"]
    assert "task" not in r.meta(bare), "no task: none carried over"
    assert "limits" not in r.meta(bare)
    given = (
        await r.spawn_as(
            "lead",
            role="writer",
            limits={"per_role": 1},
            **{"from": frm(src, 5)},
        )
    )["id"]
    m = r.meta(given)
    assert "task" not in m
    assert m["limits"] == {"per_role": 1}
    assert m["role"] == "writer", "never the source's role"
    assert r.meta(src)["task"]["label"] == "old task"
    assert r.meta(src)["limits"] == {"total": 9}


async def test_spawn_from_task_string_is_sent_as_first_message(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    out = await r.spawn_as("lead", role="writer", task="branch task", **{"from": frm(src, 5)})
    cid = out["id"]
    for _ in range(100):
        if (cid, "branch task") in SENT:
            break
        await asyncio.sleep(0.02)
    assert (cid, "branch task") in SENT, "the child never got its first message"


async def test_spawn_from_passes_fork_refusals_through(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    for at, reason in ((99, "cuts_turn"), (98, "no_such_row")):
        out = await r.spawn_as("lead", role="writer", **{"from": frm(src, at)})
        assert out["reason"] == reason, out
        assert out["at"] == at
        assert out["hint"] == "a hint"
        assert out["error"].startswith("fork:"), out
        assert forks_of(kernel, src) == []
        assert (await r.op({"type": "fleet"}))["roles"] == {}
        assert (await r.count())["running"] == 0
        assert src not in r.meta(r.reg).get("can_call", {}), "no leftover grant"
        assert r.reg not in r.meta(src).get("callable_by", {})


async def test_spawn_from_respects_limits_before_forking(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "role": "writer", "max": 1})
    first = await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)})
    assert "id" in first, first
    out = await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)})
    assert out["error"] == "limit", out
    assert out["scope"] == "role"
    assert len(forks_of(kernel, src)) == 1, "fork was not called again"


async def test_spawn_from_requires_the_caller_to_hold_fork(kernel, tmp_path):
    r, src = await fork_rig(
        kernel, tmp_path, ["lead", "other"], [("writer.md", WRITER)]
    )
    # `other` loses what the rig gave it
    kernel.ctx.edit_can_call("other", lambda m: m.pop(src, None) is not None)
    out = await r.spawn_as("other", role="writer", **{"from": frm(src, 5)})
    assert out["error"] == "not_allowed", out
    assert "may not fork" in out["reason"], out
    assert forks_of(kernel, src) == []
    assert src not in r.meta(r.reg).get("can_call", {})
    ok = await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)})
    assert "id" in ok, ok


async def test_spawn_from_gives_the_parent_pause_resume_stop(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    cid = (await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)}))["id"]
    assert r.meta("lead")["can_call"][cid] == ["pause", "resume", "stop"]
    assert r.meta(cid)["callable_by"]["lead"] == ["pause", "resume", "stop"]


async def test_spawn_from_counts_in_fleet_and_get_count_and_survives_restart(
    kernel, tmp_path
):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    cid = (await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)}))["id"]
    f = await r.op({"type": "fleet"})
    assert f["roles"]["writer"] == [{"id": cid}]
    assert (await r.count("writer"))["running"] == 1
    persist(kernel)
    k2 = boot_root()
    after = await send(k2, r.reg, {"type": "get_count", "role": "writer"})
    assert after["running"] == 1, "rebuilt from the records"
    gone = await send(kernel, "kernel_state", {"type": "delete_agent", "id": cid})
    assert gone["deleted"] is True, gone
    assert (await r.count("writer"))["running"] == 0


async def test_spawn_from_counts_against_the_leads_subtree_quota(kernel, tmp_path):
    r, src = await fork_rig(
        kernel, tmp_path, ["boss"], [("writer.md", WRITER), ("sublead.md", SUBLEAD)]
    )
    sub = (await r.spawn_as("boss", role="sublead", limits={"total": 1}))["id"]
    grant_fork(kernel, sub, src)
    first = await r.spawn_as(sub, role="writer", **{"from": frm(src, 5)})
    assert "id" in first, first
    second = await r.spawn_as(sub, role="writer", **{"from": frm(src, 5)})
    assert second["error"] == "limit", second
    assert second["scope"] == "subtree_total", second
    assert len(forks_of(kernel, src)) == 1


async def test_spawn_from_bad_shape_refused(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    for bad_from in (
        "x",
        {"at": 1},
        {"agent": src},
        {"agent": src, "at": "1"},
        {"agent": "", "at": 1},
    ):
        out = await r.spawn_as("lead", role="writer", **{"from": bad_from})
        assert "from" in out["error"], (bad_from, out)
        assert "{agent:str, at:int}" in out["error"]
    assert forks_of(kernel, src) == []
    plain = await r.spawn_as("lead", role="writer")
    assert "id" in plain and "from" not in plain, plain


# ── the card's tool grants (#436) ───────────────────────────────────

WIDE = "---\nname: wide\npurpose: Reads and writes notes.\ntier: fast\ngrants:\n  $registry: [roles, report]\n  notes: [read, write]\n---\nYou write.\n"
STAR = '---\nname: star\npurpose: Anything on notes.\ntier: fast\ngrants:\n  notes: "*"\n---\nYou do all.\n'


async def notes_tool(kernel):
    await create(kernel, handler_module="caller.tools", id="notes")


def hold(kernel, who, key, verbs):
    """`who` now holds exactly `verbs` on `key` (replacing the rig's grant)."""

    def edit(m):
        m[key] = verbs
        return True

    _, err = kernel.ctx.edit_can_call(who, edit)
    assert err is None, err


def keys_of(r, aid, field):
    return sorted((r.meta(aid).get(field) or {}).keys())


async def test_spawn_opens_the_cards_tool_grants_on_both_ends(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await notes_tool(kernel)
    before = keys_of(r, "notes", "callable_by")
    cid = (await r.spawn_as("lead", role="writer"))["id"]
    assert r.meta(cid)["can_call"]["notes"] == ["read"]
    assert r.meta("notes")["callable_by"][cid] == ["read"]
    assert keys_of(r, "notes", "callable_by") == sorted(before + [cid])


async def test_spawned_worker_calls_the_granted_tool_without_directive_setup(
    kernel, tmp_path
):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await notes_tool(kernel)
    cid = (await r.spawn_as("lead", role="writer"))["id"]
    ok = await r.call(cid, "notes", {"type": "read"})
    assert ok["verb"] == "read", ok
    no = await r.call(cid, "notes", {"type": "write"})
    assert no["reason"] == "unauthorized", no


async def test_spawn_from_opens_the_cards_tool_grants_on_both_ends(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await notes_tool(kernel)
    hold(kernel, src, "notes", "*")
    before = keys_of(r, "notes", "callable_by")
    cid = (await r.spawn_as("lead", role="writer", **{"from": frm(src, 5)}))["id"]
    assert r.meta(cid)["can_call"]["notes"] == ["read"], "not wider than the card"
    assert r.meta("notes")["callable_by"][cid] == ["read"]
    assert keys_of(r, "notes", "callable_by") == sorted(before + [cid])


async def test_spawn_tool_grant_with_star_verbs_keeps_star(kernel, tmp_path):
    r = await rig(
        kernel, tmp_path, ["lead"], [("star.md", STAR), ("writer.md", WRITER)]
    )
    await notes_tool(kernel)
    a = (await r.spawn_as("lead", role="star"))["id"]
    b = (await r.spawn_as("lead", role="star"))["id"]
    assert r.meta("notes")["callable_by"][a] == "*"
    assert r.meta("notes")["callable_by"][b] == "*"
    w = (await r.spawn_as("lead", role="writer"))["id"]
    assert r.meta("notes")["callable_by"][w] == ["read"]
    assert r.meta("notes")["callable_by"][a] == "*"


async def test_spawn_without_the_tool_agent_still_spawns(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    cid = (await r.spawn_as("lead", role="writer"))["id"]
    assert r.meta(cid)["can_call"]["notes"] == ["read"]
    assert "notes" not in kernel.ctx.agents


async def test_refused_tool_grant_undoes_the_spawn(kernel, tmp_path):
    card = "---\nname: two\npurpose: Two tools.\ntier: fast\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n  zpicky: [read]\n---\nx\n"
    r = await rig(kernel, tmp_path, ["lead"], [("two.md", card)])
    await notes_tool(kernel)
    await create(
        kernel, handler_module="picky.tools", id="zpicky", refuse_grants=True
    )
    hold(kernel, "lead", "zpicky", "*")
    notes_before = dict(r.meta("notes")["callable_by"])
    reg_before = keys_of(r, r.reg, "callable_by")
    lead_before = keys_of(r, "lead", "can_call")
    n = len(kernel.ctx.agents)
    out = await r.spawn_as("lead", role="two", task="go")
    assert out["error"] == "grant_failed", out
    assert len(kernel.ctx.agents) == n, "no child left"
    assert (await r.op({"type": "fleet"}))["roles"] == {}
    assert (await r.count())["running"] == 0
    assert r.meta("notes")["callable_by"] == notes_before, "first edit undone"
    assert keys_of(r, r.reg, "callable_by") == reg_before
    assert keys_of(r, "lead", "can_call") == lead_before


async def test_delete_agent_removes_the_spawned_agents_tool_entries(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await notes_tool(kernel)
    keep = (await r.spawn_as("lead", role="writer"))["id"]
    cid = (await r.spawn_as("lead", role="writer"))["id"]
    assert cid in r.meta("notes")["callable_by"]
    out = await deleted(r, cid)
    assert "error" not in out, out
    assert cid not in r.meta("notes")["callable_by"]
    assert cid not in r.meta(r.reg)["callable_by"]
    assert cid not in r.meta("lead")["can_call"]
    assert r.meta("notes")["callable_by"][keep] == ["read"]


async def test_delete_of_a_subtree_removes_every_spawned_agents_tool_entries(
    kernel, tmp_path
):
    r = await rig(
        kernel, tmp_path, ["lead"], [("sublead.md", SUBLEAD), ("writer.md", WRITER)]
    )
    await notes_tool(kernel)
    sub = (await r.spawn_as("lead", role="sublead"))["id"]
    w = (await r.spawn_as(sub, role="writer"))["id"]
    assert sub in r.meta("notes")["callable_by"] and w in r.meta("notes")["callable_by"]
    await deleted(r, sub)
    assert sub not in r.meta("notes")["callable_by"]
    assert w not in r.meta("notes")["callable_by"]


async def test_spawn_refuses_a_grant_the_spawner_does_not_hold(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("wide.md", WIDE)])
    await notes_tool(kernel)
    hold(kernel, "lead", "notes", ["read"])
    notes_before = dict(r.meta("notes")["callable_by"])
    n = len(kernel.ctx.agents)
    out = await r.spawn_as("lead", role="wide")
    assert out["error"] == "not_allowed", out
    why = out["reason"]
    assert "lead" in why and "write" in why and "notes" in why, why
    assert len(kernel.ctx.agents) == n, "nothing created"
    assert r.meta("notes")["callable_by"] == notes_before
    assert (await r.op({"type": "fleet"}))["roles"] == {}


async def test_spawn_refuses_star_grant_when_spawner_holds_only_verbs(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("star.md", STAR)])
    await notes_tool(kernel)
    hold(kernel, "lead", "notes", ["read"])
    out = await r.spawn_as("lead", role="star")
    assert out["error"] == "not_allowed", out


async def test_spawn_refuses_a_tool_the_spawner_has_no_grant_on(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await notes_tool(kernel)
    kernel.ctx.edit_can_call("lead", lambda m: m.pop("notes", None) is not None)
    out = await r.spawn_as("lead", role="writer")
    assert out["error"] == "not_allowed", out


async def test_spawn_from_refuses_escalation_before_forking(kernel, tmp_path):
    r, src = await fork_rig(kernel, tmp_path, ["lead"], [("wide.md", WIDE)])
    await notes_tool(kernel)
    hold(kernel, "lead", "notes", ["read"])
    out = await r.spawn_as("lead", role="wide", **{"from": frm(src, 5)})
    assert out["error"] == "not_allowed", out
    assert forks_of(kernel, src) == [], "nothing forked"


async def test_operator_spawn_has_no_escalation_check(kernel, tmp_path):
    r = await rig(kernel, tmp_path, [], [("wide.md", WIDE)])
    await notes_tool(kernel)
    cid = (await r.op({"type": "spawn", "role": "wide"}))["id"]
    assert r.meta(cid)["can_call"]["notes"] == ["read", "write"]
    assert r.meta("notes")["callable_by"][cid] == ["read", "write"]


async def test_subleads_cannot_hand_on_more_than_they_hold(kernel, tmp_path):
    r = await rig(
        kernel,
        tmp_path,
        ["lead"],
        [("sublead.md", SUBLEAD), ("wide.md", WIDE), ("writer.md", WRITER)],
    )
    await notes_tool(kernel)
    sub = (await r.spawn_as("lead", role="sublead"))["id"]
    no = await r.spawn_as(sub, role="wide")
    assert no["error"] == "not_allowed", no
    ok = await r.spawn_as(sub, role="writer")
    assert "id" in ok, ok


async def test_registry_token_grants_are_not_escalation_checked(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [])
    hold(kernel, "lead", r.reg, ["spawn"])
    out = await r.spawn_as("lead", role="project_lead")
    assert "id" in out, out
