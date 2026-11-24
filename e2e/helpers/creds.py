"""THE KEYS, REACHED THE SAME WAY ON EVERY RUNTIME — a credential store.

No backend reads a key from the environment: an AI agent's record names
`credentials: {store, ref}` and the backend reads `api_key` from that
`credential_store` before each request, pinned to its endpoint's host
(`ai_core/credentials.py`). Every e2e kernel gets one store — `creds` — with the
`env` backend (read-only): the run still exports the key it always did
(`OPENAI_API_KEY`, `VLLM_API_KEY`), and the STORE reads it. One ref per keyed
plug, bound to the host of that plug's endpoint.

Wiring, PER AGENT: a key user holds `creds: {call: [get]}` in its own `can`
(`helpers/ai.py`). Nothing is inherited (rule 1) — no line reaches an agent
whose record does not carry it, and the store keeps no list of callers.
"""

from __future__ import annotations

import os

from helpers import lines

#: The one store id every e2e kernel carries.
CREDS = "creds"

#: A key user's line on the store: read a key. Nothing writes in a run.
GET = lines.GET

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
    A key user holds `creds: {call: [get]}`; nothing is inherited (rule 1)."""
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


def creator_line(what_bundle: str) -> dict:
    """The forced `can` for a creator's `create` line on a key-using bundle: put it in
    `what[what_bundle]`. A provider the creator makes there holds `GET` only through
    this forced line, counted as handed out by whoever wrote the `create` line, and
    checked once, when that line is written."""
    del what_bundle  # the forced value is the same for every key-using bundle
    return {"can": {CREDS: GET}}
