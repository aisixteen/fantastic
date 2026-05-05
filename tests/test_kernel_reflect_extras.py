"""kernel.reflect must self-describe transports, bundles, agents,
binary_protocol, and browser_bus — enough that one reflect round-trip
bootstraps a remote caller.
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
    assert "kernel.send" in ip["shape"]
    assert "use_when" in ip


async def test_reflect_transports_in_prompt(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    inp = r["transports"]["in_prompt"]
    assert "<send" in inp["shape"]
    assert "NOT a wire format" in inp["use_when"]
    assert inp.get("example", "").startswith("<send")


async def test_reflect_transports_cli(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    cli = r["transports"]["cli"]
    assert cli["shape"].startswith("python kernel.py call")
    assert cli["shorthand"].startswith("python kernel.py reflect")


async def test_reflect_drops_top_level_send_syntax(seeded_kernel):
    """Old misleading top-level `send_syntax` / `example` MUST be gone.

    Keeping them at top-level led readers to POST the XML form to the
    server (which 404s). They now live under transports.in_prompt with
    a clear 'NOT a wire format' tag.
    """
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "send_syntax" not in r
    assert "example" not in r


async def test_reflect_available_bundles_lists_workspace(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    bundles = r["available_bundles"]
    assert isinstance(bundles, list)
    assert len(bundles) >= 2
    names = {b["name"] for b in bundles}
    assert "core" in names
    assert "cli" in names
    # sorted
    sorted_names = [b["name"] for b in bundles]
    assert sorted_names == sorted(sorted_names)
    # each entry has id + handler_module shape
    for b in bundles:
        assert set(b.keys()) == {"name", "handler_module"}
        assert b["handler_module"].endswith(".tools")


async def test_reflect_agents_lists_running(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    agents = r["agents"]
    assert isinstance(agents, list)
    ids = {a["id"] for a in agents}
    # seeded fixture brings up at least core + cli singletons
    assert "core" in ids
    assert "cli" in ids
    for a in agents:
        assert "id" in a and "handler_module" in a


async def test_reflect_well_known_still_singletons(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    wk = r["well_known"]
    assert "core" in wk and "cli" in wk
