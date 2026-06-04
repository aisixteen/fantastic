"""yaml_state — a durable YAML key-value memory agent.

One bundle, instantiated as N agents. The `mode` meta ("mem" | "data")
sets the *discipline* (and the reflect sentence) — the verbs are
identical:

  - data → current scratch-state, overwrite-in-place.
  - mem  → durable keyed facts, accrete + prune at LLM discretion.

Disk-is-truth: its state is a YAML file (`state.yaml`) in the agent's own
dir under `.fantastic` — human-editable, git-diffable, atomic-write
(temp + os.replace). It's a sidecar this bundle owns directly: `set`/
`delete`/`replace` write through immediately (the single-agent inbox
serializes them, so no locking), independent of the kernel — unlike the
agent RECORD (agent.json), which a loader persists by observing the state
stream. Cascade-delete detaches the agent; the loader rmtrees the dir
(state.yaml included) on the `removed` event — so still no `on_delete`.

Keys are flat namespaced strings (dotted convention:
`domain.subject.attribute`, e.g. `user.name`, `decision.db`). Values are
arbitrary JSON.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

import yaml

_MEM_SENTENCE = (
    "Your durable memory. Facts you must remember across sessions live here "
    "— auto-loaded into your context on boot. `set` a descriptive key the "
    "moment the user tells you something worth keeping (a name, a preference, "
    "a decision). Your current facts are already in your context — read them, "
    "don't re-fetch."
)
_DATA_SENTENCE = (
    "Your durable scratch-state (component state, config, run params, current "
    "selection). One value per key, overwrite-in-place; auto-loaded into your "
    "context on boot."
)


def _state_path(agent) -> Path:
    return agent._root_path / "state.yaml"


def _mode(agent) -> str:
    rec = agent.get(agent.id) or {}
    m = rec.get("mode")
    return m if m in ("mem", "data") else "data"


def _load(agent) -> dict[str, Any]:
    p = _state_path(agent)
    if not p.exists():
        return {}
    try:
        doc = yaml.safe_load(p.read_text(encoding="utf-8"))
    except (OSError, yaml.YAMLError):
        return {}
    return doc if isinstance(doc, dict) else {}


def _emit_yaml(doc: dict[str, Any]) -> str:
    if not doc:
        return ""
    return yaml.safe_dump(doc, sort_keys=True, allow_unicode=True)


def _dump(agent, doc: dict[str, Any]) -> None:
    p = _state_path(agent)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_name(p.name + ".tmp")
    tmp.write_text(_emit_yaml(doc), encoding="utf-8")
    os.replace(tmp, p)  # atomic


async def _reflect(id, payload, agent):
    """Identity + mode + current key count. No args."""
    doc = _load(agent)
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
    doc = _load(agent)
    key = payload.get("key")
    if key is None:
        return {"doc": doc}
    return {"key": key, "value": doc.get(key)}


async def _keys(id, payload, agent):
    """args: none. List keys + value sizes — the table-of-contents."""
    doc = _load(agent)
    return {"keys": [{"key": k, "size": len(str(v))} for k, v in sorted(doc.items())]}


async def _set(id, payload, agent):
    """args: key:str, value:any. Upsert one key (data: overwrite; mem: accrete a fact)."""
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "yaml_state.set: key (non-empty str) required"}
    if "value" not in payload:
        return {"error": "yaml_state.set: value required"}
    doc = _load(agent)
    doc[key] = payload["value"]
    _dump(agent, doc)
    return {"key": key, "set": True}


async def _delete(id, payload, agent):
    """args: key:str. Remove a key (prune / clear state)."""
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "yaml_state.delete: key (non-empty str) required"}
    doc = _load(agent)
    existed = key in doc
    doc.pop(key, None)
    _dump(agent, doc)
    return {"key": key, "deleted": existed}


async def _replace(id, payload, agent):
    """args: doc:object. Overwrite the whole store (distill/prune-rewrite; {} clears)."""
    if "doc" not in payload:
        return {"error": "yaml_state.replace: doc (object) required"}
    doc = payload["doc"]
    if not isinstance(doc, dict):
        return {"error": "yaml_state.replace: doc must be an object"}
    _dump(agent, doc)
    return {"replaced": True, "keys": len(doc)}


async def _state_yaml(id, payload, agent):
    """args: none. The entire store as YAML text — the exact block injected on boot."""
    return {"yaml": _emit_yaml(_load(agent))}


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
