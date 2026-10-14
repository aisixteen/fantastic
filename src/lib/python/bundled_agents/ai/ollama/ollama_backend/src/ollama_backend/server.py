"""A record that leaves `model` out has it READ FROM THE SERVER (the CLI's
`ai ollama -p PORT`).

Before a verb that runs a turn (`TURN_VERBS`), an absent `model` is read from the
server the record's `endpoint` names — the ONE model `GET {endpoint}/api/tags`
lists — and WRITTEN BACK to the record, so reflect shows it and it is not read
again until the field is cleared (`null`). A server listing several names them and
the record must pick one. A model set on the record is used as set.

Only the model: the window ollama loads a model with is the
request's `num_ctx`, else the Modelfile's, else the server's default — and that
default is not in the API, so nothing about the window is read; it stays the
record's (`num_ctx` / `context_window`).

A server that does not answer refuses the verb (`bad_arg`, `arg: model`), naming the
URL and what came back. The record's key, when it names one (`credentials`), rides
the request as on a chat.
"""

from __future__ import annotations

import json

import httpx
from ai_core.credentials import CredentialError
from kernel._refusal import BAD_ARG, refusal

#: The verbs that run a turn of the model — the ones the server is read for.
TURN_VERBS = ("send", "inquiry", "context_recall")
#: Seconds to reach the server, and to wait on the model list.
CONNECT_TIMEOUT = 5.0
READ_TIMEOUT = 30.0


class ServerReadError(RuntimeError):
    pass


def served_model(body: str, url: str) -> str:
    """The ONE model a `/api/tags` body lists."""
    try:
        data = json.loads(body).get("models")
    except (ValueError, AttributeError):
        data = None
    if not isinstance(data, list):
        raise ServerReadError(f"model: GET {url} is not a model list")
    names = [m.get("name") for m in data if isinstance(m, dict)]
    if len(names) != 1 or not isinstance(names[0], str) or not names[0]:
        served = ", ".join(str(n) for n in names) or "nothing"
        raise ServerReadError(
            f"model: {url} serves {served} — not one model; set model on the record"
        )
    return names[0]


async def read_model(endpoint: str, *, key, transport=None) -> str:
    """The model the server at `endpoint` serves. Raises `ServerReadError`."""
    url = f"{endpoint.rstrip('/')}/api/tags"
    try:
        secret = await key(endpoint)
    except CredentialError as e:
        raise ServerReadError(str(e)) from e
    headers = {"Authorization": f"Bearer {secret}"} if secret else {}
    try:
        async with httpx.AsyncClient(
            timeout=httpx.Timeout(READ_TIMEOUT, connect=CONNECT_TIMEOUT),
            transport=transport,
        ) as client:
            resp = await client.get(url, headers=headers)
    except httpx.HTTPError as e:
        raise ServerReadError(
            f"model: GET {url} failed — the server is not reachable there; set "
            "model on the record"
        ) from e
    if resp.status_code != 200:
        raise ServerReadError(
            f"model: GET {url} answered HTTP {resp.status_code} — the server (or a "
            "door in front of it) does not serve it; set model on the record"
        )
    return served_model(resp.text, url)


async def resolve(id: str, kernel, *, key, transport=None) -> dict | None:
    """Read an absent `model` from the server and write it back. None when the
    record names one or it was written; otherwise the verb's refusal."""
    rec = kernel.get(id) or {}
    # Nothing to read without an endpoint — the provider names it when it is built.
    if rec.get("model") is not None or not str(rec.get("endpoint") or "").strip():
        return None
    try:
        model = await read_model(
            str(rec.get("endpoint") or ""), key=key, transport=transport
        )
    except ServerReadError as e:
        return refusal(BAD_ARG, str(e), arg="model")
    kernel.update(id, model=model)
    return None
