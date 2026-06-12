"""yaml_state — a durable YAML key-value memory agent.

One bundle, instantiated as N agents. The `mode` meta ("mem" | "data")
sets the *discipline* (and the reflect sentence) — the verbs are
identical:

  - data → current scratch-state, overwrite-in-place.
  - mem  → durable keyed facts, accrete + prune at LLM discretion.

ALL disk IO goes THROUGH a `file_bridge` AGENT (the gated fs edge — sealed /
deny-all by default), referenced by `file_bridge_id` on this agent's record. This
bundle owns NO disk surface of its own and never imports `fs`: it `send`s `read` /
`write` verbs to its provider, exactly like `scheduler` / `ai_core`. The provider is
wired (and OPENED) on demand by the operator/LLM — `set` / `delete` / `replace`
**failfast** until `file_bridge_id` is set, and surface a denied write rather than
losing it silently. Wire it to the **`.fantastic` store** (the same one the loader
persists records through — ONE file_bridge serves both): the path is store-relative
`agents/<id>/state.yaml`, so the sidecar lands NEXT TO its `agent.json`. Human-editable,
git-diffable, disk-is-truth (read fresh each call, no cache — an external edit is seen
on the next read).

Keys are flat namespaced strings (dotted convention:
`domain.subject.attribute`, e.g. `user.name`, `decision.db`). Values are
arbitrary JSON.
"""

from __future__ import annotations

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


def _file_bridge_id(agent) -> str | None:
    rec = agent.get(agent.id) or {}
    return rec.get("file_bridge_id")


def _state_path(agent) -> str:
    """state.yaml in the agent's own dir, RELATIVE to the provider's root. The provider
    is the `.fantastic` store (the one the loader persists records through), so the path
    is `agents/<id>/state.yaml` — landing the sidecar NEXT TO its `agent.json` under the
    single store (no `.fantastic/.fantastic/...` double-nest)."""
    return f"agents/{agent.id}/state.yaml"


def _mode(agent) -> str:
    rec = agent.get(agent.id) or {}
    m = rec.get("mode")
    return m if m in ("mem", "data") else "data"


def _emit_yaml(doc: dict[str, Any]) -> str:
    if not doc:
        return ""
    return yaml.safe_dump(doc, sort_keys=True, allow_unicode=True)


def _need_file_bridge(agent, verb: str) -> dict | None:
    """Failfast if no provider is wired — persistence needs an opened file_bridge."""
    if not _file_bridge_id(agent):
        return {
            "error": (
                f"yaml_state.{verb}: file_bridge_id required — wire (and open) a "
                "file_bridge to persist"
            )
        }
    return None


async def _load(agent) -> dict[str, Any]:
    """Read the store THROUGH the wired provider. Unwired / missing / denied ⇒ {}."""
    fid = _file_bridge_id(agent)
    if not fid:
        return {}
    r = await agent.send(fid, {"type": "read", "path": _state_path(agent)})
    if not isinstance(r, dict) or "content" not in r:
        return {}
    try:
        doc = yaml.safe_load(r["content"])
    except yaml.YAMLError:
        return {}
    return doc if isinstance(doc, dict) else {}


async def _persist(agent, doc: dict[str, Any], verb: str) -> dict | None:
    """Write the store THROUGH the provider; surface a denied/failed write as an error
    (no silent loss). Returns an error dict, or None on success."""
    fid = _file_bridge_id(agent)
    w = await agent.send(
        fid, {"type": "write", "path": _state_path(agent), "content": _emit_yaml(doc)}
    )
    if not isinstance(w, dict):
        return {"error": f"yaml_state.{verb}: provider gave no reply"}
    reason = w.get("error") or (
        w.get("reason") if w.get("reason") == "unauthorized" else None
    )
    if reason:
        out = {"error": f"yaml_state.{verb}: provider refused write — {reason}"}
        if w.get("hint"):
            out["hint"] = w["hint"]
        return out
    return None


async def _reflect(id, payload, agent):
    """Identity + mode + current key count + file_bridge_id binding. No args."""
    doc = await _load(agent)
    mode = _mode(agent)
    return {
        "id": id,
        "sentence": _MEM_SENTENCE if mode == "mem" else _DATA_SENTENCE,
        "mode": mode,
        "key_count": len(doc),
        "file_bridge_id": _file_bridge_id(agent),
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
    """args: key:str, value:any. Upsert one key (data: overwrite; mem: accrete a fact). Persisted through file_bridge_id; failfast if unwired."""
    if err := _need_file_bridge(agent, "set"):
        return err
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
    """args: key:str. Remove a key (prune / clear state). Persisted through file_bridge_id; failfast if unwired."""
    if err := _need_file_bridge(agent, "delete"):
        return err
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
    """args: doc:object. Overwrite the whole store (distill/prune-rewrite; {} clears). Persisted through file_bridge_id; failfast if unwired."""
    if err := _need_file_bridge(agent, "replace"):
        return err
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
