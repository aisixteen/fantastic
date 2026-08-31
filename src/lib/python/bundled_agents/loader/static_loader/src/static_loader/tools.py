"""static_loader — a DECLARATIVE YAML roster loader.

The industry locks agents to a configurable roster. This closes that: a
`fantastic.yaml` manifest declares the agents, and this loader hydrates them.
The kernel knows NOTHING about YAML — this bundle owns the format (mirror of how
`kernel_state` owns the on-disk `.fantastic` format).

How it hydrates, and why it needs no god-mode. At `boot` the loader reads its
manifest and creates each declared agent through the NORMAL `create_agent` path,
so every one carries the Unix baseline grant plus any explicit `can_call` the
manifest names. That whole pass runs under `operator_context()` — the loader
hydrating the roster it was told to build is the operator's authority at boot,
exactly as `create_agent` boots the child it just made. It is NOT standing power:
the loader's OWN record carries no special grant. After boot the roster is
deny-all like everything else — an agent cannot `create_agent` — so the set of
agents is FIXED by the manifest. That is the whole point: declare the roster,
and mutation is closed by the transport's default, not by a mode flag.

Manifest shape (`fantastic.yaml`):

    agents:
      - id: memory                     # explicit ids (idempotent across reboots)
        handler_module: text_file.tools
      - id: worker
        handler_module: exec_bridge.tools
        allowed: ['python3 .*']
        can_call:                      # explicit lateral grants (deny-all)
          memory: [set, read]
"""

from __future__ import annotations

from pathlib import Path

import yaml
from file_bridge import fs

from kernel import operator_context
from kernel._refusal import UNHANDLED, refusal

MANIFEST_FIELD = "manifest"


def _load_manifest(agent) -> tuple[list[dict], str | None]:
    """Read + parse the manifest. Returns (agents, error). Cold read, clamped to
    cwd by the running-dir law — the same one-time bring-up path as
    `kernel_state.read_tree` (before any file_bridge agent can exist)."""
    path = agent.record.get(MANIFEST_FIELD)
    if not isinstance(path, str) or not path:
        return [], f"static_loader: no {MANIFEST_FIELD!r} path on the record"
    try:
        text = fs.read_text(Path.cwd(), path)
    except (OSError, ValueError) as e:
        return [], f"static_loader: cannot read manifest {path!r}: {e}"
    try:
        data = yaml.safe_load(text) or {}
    except yaml.YAMLError as e:
        return [], f"static_loader: manifest {path!r} is not valid YAML: {e}"
    if not isinstance(data, dict):
        return [], f"static_loader: manifest {path!r} must be a mapping with `agents:`"
    agents = data.get("agents", [])
    if not isinstance(agents, list):
        return [], f"static_loader: `agents:` in {path!r} must be a list"
    return agents, None


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a manifest that is absent or unreadable at WRITE time, so a broken
    roster never becomes a booting record. Merged view (record + patch)."""
    merged = {**record, **meta}
    if MANIFEST_FIELD not in merged:
        return None  # allowed to exist without one yet; boot will report it
    path = merged.get(MANIFEST_FIELD)
    if not isinstance(path, str) or not path:
        return f"static_loader: {MANIFEST_FIELD} must be a non-empty path string"
    try:
        fs.read_text(Path.cwd(), path)
    except (OSError, ValueError) as e:
        return f"static_loader: manifest {path!r} is not readable: {e}"
    return None


async def _boot(id, payload, agent):
    """Hydrate the declared roster as OPERATOR. Idempotent: a spec whose id
    already exists (hydrated from disk on a reboot) is skipped, so the manifest
    and the persisted tree never duplicate. Specs SHOULD carry explicit ids."""
    agents, err = _load_manifest(agent)
    if err:
        return {"error": err, "booted": False}
    created: list[str] = []
    skipped: list[str] = []
    with operator_context():
        for spec in agents:
            if not isinstance(spec, dict) or not spec.get("handler_module"):
                return {
                    "error": f"static_loader: each roster entry needs a handler_module: {spec!r}",
                    "booted": False,
                }
            aid = spec.get("id")
            if aid and agent.ctx.agents.get(aid) is not None:
                skipped.append(aid)
                continue
            # `parent_id` = THIS loader → the declared agent becomes the loader's
            # child ($parent = the loader), exactly as addressing the loader used to
            # do. Baseline grant is stamped + the manifest's explicit `can_call`
            # merges on top; the child boots inside create_agent. The verb itself is
            # kernel-owned, so it goes to `kernel` and names its operand.
            rec = (
                await agent.send(
                    [
                        {
                            "target": "kernel",
                            "payload": {
                                "type": "create_agent",
                                **spec,
                                # AFTER the spread, not before. A manifest entry
                                # carrying its own `parent_id` used to win and
                                # place the agent somewhere else entirely —
                                # measured 2026-08-28: an entry naming the root
                                # landed as the ROOT's child, not the loader's,
                                # silently breaking the "roster is the loader's
                                # subtree" guarantee this docstring states and
                                # that owned-subtree delete depends on.
                                "parent_id": agent.id,
                            },
                        }
                    ]
                )
            )[0]["result"]
            if isinstance(rec, dict) and rec.get("id"):
                created.append(rec["id"])
            else:
                return {
                    "error": f"static_loader: create failed for {spec!r}: {rec}",
                    "booted": False,
                }
    return {"booted": True, "created": created, "skipped": skipped}


async def _reflect(id, payload, agent):
    """The loader's posture: what manifest it hydrates, and that it IS a loader
    (so `$loader`-style discovery + a client both see the declarative root)."""
    return {
        "id": id,
        "sentence": "declarative YAML roster loader",
        "display_name": agent.display_name or id,
        "loader": True,
        "manifest": agent.record.get(MANIFEST_FIELD),
        "verbs": {
            name: (fn.__doc__ or "").strip().splitlines()[0] if fn.__doc__ else ""
            for name, fn in VERBS.items()
        },
    }


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return refusal(
            UNHANDLED,
            f"static_loader: unknown type {payload.get('type')!r}",
            verb=payload.get("type"),
        )
    return await fn(id, payload, agent)
