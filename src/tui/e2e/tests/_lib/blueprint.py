"""THE BLUEPRINT FIXTURE for the import tests (#269): E130, E131, E132 and E133 share it.

A running `fantastic canvas` in `rig.proj` is the kernel; a blueprint is a folder
under `rig.proj` (the verb takes paths relative to the running dir, and refuses `..`).
The blueprints are written by hand, the way a person edits an exported one: nesting
is the parent, `@memory` is a port (an outside reference the importer binds).

    rig = Rig(__file__, live=False); rig.build(); rig.make_home(None); rig.start()
    world(rig)                      # `team_home` (the parent) and `memory2` (a store to bind), under `canvas_root`
    write_team(rig, "bp")           # squad > lead, scout  (lead holds lines on scout and `@memory`)
    reply = importing(rig, "bp", "team_home", {"@memory": "memory2"}, approve=True)
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

from .mcp_session import result_of

PARENT = "team_home"
STORE = "memory2"

TEAM = """\
agents:
  - id: squad
    handler_module: ram_store.tools
    agents:
      # scout before lead: a line names an id that already exists (an agent's
      # write of a line on a missing id is the unknown answer)
      - id: scout
        handler_module: ram_store.tools
      - id: lead
        handler_module: ram_store.tools
        description: first
        can: {scout: {call: ['*']}, '@memory': {call: [append, read]}}
        memory_store_id: '@memory'
ports:
  '@memory': {kind: memory, why: the notes it keeps, used_by: [lead]}
"""

#: the same team with no port: nothing to bind
PLAIN = """\
agents:
  - id: crew
    handler_module: ram_store.tools
    agents:
      - id: deckhand
        handler_module: ram_store.tools
      - id: mate
        handler_module: ram_store.tools
        description: first
        can: {deckhand: {call: ['*']}}
"""


def send(rig, target: str, payload: dict, timeout: float = 60) -> dict:
    """One call into the running kernel through its MCP channel, as `fantastic send`
    makes it, the leg's call: the ack, then the item's `result` from the session's
    settle notification (#795)."""
    return result_of(rig.proj / ".fantastic", target, payload, timeout)


#: the main canvas: the `mcp` leg (`fantastic send`) holds `create` on its tree
WHERE = "canvas_root"


def agent_ids(rig) -> list[str]:
    """Every agent of the import's landing subtree (`PARENT` and all under it), as
    the leg sees it (`reflect tree=ids`, following `next`)."""
    ids: list[str] = []
    after = None
    while True:
        page = send(rig, PARENT, {"type": "reflect", "tree": "ids", **({"after": after} if after else {})})
        ids += [x if isinstance(x, str) else x.get("id") for x in page.get("tree") or []
                if isinstance(x, str) or (isinstance(x, dict) and x.get("id"))]
        after = page.get("next")
        if not after:
            return sorted(ids)


def record_ids(rig) -> list[str]:
    """Every agent record the kernel has saved, read from disk (the project's
    `.fantastic` and the home), not through the leg: the whole tree, also where
    the leg's `reflect` gets the unknown answer."""
    return sorted(f.parent.name for root in (rig.proj / ".fantastic", rig.home) for f in root.rglob("agent.yaml"))


def exists(rig, agent_id: str) -> bool:
    """Whether the leg reaches `agent_id`: its `reflect` is not the unknown answer."""
    r = send(rig, agent_id, {"type": "reflect"})
    return bool(r) and not (r.get("reason") == "unknown_agent" and r.get("error") == f"no agent {agent_id!r}")


def world(rig) -> None:
    """The parent every import goes under, and a second memory store to bind; both
    under the main canvas, where the leg's `create` line reaches."""
    for made in (send(rig, "kernel_state", {"type": "create_agent", "handler_module": "ram_store.tools", "id": PARENT, "parent_id": WHERE}),
                 send(rig, "kernel_state", {"type": "create_agent", "handler_module": "sqlite_store.tools", "id": STORE,
                                            "parent_id": WHERE, "path": f"{STORE}.db"})):
        assert "error" not in made, f"world: create refused: {made}"


def write_blueprint(rig, folder: str, yaml_text: str, files: dict[str, str] | None = None) -> Path:
    """A blueprint folder under the project: `blueprint.yaml` and the extra files."""
    d = rig.proj / folder
    d.mkdir(parents=True, exist_ok=True)
    (d / "blueprint.yaml").write_text(yaml_text)
    for name, text in (files or {}).items():
        (d / name).parent.mkdir(parents=True, exist_ok=True)
        (d / name).write_text(text)
    return d


def write_team(rig, folder: str = "bp") -> Path:
    return write_blueprint(rig, folder, TEAM)


def importing(rig, folder: str, parent: str = PARENT, bind: dict | None = None, **flags: Any) -> dict:
    """`user.import_subtree`; `flags` are `approve` / `allow_code` as given (a string is sent as a string)."""
    return send(rig, "user", {"type": "import_subtree", "path": folder, "parent_id": parent, "bind": bind or {}, **flags})


def record(rig, agent_id: str, secs: float = 10) -> dict:
    """An agent's record on disk (the kernel writes it a moment after creating it)."""
    end = time.time() + secs
    while time.time() < end:
        rec = rig.host_record(agent_id)
        if rec is not None:
            return rec
        time.sleep(0.2)
    return {}
