"""The through-loader memory path, end-to-end, with NO model — the deterministic twin
of the LLM-discovery test (integration_tests/memory/test_ai_memory_discovery.py).

An operator/LLM does exactly this: wire the `.fantastic` store ONCE (the loader finds
it), create a yaml_state memory agent (NOTHING else — no per-agent wiring), set a fact,
read it back. It lands on disk THROUGH the loader, next to the agent's own record.
No `file_bridge_id` anywhere on the memory agent."""

from pathlib import Path


async def test_memory_works_through_loader_end_to_end(seeded_kernel, store_agent):
    # 1. a memory agent — created with NO persistence wiring at all.
    mem = (
        await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "yaml_state.tools",
                "mode": "mem",
            },
        )
    )["id"]
    # 2. remember a fact.
    s = await seeded_kernel.send(
        mem, {"type": "set", "key": "user.name", "value": "Ada"}
    )
    assert s.get("set") is True, s
    # 3. it landed on disk, THROUGH the loader, next to the agent's record.
    on_disk = Path(".fantastic") / "agents" / mem / "state.yaml"
    assert on_disk.exists() and "Ada" in on_disk.read_text()
    # 4. read it back.
    r = await seeded_kernel.send(mem, {"type": "read", "key": "user.name"})
    assert r["value"] == "Ada"


async def test_no_store_is_a_loud_failure_not_silent_ram(seeded_kernel):
    # The loader has no `.fantastic` store wired ⇒ the write failfasts (no RAM).
    mem = (
        await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "yaml_state.tools",
                "mode": "mem",
            },
        )
    )["id"]
    r = await seeded_kernel.send(mem, {"type": "set", "key": "k", "value": "v"})
    assert "error" in r and "no store" in r["error"]
