"""INHERITANCE: the one place a bundle's `record` reaches its instances. Every source
is the same here — a compiled bundle gives no fields, a folder bundle its `record`, a
meta bundle its own and each one's on the way to its base (`_bundles.defaults`). The
fields go UNDER an instance's own: at `create_agent`, when a record is loaded, and on
`reload_bundle`. The loader only reads files; it knows nothing of this. The twin of
rust `fantastic-kernel/src/inherit.rs`.

A record's KIND is its `handler_module`, or its `bundle` when it runs a meta bundle on
another base than the bundle's own (an AI role on whichever backend its spawner uses):
`handler_module: vllm_backend.tools, bundle: worker`."""

from __future__ import annotations

from typing import Any

from kernel._bundles import base_of, defaults, meta_chain


def kind_of(handler_module: str, meta: dict) -> str:
    """The bundle a record inherits from: its `bundle`, else its `handler_module`."""
    b = meta.get("bundle")
    return b if isinstance(b, str) and b else handler_module


def apply(handler_module: str, meta: dict) -> dict:
    """`meta` with the fields of the record's bundle (`kind_of`) under its own."""
    return {**defaults(kind_of(handler_module, meta)), **meta}


def create_payload(payload: dict) -> dict:
    """A `create_agent` payload with its bundle's fields under it. `bundle: X` (beside
    the base's handler_module, or alone) is the same create as `handler_module: X`."""
    out = dict(payload)
    b = out.get("bundle")
    if isinstance(b, str) and b:
        # the bundle's own base (or none named): the kind IS the bundle. Another base:
        # it keeps its handler and names the bundle it inherits from.
        hm = out.get("handler_module")
        if not hm or hm == b or hm == base_of(b):
            out.pop("bundle")
            out["handler_module"] = b
    hm = out.get("handler_module")
    return apply(hm, out) if isinstance(hm, str) else out


def inheriting(agents: dict, name: str) -> list[tuple[Any, dict]]:
    """The instances that inherit from bundle `name` (its own and those of every bundle
    that extends it), each with the fields it inherits NOW: taken before a reload, so
    `reapply` can tell an inherited field from one the instance set itself."""
    out = []
    for a in agents.values():
        if not isinstance(a.handler_module, str):
            continue
        kind = kind_of(a.handler_module, a._meta)
        if kind == name or name in meta_chain(kind):
            out.append((a, defaults(kind)))
    return out


def reapply(ctx, before: list[tuple[Any, dict]]) -> list[str]:
    """After a reload: each instance's inherited fields take the new values (a field the
    bundle dropped goes); a field the instance changed is its own and stays. Returns the
    ids it changed, sorted."""
    ids = []
    for a, old in before:
        mine = dict(a._meta)
        new = defaults(kind_of(a.handler_module, mine))
        patch, gone = {}, []
        for k, v in old.items():
            if k in mine and mine[k] == v:
                if k not in new:
                    gone.append(k)
                elif new[k] != v:
                    patch[k] = new[k]
        for k, v in new.items():
            if k not in old and k not in mine:
                patch[k] = v
        if not patch and not gone:
            continue
        for k in gone:
            a._meta.pop(k, None)
        ctx.update(a.id, **patch)  # the `updated` event re-persists the record
        ids.append(a.id)
    return sorted(ids)
