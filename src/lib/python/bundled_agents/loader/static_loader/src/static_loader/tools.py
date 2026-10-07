"""static_loader — a DECLARATIVE YAML roster loader.

The industry locks agents to a configurable roster. This closes that: a
`fantastic.yaml` manifest declares the agents, and this loader hydrates them.
The kernel knows NOTHING about YAML — this bundle owns the format (mirror of how
`kernel_state` owns the on-disk `.fantastic` format).

How it hydrates, and why it needs no standing power. At `boot` the loader reads
its manifest and creates each declared agent through the kernel's own
`create_agent` implementation, so every one carries the baseline grant plus any
explicit grant the manifest names. Hydrating the roster at boot is KERNEL
BOOKKEEPING, like `create_agent` booting the child it just made: each create /
re-apply is a direct call of the kernel verb (`Kernel.bookkeeping` on the root),
never a gated send. The loader's OWN record carries no special grant. And the
pass runs ONLY on the kernel's own boot — origin none: the cold boot, or
`create_agent` booting the loader it just made. An agent-sent `boot` is refused:
any agent granted a reach to the loader could otherwise re-run the manifest — restore a grant the
operator revoked, re-create an agent the operator deleted, or apply whatever the
manifest file says at that moment. After boot the roster is
deny-all like everything else — an agent creates only when granted `create_agent`
on the root — so the set of
agents is FIXED by the manifest. That is the whole point: declare the roster,
and mutation is closed by the transport's default, not by a mode flag.

Manifest shape (`fantastic.yaml`):

    agents:
      - id: memory                     # explicit ids (idempotent across reboots)
        handler_module: file_bridge.tools
        callable_by:                   # the INGRESS half — deny-all is TWO-SIDED,
          worker: [write, read]        # so an edge needs both ends declared here
      - id: worker
        handler_module: exec_bridge.tools
        allowed: ['python3 .*']
        can_call:                      # the EGRESS half
          memory: [write, read]

    Nothing is auto-granted: a manifest entry gets the baseline ({$self}) and
    exactly what it declares.
"""

from __future__ import annotations

import yaml
from file_bridge import fs

from kernel import current_origin
from kernel._authz import DEFAULT_CALLABLE_BY, DEFAULT_CAN_CALL
from kernel._refusal import UNAUTHORIZED, UNHANDLED, refusal

MANIFEST_FIELD = "manifest"

# The one text an agent-sent `boot` is refused with — rust/swift byte for byte.
BOOT_REFUSED = (
    "static_loader: an agent-sent boot is refused — hydrating the roster is the "
    "kernel's own bookkeeping, so only the kernel's own boot (cold boot, "
    "create_agent) runs it"
)
BOOT_REFUSED_HINT = (
    "change an agent's record with update_agent (its parent writes it); the "
    "operator re-boots the loader"
)


def _load_manifest(agent) -> tuple[list[dict], str | None]:
    """Read + parse the manifest. Returns (agents, error). Cold read, clamped to
    the kernel's running dir (`Kernel.workdir`) — the same one-time bring-up path
    as `kernel_state.read_tree` (before any file_bridge agent can exist)."""
    path = agent.record.get(MANIFEST_FIELD)
    if not isinstance(path, str) or not path:
        return [], f"static_loader: no {MANIFEST_FIELD!r} path on the record"
    try:
        text = fs.read_text(fs.Root(agent.ctx.workdir), path)
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
    """Refuse a malformed `manifest` at WRITE time — SHAPE only (a non-empty path
    string). Whether the file is there and parses is the BOOT read's to report,
    against `Kernel.workdir`: this hook is handed no kernel, so a write-time read
    could only clamp to some other base. Merged view (record + patch)."""
    merged = {**record, **meta}
    if MANIFEST_FIELD not in merged:
        return None  # allowed to exist without one yet; boot will report it
    path = merged.get(MANIFEST_FIELD)
    if not isinstance(path, str) or not path:
        return f"static_loader: {MANIFEST_FIELD} must be a non-empty path string"
    return None


async def _boot(id, payload, agent):
    """Hydrate the declared roster — kernel bookkeeping at boot.

    THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM, cloud, an API).
    Idempotent by id — an entry whose agent already exists is never duplicated —
    but it is RE-APPLIED, not skipped: every declared field is written back over
    the hydrated record, so editing the yaml is how you change the roster. Only
    `handler_module` cannot change under a live agent (its in-process state
    belongs to the old bundle); that entry fails loud. Every spec MUST carry an
    explicit id — an id-less entry is refused, because it can only ever be
    created anew. Returns `created` (new this boot) + `reapplied`.

    The kernel's own boot only (origin none); an agent-sent boot is refused."""
    if current_origin() is not None:
        # The pass below calls the kernel's verbs directly, so the one who asks
        # for it must be the kernel or the operator: the cold boot, `create_agent`
        # booting this loader, the CLI. A caller's boot is not that — refused
        # before the manifest is read.
        return refusal(UNAUTHORIZED, BOOT_REFUSED, hint=BOOT_REFUSED_HINT)
    agents, err = _load_manifest(agent)
    if err:
        return {"error": err, "booted": False}
    created: list[str] = []
    reapplied: list[str] = []
    root = agent.ctx.root

    async def kernel_verb(p: dict):
        # A DIRECT call of the kernel verb (its root implementation) — roster
        # hydration is kernel bookkeeping, never a gated send.
        return (await agent.ctx.bookkeeping(root, p))["result"]

    for spec in agents:
        if not isinstance(spec, dict) or not spec.get("handler_module"):
            return {
                "error": f"static_loader: each roster entry needs a handler_module: {spec!r}",
                "booted": False,
            }
        aid = spec.get("id")
        # An id-less entry cannot be idempotent: the roster is keyed by id,
        # so nothing matches the agent the LAST boot minted and this boot
        # mints a SECOND one (`file_bridge_ab12cd`, then `file_bridge_ef34ab`),
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
            # `can_call` would delete the baseline the kernel stamps at
            # create ($self). Merge over the baseline, exactly as
            # `create_agent` does.
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
                r = await kernel_verb({"type": "update_agent", "id": aid, **declared})
                if isinstance(r, dict) and r.get("error"):
                    return {
                        "error": f"static_loader: re-applying {aid!r}: {r['error']}",
                        "booted": False,
                    }
            reapplied.append(aid)
            continue
        # `parent_id` = THIS loader → the declared agent becomes the loader's
        # child ($parent = the loader). Baseline grant is stamped + the
        # manifest's explicit grants merge on top; the child boots inside
        # create_agent.
        rec = await kernel_verb(
            {
                "type": "create_agent",
                **spec,
                # AFTER the spread, not before. A manifest entry carrying its
                # own `parent_id` used to win and place the agent somewhere
                # else entirely — measured 2026-08-28: an entry naming the
                # root landed as the ROOT's child, not the loader's, silently
                # breaking the "roster is the loader's subtree" guarantee this
                # docstring states and that owned-subtree delete depends on.
                "parent_id": agent.id,
            }
        )
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
    (so a client sees the declarative root)."""
    return {
        "id": id,
        "sentence": "declarative YAML roster loader",
        "display_name": agent.display_name or id,
        "loader": True,
        "manifest": agent.record.get(MANIFEST_FIELD),
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
