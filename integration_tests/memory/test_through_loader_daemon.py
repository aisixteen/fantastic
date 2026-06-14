"""Non-LLM daemon smoke for the through-loader memory path — proves a yaml_state with
NO file_bridge_id persists THROUGH the loader over the LIVE WS daemon (not just
in-process), isolating "does the harness/daemon work" from "is the model slow". Free."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_INTEG = _HERE.parent
if str(_INTEG) not in sys.path:
    sys.path.insert(0, str(_INTEG))

from helpers.seeding import seed_create, seed_web, seed_web_ws  # noqa: E402
from helpers.ws import ws_call  # noqa: E402


async def test_yaml_state_persists_through_loader_over_ws(
    python_binary, python_kernel, parity_tmp, free_port
):
    wd = parity_tmp("through_loader_daemon") / "host"
    wd.mkdir(parents=True, exist_ok=True)
    port = free_port()

    seed_web(python_binary, wd, port)
    seed_web_ws(python_binary, wd)
    # The loader's discoverable store.
    seed_create(
        python_binary, wd,
        handler_module="file_bridge.tools", agent_id="store",
        root=".fantastic", ingress_rule="allow_all",
    )
    # Memory agent — NO file_bridge_id. Persists through the loader.
    seed_create(
        python_binary, wd,
        handler_module="yaml_state.tools", agent_id="mem", mode="mem",
    )

    await python_kernel(wd, port)

    s = await asyncio.wait_for(
        ws_call(port, "mem", "set", key="user.name", value="Ada"), timeout=30.0
    )
    assert s.get("set") is True, s
    r = await asyncio.wait_for(ws_call(port, "mem", "read", key="user.name"), timeout=30.0)
    assert r.get("value") == "Ada", r
    # Reflect no longer carries a per-agent persistence wiring.
    rf = await asyncio.wait_for(ws_call(port, "mem", "reflect"), timeout=30.0)
    assert "file_bridge_id" not in rf, rf
    # On disk, through the loader, next to the record.
    assert (wd / ".fantastic" / "agents" / "mem" / "state.yaml").read_text().count("Ada")
