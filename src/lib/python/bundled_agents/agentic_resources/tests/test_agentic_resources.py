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

WRITER = "---\nname: writer\npurpose: Writes the first draft of a text.\ntier: fast\nmodel: from-the-card\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n---\nYou write clearly and briefly.\n"
REVIEWER = "---\nname: reviewer\npurpose: Reads a draft and says what to fix.\ntier: fast\n---\nYou review drafts.\n"
SUBLEAD = "---\nname: sublead\npurpose: Runs a branch of the work.\ntier: fast\ngrants:\n  $registry: [roles, spawn, get_count, report]\n---\nYou run one branch.\n"


@pytest.fixture(autouse=True)
def _on(monkeypatch):
    monkeypatch.setenv("FANTASTIC_AGENTIC_RESOURCES", "1")


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


async def rig(kernel, tmp_path, callers, files):
    d = tmp_path / "roles"
    write_cards(d, files)
    reg = "ar_reg"
    for c in callers:
        await create(
            kernel, handler_module="caller.tools", id=c, can_call={reg: "*"}
        )
    tier = {"handler_module": "caller.tools", "model": "m1", "endpoint": "http://x"}
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
    assert m["can_call"][r.reg] == ["roles", "report"]


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
    assert f["roles"]["writer"][0]["state"] == "running"


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


async def test_spawn_task_line_stored(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    a = await r.spawn_as("lead", role="writer", task={"label": "draft intro"})
    assert r.meta(a["id"])["task"] == {
        "label": "draft intro",
        "state": "running",
        "waits_on": [],
        "result": None,
    }
    b = await r.spawn_as(
        "lead", role="writer", task={"label": "polish", "waits_on": [a["id"]]}
    )
    t = r.meta(b["id"])["task"]
    assert t["state"] == "waiting" and t["waits_on"] == [a["id"]] and t["result"] is None
    assert (await r.spawn_as("lead", role="writer", task={}))["reason"] == "bad_arg"
    c = await r.spawn_as("lead", role="writer", task="do it")
    assert "task" not in r.meta(c["id"])
    # the string is the child's first message, delivered without holding the spawn
    for _ in range(100):
        if (c["id"], "do it") in SENT:
            break
        await asyncio.sleep(0.02)
    assert (c["id"], "do it") in SENT


async def test_report_sets_state_and_messages_parent(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead", "other"], [("writer.md", WRITER)])
    inbox = kernel.ctx.ensure_inbox("lead")
    cid = (await r.spawn_as("lead", role="writer", task={"label": "x"}))["id"]
    rep = await r.call(
        cid, r.reg, {"type": "report", "state": "done", "result": {"words": 120}}
    )
    assert rep == {"ok": True}, rep
    t = r.meta(cid)["task"]
    assert (t["state"], t["result"], t["label"]) == ("done", {"words": 120}, "x")
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
    assert r.meta(cid)["task"]["state"] == "done"


def _find(v, aid):
    if isinstance(v, dict):
        if v.get("id") == aid:
            return v
        for c in v.values():
            f = _find(c, aid)
            if f is not None:
                return f
    elif isinstance(v, list):
        for c in v:
            f = _find(c, aid)
            if f is not None:
                return f
    return None


async def test_task_tree_visible_in_reflect_tree_all(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    a = (await r.spawn_as("lead", role="writer", task={"label": "intro"}))["id"]
    b = (
        await r.spawn_as("lead", role="writer", task={"label": "body", "waits_on": [a]})
    )["id"]
    tree = await send(kernel, "kernel_state", {"type": "reflect", "tree": "all"})
    lead = _find(tree, "lead")
    assert lead is not None
    assert _find(lead, a)["task"]["label"] == "intro"
    assert _find(lead, b)["task"]["state"] == "waiting"


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


async def test_get_count_counts_active_only(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    await r.op({"type": "set_limit", "max": 2})
    a = (await r.spawn_as("lead", role="writer", task={"label": "a"}))["id"]
    b = (
        await r.spawn_as("lead", role="writer", task={"label": "b", "waits_on": [a]})
    )["id"]
    assert (await r.count())["running"] == 2, "running and waiting both count"
    assert (await r.spawn_as("lead", role="writer"))["error"] == "limit"
    await r.call(a, r.reg, {"type": "report", "state": "paused"})
    assert await r.count() == {"running": 1, "max": 2}
    assert "id" in await r.spawn_as("lead", role="writer"), "a freed slot"
    await r.call(b, r.reg, {"type": "report", "state": "done"})
    assert (await r.count())["running"] == 1
    await r.call(a, r.reg, {"type": "report", "state": "running"})
    assert (await r.count())["running"] == 2, "resumed work uses a slot again"


async def test_no_auto_cleanup_on_done(kernel, tmp_path):
    r = await rig(kernel, tmp_path, ["lead"], [("writer.md", WRITER)])
    a = (await r.spawn_as("lead", role="writer", task={"label": "a"}))["id"]
    b = (await r.spawn_as("lead", role="writer"))["id"]
    before = len(kernel.ctx.agents)
    await r.call(a, r.reg, {"type": "report", "state": "done", "result": "ok"})
    await r.call(b, r.reg, {"type": "report", "state": "failed", "result": "no"})
    assert len(kernel.ctx.agents) == before, "nothing was deleted"
    m = r.meta(a)
    assert (m["spawned_by"], m["role"]) == (r.reg, "writer")
    assert (m["task"]["label"], m["task"]["state"]) == ("a", "done")
    assert (await r.count())["running"] == 0, "the count dropped"
    f = await r.op({"type": "fleet"})
    assert len(f["roles"]["writer"]) == 2, f
    gone = await send(kernel, "kernel_state", {"type": "delete_agent", "id": a})
    assert gone["deleted"] is True
    assert len(kernel.ctx.agents) == before - 1


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
