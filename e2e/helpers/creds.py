"""THE KEYS, REACHED THE SAME WAY ON EVERY RUNTIME — a credential store.

No backend reads a key from the environment: an AI agent's record names
`credentials: {store, ref}` and the backend reads `api_key` from that
`credential_store` before each request, pinned to its endpoint's host
(`ai_core/credentials.py`). Every e2e kernel gets one store — `creds` — with the
`env` backend (read-only): the run still exports the key it always did
(`OPENAI_API_KEY`, `VLLM_API_KEY`), and the STORE reads it. One ref per keyed
plug, bound to the host of that plug's endpoint.

Wiring, explicit on both ends, PER AGENT: an AI agent the harness creates gets
`can_call: {creds: [get]}` and the store's `callable_by` names it (`helpers/ai.py`).
Nothing is inherited — no grant reaches an agent it does not name.
"""

from __future__ import annotations

import os
from typing import Any

#: The one store id every e2e kernel carries.
CREDS = "creds"

#: What an agent may do on the store: read a key. Nothing writes in a run.
GET = ["get"]

#: Per keyed plug: (ref, the env var the store reads, the env var naming the
#: plug's endpoint). The ref IS the plug name.
KEYED: dict[str, tuple[str, str]] = {
    "responses": ("OPENAI_API_KEY", "RESPONSES_ENDPOINT"),
    "vllm": ("VLLM_API_KEY", "VLLM_ENDPOINT"),
    "anthropic": ("ANTHROPIC_API_KEY", "ANTHROPIC_ENDPOINT"),
}


def credentials(ref: str) -> dict:
    """The `credentials` field of an AI record reading `ref` from `creds`."""
    return {"store": CREDS, "ref": ref}


def env_refs() -> dict[str, tuple[str, str]]:
    """`{ref: (env var, endpoint)}` for every keyed plug whose endpoint this run
    names — the store a live run seeds. A ref whose key var is unset still
    exists: reading it fails loud, naming the variable."""
    return {
        ref: (var, os.environ[ep_var])
        for ref, (var, ep_var) in KEYED.items()
        if os.environ.get(ep_var)
    }


def store_record(root_id: str, refs: dict[str, tuple[str, str]]) -> dict:
    """The `creds` record: env-backed, one ref per `{ref: (env var, endpoint)}`.
    Its `callable_by` starts empty — each AI agent is added as it is created."""
    return {
        "id": CREDS,
        "handler_module": "credential_store.tools",
        "parent_id": root_id,
        "backend": "env",
        "refs": {
            ref: {"host": endpoint, "env": {"api_key": var}}
            for ref, (var, endpoint) in refs.items()
        },
    }


def wire_in_process(root: Any, refs: dict[str, tuple[str, str]]) -> None:
    """In-process kernel: create `creds` under the root."""
    ctx = root.ctx
    if CREDS not in ctx.agents:
        rec = store_record(root.id, refs)
        made = root.create(
            rec.pop("handler_module"),
            **{k: v for k, v in rec.items() if k != "parent_id"},
        )
        assert made.get("id") == CREDS, made


async def open_store(root: Any, aid: str) -> None:
    """The STORE's half alone: `creds` accepts `get` from `aid` — for an agent that does
    not exist yet (a probe's model creates it and writes its own `can_call`)."""
    listed = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]
    recs = {a.get("id"): a for a in ((listed.get("result") or {}).get("agents") or [])}
    grants = dict((recs.get(CREDS) or {}).get("callable_by") or {})
    if grants.get(aid) in (GET, "*"):
        return
    grants[aid] = GET
    out = (await root.send([{"target": "kernel", "payload": {
        "type": "update_agent", "id": CREDS, "callable_by": grants}}]))[0]["result"]
    if isinstance(out, dict) and out.get("error"):
        raise AssertionError(f"open_store({aid}): creds.callable_by refused: {out}")


async def grant_reader(root: Any, aid: str) -> None:
    """Open BOTH halves of `aid` → `creds` for `get`: the agent's `can_call` and the
    store's `callable_by` — read, add one key, write back (update replaces a field
    whole). For every AI agent a keyed plug drives; nothing is inherited."""
    listed = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]
    recs = {a.get("id"): a for a in ((listed.get("result") or {}).get("agents") or [])}
    for holder, field_, other in ((aid, "can_call", CREDS), (CREDS, "callable_by", aid)):
        grants = dict((recs.get(holder) or {}).get(field_) or {})
        if grants.get(other) in (GET, "*"):
            continue
        grants[other] = GET
        out = (await root.send([{"target": "kernel", "payload": {
            "type": "update_agent", "id": holder, field_: grants}}]))[0]["result"]
        if isinstance(out, dict) and out.get("error"):
            raise AssertionError(f"grant_reader({aid}): {holder}.{field_} refused: {out}")
