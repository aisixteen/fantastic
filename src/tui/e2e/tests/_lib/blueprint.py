"""THE BLUEPRINT FIXTURE for the import tests (#269): E130, E131, E132 and E133 share it.

A running `fantastic canvas` in `rig.proj` is the kernel; a blueprint is a folder
under `rig.proj` (the verb takes paths relative to the running dir, and refuses `..`).
The blueprints are written by hand, the way a person edits an exported one: nesting
is the parent, `@memory` is a port (an outside reference the importer binds).

    rig = Rig(__file__, live=False); rig.build(); rig.make_home(None); rig.start()
    world(rig)                      # `team_home` (the parent) and `memory2` (a store to bind)
    write_team(rig, "bp")           # squad > lead, scout  (lead reaches `@memory`)
    reply = importing(rig, "bp", "team_home", {"@memory": "memory2"}, approve=True)
"""

from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path
from typing import Any

PARENT = "team_home"
STORE = "memory2"

TEAM = """\
agents:
  - id: squad
    handler_module: ram_store.tools
    agents:
      - id: lead
        handler_module: ram_store.tools
        note: first
        can_call: {scout: '*', '@memory': '*'}
        memory_store_id: '@memory'
      - id: scout
        handler_module: ram_store.tools
        callable_by: {lead: '*'}
ports:
  '@memory': {kind: memory, why: the notes it keeps, used_by: [lead]}
"""

#: the same team with no port: nothing to bind
PLAIN = """\
agents:
  - id: crew
    handler_module: ram_store.tools
    agents:
      - id: mate
        handler_module: ram_store.tools
        note: first
      - id: deckhand
        handler_module: ram_store.tools
        callable_by: {mate: '*'}
"""


def send(rig, target: str, payload: dict, timeout: float = 60) -> dict:
    """One call into the running kernel from the shell (`fantastic send`): the reply's `result`."""
    r = subprocess.run([str(rig.binary), "send", json.dumps({"target": target, "payload": payload})],
                       cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=timeout)
    try:
        return json.loads(r.stdout).get("result") or {}
    except ValueError:
        return {"error": (r.stdout + r.stderr)[-400:]}


def agent_ids(rig) -> list[str]:
    """Every agent the kernel holds."""
    return sorted(a["id"] for a in send(rig, "kernel_state", {"type": "list_agents"}).get("agents", []))


def world(rig) -> None:
    """The parent every import goes under, and a second memory store to bind."""
    send(rig, "kernel_state", {"type": "create_agent", "handler_module": "ram_store.tools", "id": PARENT, "parent_id": "kernel_state"})
    send(rig, "kernel_state", {"type": "create_agent", "handler_module": "sqlite_store.tools", "id": STORE,
                               "parent_id": "kernel_state", "path": f"{STORE}.db"})


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
