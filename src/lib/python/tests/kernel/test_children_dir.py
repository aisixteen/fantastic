"""Configurable children-container dir — `<agent>/<children_dir>/<child>`.

Declared config (default "agents"), not hardcoded: the root record's
`children_dir` meta drives the kernel + the loader, so a self-describing layout
can use `host_agents/` (host tree) and `web_agents/` (a web_loader namespace).
The JS kernel is the same kernel — it carries the field too (its loader lays out
the disk). Mirrors how `root`/`watch` are declared config the substrate
wires.
"""

from __future__ import annotations

from _testkit import persist
from file_bridge import fs
from kernel_state.tools import read_tree, write_record
from kernel import Kernel


def test_default_children_dir_is_agents(kernel):
    assert kernel.ctx.children_dir == "agents"
    kernel.create("file_bridge.tools", allow=["."], id="x")
    p = kernel.ctx.agents["x"]._root_path
    assert p.name == "x" and p.parent.name == "agents"


def test_custom_children_dir_round_trips(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root_dir = tmp_path / ".fantastic"
    # seed a root that DECLARES children_dir = host_agents
    write_record(
        fs.Root(tmp_path, root_dir),
        {
            "id": "kernel_state",
            "handler_module": "kernel_state.tools",
            "children_dir": "host_agents",
        },
    )

    k = Kernel()
    k.load(read_tree(fs.Root(tmp_path, root_dir)), root_path=root_dir)
    assert k.children_dir == "host_agents"

    # a child nests under host_agents/, not agents/
    k.create("file_bridge.tools", allow=["."], id="f1", x=7)
    persist(k.root)
    assert (root_dir / "host_agents" / "f1" / "agent.yaml").exists()
    assert not (root_dir / "agents").exists()

    # reboot rehydrates from host_agents/
    k2 = Kernel()
    k2.load(read_tree(fs.Root(tmp_path, root_dir)), root_path=root_dir)
    assert k2.children_dir == "host_agents"
    assert k2.get("f1")["x"] == 7


async def _call(kernel, payload: dict) -> dict:
    return (await kernel.send([{"target": "kernel", "payload": payload}]))[0]["result"]


_BAD_CHILDREN_DIRS = [
    ("../sibling", "a single path segment (no separators); got '../sibling'"),
    ("a/b", "a single path segment (no separators); got 'a/b'"),
    ("..", "a name that is not a path traversal; got '..'"),
    (".hidden", "a name that is not hidden; got '.hidden'"),
    ("", "a non-empty string"),
    (None, "a non-empty string"),
]


async def test_children_dir_is_one_safe_segment_at_create(kernel):
    """ARCH2-6: `children_dir` becomes a directory, so the write holds it to the
    agent-id rule (`guard_agent_id`). Only `isinstance(str)` was checked: a
    `../x` was accepted, and the next boot's clamped walk refused the tree."""
    for value, want in _BAD_CHILDREN_DIRS:
        r = await _call(
            kernel,
            {
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "file_bridge.tools",
                "id": "x",
                "children_dir": value,
            },
        )
        assert r == {
            "error": f"create_agent: children_dir must be {want}",
            "reason": "bad_arg",
            "arg": "children_dir",
        }, r
        assert "x" not in kernel.ctx.agents
    ok = await _call(
        kernel,
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "file_bridge.tools",
            "id": "x",
            "children_dir": "web_agents",
        },
    )
    assert ok["children_dir"] == "web_agents", ok


async def test_children_dir_is_one_safe_segment_at_update(kernel):
    for value, want in _BAD_CHILDREN_DIRS:
        r = await _call(
            kernel,
            {"type": "update_agent", "id": "kernel_state", "children_dir": value},
        )
        assert r == {
            "error": f"update_agent: children_dir must be {want}",
            "reason": "bad_arg",
            "arg": "children_dir",
        }, r
        assert "children_dir" not in kernel.ctx.root.record
    ok = await _call(
        kernel,
        {"type": "update_agent", "id": "kernel_state", "children_dir": "host_agents"},
    )
    assert ok["updated"] is True and ok["agent"]["children_dir"] == "host_agents"


def _seed(path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def test_cold_walk_refuses_an_escaping_children_dir(tmp_path):
    """The clamped cold walk (`fs`) refuses a children_dir that resolves outside
    `.fantastic` — loud, the boot fails — instead of hydrating records from
    another directory. Rust and swift refuse the same input with the same text."""
    work = tmp_path / "work"
    _seed(
        work / ".fantastic" / "agent.yaml",
        "id: kernel_state\nchildren_dir: ../sibling\n",
    )
    _seed(work / "sibling" / "probe_a" / "agent.yaml", "id: probe_a\n")
    try:
        read_tree(fs.Root(work, ".fantastic"))
    except ValueError as e:
        assert str(e).startswith("fs: path '../sibling' escapes root")
    else:
        raise AssertionError("an escaping children_dir must refuse")


def test_cold_walk_refuses_an_outward_symlinked_entry(tmp_path):
    work = tmp_path / "work"
    _seed(work / ".fantastic" / "agent.yaml", "id: kernel_state\n")
    _seed(tmp_path / "outside" / "agent.yaml", "id: probe_out\n")
    (work / ".fantastic" / "agents").mkdir()
    (work / ".fantastic" / "agents" / "evil").symlink_to(tmp_path / "outside")
    try:
        read_tree(fs.Root(work, ".fantastic"))
    except ValueError as e:
        assert str(e).startswith("fs: path 'agents/evil/agent.yaml' escapes root")
    else:
        raise AssertionError("an entry linked outside .fantastic must refuse")


def test_cold_walk_follows_an_inward_symlinked_entry(tmp_path):
    work = tmp_path / "work"
    fdir = work / ".fantastic"
    _seed(fdir / "agent.yaml", "id: kernel_state\n")
    _seed(fdir / "stash" / "a" / "agent.yaml", "id: a\n")
    (fdir / "agents").mkdir()
    (fdir / "agents" / "a").symlink_to(fdir / "stash" / "a")
    recs = read_tree(fs.Root(work, ".fantastic"))
    assert [(r["id"], r.get("parent_id")) for r in recs] == [
        ("kernel_state", None),
        ("a", "kernel_state"),
    ]
