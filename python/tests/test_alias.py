"""Well-known aliases — a declared `alias` meta makes an agent reachable by a
stable name through `send` (like the built-in `kernel`→root alias). Used so a
JS bridge reaches `web/kernel_state` as `web_loader` without knowing its hex id.

Declared config, not automation: the operator sets `alias=…`; the substrate
(`Kernel.register`) wires it into `well_known`; `Agent.send` resolves it.
"""

from __future__ import annotations

from _testkit import boot_root, persist


async def test_alias_resolves_to_agent(kernel):
    kernel.create("file_bridge.tools", id="store_x", alias="store")
    # wired into well_known on register
    assert kernel.ctx.well_known.get("store") == "store_x"
    # send by alias reaches the agent
    r = await kernel.send("store", {"type": "reflect"})
    assert r["id"] == "store_x"


async def test_literal_id_unaffected_by_aliases(kernel):
    kernel.create("file_bridge.tools", id="real", alias="real_alias")
    # a literal id that is NOT an alias resolves to itself
    r = await kernel.send("real", {"type": "reflect"})
    assert r["id"] == "real"


async def test_alias_cleared_on_delete(kernel):
    kernel.create("file_bridge.tools", id="a1", alias="myalias")
    assert "myalias" in kernel.ctx.well_known
    await kernel.delete("a1")
    assert "myalias" not in kernel.ctx.well_known
    # a dangling alias errors cleanly (resolves to itself → no such agent)
    r = await kernel.send("myalias", {"type": "reflect"})
    assert "error" in r


def test_alias_survives_reboot(tmp_path, monkeypatch):
    """A persisted `alias` meta re-wires on load — the loader rehydrates the
    record and `register` sees the alias again."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    k1.create("file_bridge.tools", id="aliased", alias="myname")
    persist(k1)
    del k1
    k2 = boot_root()
    assert k2.ctx.well_known.get("myname") == "aliased"
