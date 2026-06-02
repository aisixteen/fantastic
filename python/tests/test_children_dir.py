"""Configurable children-container dir — `<agent>/<children_dir>/<child>`.

Declared config (default "agents"), not hardcoded: the root record's
`children_dir` meta drives the kernel + the loader, so a self-describing layout
can use `host_agents/` (host tree) and `web_agents/` (a web_loader namespace).
The JS kernel is the same kernel — it carries the field too (its loader lays out
the disk). Mirrors how `alias`/`root`/`watch` are declared config the substrate
wires.
"""

from __future__ import annotations

from _testkit import persist
from fs_loader.tools import read_tree, write_record
from kernel import Kernel


def test_default_children_dir_is_agents(kernel):
    assert kernel.ctx.children_dir == "agents"
    kernel.create("file.tools", id="x")
    p = kernel.ctx.agents["x"]._root_path
    assert p.name == "x" and p.parent.name == "agents"


def test_custom_children_dir_round_trips(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root_dir = tmp_path / ".fantastic"
    # seed a root that DECLARES children_dir = host_agents
    write_record(
        root_dir,
        {
            "id": "fs_loader",
            "handler_module": "fs_loader.tools",
            "children_dir": "host_agents",
        },
    )

    k = Kernel()
    k.load(read_tree(root_dir), root_path=root_dir)
    assert k.children_dir == "host_agents"

    # a child nests under host_agents/, not agents/
    k.create("file.tools", id="f1", x=7)
    persist(k.root)
    assert (root_dir / "host_agents" / "f1" / "agent.json").exists()
    assert not (root_dir / "agents").exists()

    # reboot rehydrates from host_agents/
    k2 = Kernel()
    k2.load(read_tree(root_dir), root_path=root_dir)
    assert k2.children_dir == "host_agents"
    assert k2.get("f1")["x"] == 7
