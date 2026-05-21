"""Substrate primer (root reflect) — must self-describe transports,
bundles, agents tree, binary_protocol, and browser_bus enough that
one reflect round-trip bootstraps a remote caller.
"""

from __future__ import annotations


async def test_reflect_includes_binary_protocol(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "binary_protocol" in r
    bp = r["binary_protocol"]
    assert "trigger" in bp
    assert "wire_format" in bp
    assert "header_field" in bp
    assert "_binary_path" in bp["header_field"]


async def test_reflect_includes_browser_bus(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "browser_bus" in r
    bb = r["browser_bus"]
    assert bb["channel"] == "fantastic"
    assert "BroadcastChannel" in bb["transport"]
    assert "bus" in bb["available_in_js"]


async def test_reflect_browser_bus_envelope_documented(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    bb = r["browser_bus"]
    for k in ("type", "target_id", "source_id"):
        assert k in bb["envelope"]


async def test_reflect_transports_in_process(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "transports" in r
    ip = r["transports"]["in_process"]
    assert "agent.send" in ip["shape"]
    assert "use_when" in ip


async def test_reflect_transports_in_prompt(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    inp = r["transports"]["in_prompt"]
    assert "<send" in inp["shape"]
    assert inp.get("example", "").startswith("<send")


async def test_reflect_transports_cli(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    cli = r["transports"]["cli"]
    # The one-shot form is `fantastic <id> <verb>` — there is no
    # `call` subcommand (that token would be read as an agent id).
    assert cli["shape"].startswith("fantastic <agent_id> <verb>")
    assert cli["shorthand"].startswith("fantastic reflect")


async def test_reflect_drops_top_level_send_syntax(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "send_syntax" not in r
    assert "example" not in r


async def test_reflect_available_bundles_lists_workspace(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    bundles = r["available_bundles"]
    assert isinstance(bundles, list)
    assert len(bundles) >= 2
    names = {b["name"] for b in bundles}
    assert "cli" in names
    assert "file" in names
    # sorted
    sorted_names = [b["name"] for b in bundles]
    assert sorted_names == sorted(sorted_names)
    for b in bundles:
        assert set(b.keys()) == {"name", "handler_module"}
        assert b["handler_module"].endswith(".tools")


async def test_reflect_tree_includes_seeded_agents(seeded_kernel):
    """Root primer's `tree` field shows root + all descendants in
    nested form. With the seeded fixture (cli singleton), root has at
    least one child."""
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    tree = r["tree"]
    assert tree["id"] == "core"
    child_ids = {c["id"] for c in tree.get("children", [])}
    assert "cli" in child_ids


async def test_reflect_well_known_present(seeded_kernel):
    """`well_known` is the named-singleton index. Empty by default; bundles
    that want to publish themselves write to ctx.well_known on boot."""
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "well_known" in r
    assert isinstance(r["well_known"], dict)
