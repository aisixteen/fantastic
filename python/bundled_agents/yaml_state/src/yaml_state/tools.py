"""yaml_state — a durable YAML key-value memory agent.

One bundle, instantiated as N agents. The `mode` meta ("mem" | "data")
sets the *discipline* (and the reflect sentence) — the verbs are
identical:

  - data → current scratch-state, overwrite-in-place.
  - mem  → durable keyed facts, accrete + prune at LLM discretion.

This bundle owns NO disk surface of its own and never imports `fs`. It persists
THROUGH the loader (`kernel_state`) — there is NOTHING to wire: `set` / `read` just
work. The loader writes `state.yaml` to the one `.fantastic` store it owns, landing the
sidecar NEXT TO this agent's `agent.json` at `agents/<id>/state.yaml`. If no store is
wired at the root, writes **failfast** (`set` / `delete` / `replace` surface the error;
NO silent RAM) and reads return empty. Human-editable, git-diffable, disk-is-truth (read
fresh each call, no cache — an external edit is seen on the next read).

Keys are flat namespaced strings (dotted convention:
`domain.subject.attribute`, e.g. `user.name`, `decision.db`). Values are
arbitrary JSON.
"""

from __future__ import annotations

from typing import Any

import yaml

_MEM_SENTENCE = (
    "Your durable memory. Facts to remember across sessions live here. `set` a "
    "descriptive key the moment the user tells you something worth keeping (a name, "
    "a preference, a decision); `read` / `keys` them back when relevant."
)
_DATA_SENTENCE = (
    "Your durable scratch-state (component state, config, run params, current "
    "selection). One value per key, overwrite-in-place; `read` it when you need it."
)


def _mode(agent) -> str:
    rec = agent.get(agent.id) or {}
    m = rec.get("mode")
    return m if m in ("mem", "data") else "data"


def _emit_yaml(doc: dict[str, Any]) -> str:
    if not doc:
        return ""
    return yaml.safe_dump(doc, sort_keys=True, allow_unicode=True)


async def _load(agent) -> dict[str, Any]:
    """Read the store THROUGH the loader (`kernel_state.load_blob`). Missing / denied /
    no-store ⇒ {} (reads are lenient; writes failfast)."""
    r = await agent.send(
        "kernel_state",
        {"type": "load_blob", "agent_id": agent.id, "name": "state.yaml"},
    )
    content = r.get("content") if isinstance(r, dict) else None
    if not content:
        return {}
    try:
        doc = yaml.safe_load(content)
    except yaml.YAMLError:
        return {}
    return doc if isinstance(doc, dict) else {}


async def _persist(agent, doc: dict[str, Any], verb: str) -> dict | None:
    """Write the store THROUGH the loader (`kernel_state.persist_blob`); surface a
    denied/failed/no-store write as an error (no silent loss). Returns an error dict,
    or None on success."""
    w = await agent.send(
        "kernel_state",
        {
            "type": "persist_blob",
            "agent_id": agent.id,
            "name": "state.yaml",
            "content": _emit_yaml(doc),
        },
    )
    if not isinstance(w, dict):
        return {"error": f"yaml_state.{verb}: loader gave no reply"}
    if w.get("error"):
        out = {"error": f"yaml_state.{verb}: {w['error']}"}
        if w.get("reason"):
            out["reason"] = w["reason"]
        if w.get("hint"):
            out["hint"] = w["hint"]
        return out
    return None


async def _reflect(id, payload, agent):
    """Identity + mode + current key count. No args."""
    doc = await _load(agent)
    mode = _mode(agent)
    return {
        "id": id,
        "sentence": _MEM_SENTENCE if mode == "mem" else _DATA_SENTENCE,
        "mode": mode,
        "key_count": len(doc),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, agent):
    """No-op. The agent holds no process state."""
    return None


async def _read(id, payload, agent):
    """args: key?:str. Value at `key` (null if absent); whole doc if `key` omitted."""
    doc = await _load(agent)
    key = payload.get("key")
    if key is None:
        return {"doc": doc}
    return {"key": key, "value": doc.get(key)}


async def _keys(id, payload, agent):
    """args: none. List keys + value sizes — the table-of-contents."""
    doc = await _load(agent)
    return {"keys": [{"key": k, "size": len(str(v))} for k, v in sorted(doc.items())]}


async def _set(id, payload, agent):
    """args: key:str, value:any. Upsert one key (data: overwrite; mem: accrete a fact). Persisted through the loader; failfast if no store wired."""
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "yaml_state.set: key (non-empty str) required"}
    if "value" not in payload:
        return {"error": "yaml_state.set: value required"}
    doc = await _load(agent)
    doc[key] = payload["value"]
    if err := await _persist(agent, doc, "set"):
        return err
    return {"key": key, "set": True}


async def _delete(id, payload, agent):
    """args: key:str. Remove a key (prune / clear state). Persisted through the loader; failfast if no store wired."""
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "yaml_state.delete: key (non-empty str) required"}
    doc = await _load(agent)
    existed = key in doc
    doc.pop(key, None)
    if err := await _persist(agent, doc, "delete"):
        return err
    return {"key": key, "deleted": existed}


async def _replace(id, payload, agent):
    """args: doc:object. Overwrite the whole store (distill/prune-rewrite; {} clears). Persisted through the loader; failfast if no store wired."""
    if "doc" not in payload:
        return {"error": "yaml_state.replace: doc (object) required"}
    doc = payload["doc"]
    if not isinstance(doc, dict):
        return {"error": "yaml_state.replace: doc must be an object"}
    if err := await _persist(agent, doc, "replace"):
        return err
    return {"replaced": True, "keys": len(doc)}


async def _state_yaml(id, payload, agent):
    """args: none. The entire store as YAML text — the exact block injected on boot."""
    return {"yaml": _emit_yaml(await _load(agent))}


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "read": _read,
    "keys": _keys,
    "set": _set,
    "delete": _delete,
    "replace": _replace,
    "state_yaml": _state_yaml,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return {"error": f"yaml_state: unknown type {payload.get('type')!r}"}
    return await fn(id, payload, agent)
