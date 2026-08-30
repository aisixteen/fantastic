"""durable_memory — a modeless durable KV agent.

One store, one discipline: keys in, values out. What the store is FOR lives in
the agent's `description`, where every agent already says it — not in a
bundle-private `mode` enum serving three canned sentences. (An earlier design
carried `mode: mem|data|chat`; the verbs were identical in all three, so the
modes were dropped.)

NO FS OF ITS OWN. The record names `file_bridge_id`, and every byte moves
through kernel sends to that leg over the AGNOSTIC STREAM CHANNEL —
`write_stream` / `read_stream`, raw bytes, no string body anywhere. This bundle
never imports `open()`/`Path`; a store with no leg wired FAILFASTS on write
(no silent RAM) and reads empty.

YAML is the intermediate: the live state is one dict, serialized to YAML bytes
on every write, parsed on read. Disk-is-truth — read fresh each call, no cache,
so an external edit is seen on the next read — and the sidecar stays
human-editable, greppable and git-diffable.

Keys are flat namespaced strings (dotted convention: `domain.subject.attribute`).
Values are arbitrary JSON. `get` is a RESERVED kernel verb, so the read verb is
`read`.
"""

from __future__ import annotations

from typing import Any

import yaml

from kernel import operator_context

#: Where the store lands inside the bound leg's root.
STATE_PATH = "state.yaml"


def _fbid(agent) -> str | None:
    rec = agent.get(agent.id) or {}
    v = rec.get("file_bridge_id")
    return v if isinstance(v, str) and v else None


def _emit(doc: dict[str, Any]) -> bytes:
    if not doc:
        return b""
    return yaml.safe_dump(doc, sort_keys=True, allow_unicode=True).encode("utf-8")


async def _load(agent) -> dict[str, Any]:
    """The whole store, via `read_stream` chunks off the bound leg. Missing leg,
    missing file, or unparseable content ⇒ {} — reads are lenient; writes are
    where loss would hide, and they failfast instead."""
    fid = _fbid(agent)
    if not fid:
        return {}
    chunks: list[bytes] = []
    offset = 0
    # Persistence is processed from ABOVE: reading/writing your own sidecar is
    # operator bookkeeping (a side effect of your authorized verb), not a
    # lateral call — so it runs ungated, like create_agent's child-boot.
    with operator_context():
        while True:
            r = (
                await agent.send(
                    [
                        {
                            "target": fid,
                            "payload": {
                                "type": "read_stream",
                                "path": STATE_PATH,
                                "offset": offset,
                            },
                        }
                    ]
                )
            )[0]["result"]
            if not isinstance(r, dict) or r.get("error"):
                return {}
            b = r.get("bytes")
            if isinstance(b, (bytes, bytearray)) and b:
                chunks.append(bytes(b))
            if r.get("eof") or not b:
                break
            offset = r.get("next_offset", offset + len(b))
    raw = b"".join(chunks)
    if not raw:
        return {}
    try:
        doc = yaml.safe_load(raw.decode("utf-8", errors="replace"))
    except yaml.YAMLError:
        return {}
    return doc if isinstance(doc, dict) else {}


async def _persist(agent, doc: dict[str, Any], verb: str) -> dict | None:
    """Write the whole store as ONE truncating `write_stream` chunk to the bound
    leg. An unwired or refusing leg is surfaced as the verb's error — never
    silent RAM. Returns an error dict, or None on success."""
    fid = _fbid(agent)
    if not fid:
        return {
            "error": (
                f"durable_memory.{verb}: no file_bridge_id wired — writes have "
                "nowhere durable to land, refusing rather than keeping state in RAM"
            )
        }
    with operator_context():  # persist from above — see _load
        w = (
            await agent.send(
                [
                    {
                        "target": fid,
                        "payload": {
                            "type": "write_stream",
                            "path": STATE_PATH,
                            "bytes": _emit(doc),
                            "truncate": True,
                        },
                    }
                ]
            )
        )[0]["result"]
    if not isinstance(w, dict):
        return {"error": f"durable_memory.{verb}: sink gave no reply"}
    if w.get("error"):
        out = {"error": f"durable_memory.{verb}: {w['error']}"}
        for k in ("reason", "hint"):
            if w.get(k):
                out[k] = w[k]
        return out
    return None


async def _reflect(id, payload, agent):
    rec = agent.get(id) or {}
    doc = await _load(agent)
    return {
        "id": id,
        "sentence": rec.get("description")
        or "A durable key/value store. `set`/`read`/`append` named keys; state "
        "survives restarts through its bound file leg.",
        "file_bridge_id": _fbid(agent),
        "keys": sorted(doc),
        "verbs": {
            "read": "args: key:str — {key, value} (value null when absent)",
            "set": "args: key:str, value:any — stores it (overwrite)",
            "append": "args: key:str, value:any — appends to the LIST at key "
            "(created if absent; refuses a non-list)",
            "keys": "no args — {keys: [...]}",
            "delete": "args: key:str — {key, deleted:bool}",
        },
    }


async def _boot(id, payload, agent):
    return None


async def _read(id, payload, agent):
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "durable_memory.read: key (str) required"}
    doc = await _load(agent)
    return {"key": key, "value": doc.get(key)}


async def _keys(id, payload, agent):
    return {"keys": sorted(await _load(agent))}


async def _set(id, payload, agent):
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "durable_memory.set: key (str) required"}
    if "value" not in payload:
        return {"error": "durable_memory.set: value required"}
    doc = await _load(agent)
    doc[key] = payload["value"]
    if err := await _persist(agent, doc, "set"):
        return err
    return {"key": key, "ok": True}


async def _append(id, payload, agent):
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "durable_memory.append: key (str) required"}
    if "value" not in payload:
        return {"error": "durable_memory.append: value required"}
    doc = await _load(agent)
    cur = doc.get(key)
    if cur is None:
        cur = []
    if not isinstance(cur, list):
        return {
            "error": f"durable_memory.append: key {key!r} holds a non-list — "
            "`set` it to a list first, or pick another key"
        }
    cur.append(payload["value"])
    doc[key] = cur
    if err := await _persist(agent, doc, "append"):
        return err
    return {"key": key, "ok": True, "length": len(cur)}


async def _delete(id, payload, agent):
    key = payload.get("key")
    if not isinstance(key, str) or not key:
        return {"error": "durable_memory.delete: key (str) required"}
    doc = await _load(agent)
    existed = key in doc
    doc.pop(key, None)
    if existed:
        if err := await _persist(agent, doc, "delete"):
            return err
    return {"key": key, "deleted": existed}


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "read": _read,
    "keys": _keys,
    "set": _set,
    "append": _append,
    "delete": _delete,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return {"error": f"durable_memory: unknown type {payload.get('type')!r}"}
    return await fn(id, payload, agent)
