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
        callable_by:                   # the INGRESS half — deny-all is TWO-SIDED,
          worker: [write, read]        # so an edge needs both ends declared here
      - id: worker
        handler_module: exec_bridge.tools
        allowed: ['python3 .*']
        can_call:                      # the EGRESS half
          memory: [write, read]

    Nothing is auto-granted: a manifest entry gets the structural baseline
    ({$self,$parent,$children}) and exactly what it declares. An agent that keeps
    its own sidecar (an AI's chat, the scheduler's schedules) declares
    `can_call: {$loader: [persist_blob, load_blob]}` itself.
"""

from __future__ import annotations

from pathlib import Path

import yaml
from file_bridge import fs

from kernel import operator_context
from kernel._authz import DEFAULT_CALLABLE_BY, DEFAULT_CAN_CALL
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
    """Hydrate the declared roster as OPERATOR.

    THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM, cloud, an API).
    Idempotent by id — an entry whose agent already exists is never duplicated —
    but it is RE-APPLIED, not skipped: every declared field is written back over
    the hydrated record, so editing the yaml is how you change the roster. Only
    `handler_module` cannot change under a live agent (its in-process state
    belongs to the old bundle); that entry fails loud. Every spec MUST carry an
    explicit id — an id-less entry is refused, because it can only ever be
    created anew. Returns `created` (new this boot) + `reapplied`."""
    agents, err = _load_manifest(agent)
    if err:
        return {"error": err, "booted": False}
    created: list[str] = []
    reapplied: list[str] = []
    with operator_context():
        for spec in agents:
            if not isinstance(spec, dict) or not spec.get("handler_module"):
                return {
                    "error": f"static_loader: each roster entry needs a handler_module: {spec!r}",
                    "booted": False,
                }
            aid = spec.get("id")
            # An id-less entry cannot be idempotent: the roster is keyed by id,
            # so nothing matches the agent the LAST boot minted and this boot
            # mints a SECOND one (`text_file_ab12cd`, then `text_file_ef34ab`),
            # which the flush persists — the roster grows by one agent per
            # restart and the operator is never told. The manifest is truth, so
            # it must name its agents.
            if not isinstance(aid, str) or not aid:
                return {
                    "error": f"static_loader: roster entry needs an explicit id: {spec!r}",
                    "booted": False,
                }
            live = agent.ctx.agents.get(aid)
            if live is not None:
                # THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM,
                # cloud, an API). So an id that already exists is RE-APPLIED from
                # the manifest, never skipped: editing a grant in the yaml used
                # to change nothing at all, because the hydrated record silently
                # won and the operator got no word about it.
                declared = {
                    k: v
                    for k, v in spec.items()
                    if k not in ("id", "handler_module", "parent_id")
                }
                # `update_agent` REPLACES a named field, so a bare re-apply of
                # `can_call` would delete the structural baseline the kernel
                # stamps at create ($self/$parent/$children) and sever the agent
                # from its own parent and children. Merge over the baseline,
                # exactly as `create_agent` does.
                for field, base in (
                    ("can_call", DEFAULT_CAN_CALL),
                    ("callable_by", DEFAULT_CALLABLE_BY),
                ):
                    if isinstance(declared.get(field), dict):
                        declared[field] = {**base, **declared[field]}
                if live.handler_module != spec["handler_module"]:
                    # A bundle cannot be swapped under a live agent (its
                    # in-process state belongs to the old one); the manifest is
                    # asking for a different agent under a used name.
                    return {
                        "error": (
                            f"static_loader: {aid!r} exists as "
                            f"{live.handler_module!r} but the manifest declares "
                            f"{spec['handler_module']!r} — a handler_module is "
                            "fixed at create; delete the agent to re-declare it"
                        ),
                        "booted": False,
                    }
                if declared:
                    r = (
                        await agent.send(
                            [
                                {
                                    "target": "kernel",
                                    "payload": {
                                        "type": "update_agent",
                                        "id": aid,
                                        **declared,
                                    },
                                }
                            ]
                        )
                    )[0]["result"]
                    if isinstance(r, dict) and r.get("error"):
                        return {
                            "error": f"static_loader: re-applying {aid!r}: {r['error']}",
                            "booted": False,
                        }
                reapplied.append(aid)
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
    return {"booted": True, "created": created, "reapplied": reapplied}


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
