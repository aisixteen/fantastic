"""What the record leaves out is READ FROM THE API (owner, 2026-10-07) — the same
contract as vllm_backend's `server.py`.

`max_tokens` and `context_window` may be left off the record. Before a verb that
runs a turn (`TURN_VERBS`), each one still absent is read from the API's model
entry — `GET {models}/{model}`, where `{models}` is the record's endpoint with its
`/messages` replaced by `/models` — and WRITTEN BACK to the record:

- `max_tokens` — the entry's `max_tokens` (the most a request may ask for);
- `context_window` — its `max_input_tokens`.

`model` is the record's to name (the API serves many); a field set on the record is
used as set, never read. An API that does not answer refuses the verb, naming the
field, the URL and what came back. The key rides as on a chat request.
"""

from __future__ import annotations

import json

import httpx
from ai_core.credentials import CredentialError
from kernel._refusal import BAD_ARG, refusal

from anthropic_backend.provider import CONNECT_TIMEOUT, headers

#: The fields a record may leave to the API, in the order they are read.
INFERRED = ("max_tokens", "context_window")
#: The entry field each one is read from.
SOURCE = {"max_tokens": "max_tokens", "context_window": "max_input_tokens"}
#: The verbs that run a turn of the model — the ones the API is read for.
TURN_VERBS = ("send", "inquiry", "context_recall")
#: Seconds to wait on the model entry.
READ_TIMEOUT = 30.0


class ServerReadError(RuntimeError):
    def __init__(self, field: str, message: str):
        super().__init__(message)
        self.field = field


def missing(rec: dict) -> list[str]:
    """The record's fields left to the API."""
    return [f for f in INFERRED if rec.get(f) is None]


def models_url(endpoint: str) -> str | None:
    """The model list beside a Messages endpoint; None when the endpoint does not
    end in `/messages`."""
    endpoint = endpoint.rstrip("/")
    if endpoint.endswith("/messages"):
        return endpoint[: -len("/messages")] + "/models"
    return None


async def read(rec: dict, *, key, transport=None) -> dict:
    """The values for the record's absent fields. Raises `ServerReadError`."""
    want = missing(rec)
    endpoint = str(rec.get("endpoint") or "").rstrip("/")
    base = models_url(endpoint)
    if base is None:
        raise ServerReadError(
            want[0],
            f"{want[0]}: the endpoint {endpoint} does not end in /messages, so the "
            f"model list is not known — set {want[0]} on the record",
        )
    url = f"{base}/{rec.get('model')}"
    try:
        secret = await key(endpoint)
    except CredentialError as e:
        raise ServerReadError(want[0], str(e)) from e
    try:
        async with httpx.AsyncClient(
            timeout=httpx.Timeout(READ_TIMEOUT, connect=CONNECT_TIMEOUT),
            transport=transport,
        ) as client:
            resp = await client.get(url, headers=headers(secret))
    except httpx.HTTPError as e:
        raise ServerReadError(
            want[0],
            f"{want[0]}: GET {url} failed — the API is not reachable there; set "
            f"{want[0]} on the record",
        ) from e
    if resp.status_code != 200:
        why = (
            " — the API wants a key: name it in `credentials` ({store, ref})"
            if resp.status_code in (401, 403)
            else ""
        )
        raise ServerReadError(
            want[0],
            f"{want[0]}: GET {url} answered HTTP {resp.status_code}{why}; set "
            f"{want[0]} on the record",
        )
    try:
        entry = json.loads(resp.text)
    except ValueError:
        entry = None
    out: dict = {}
    for f in want:
        n = entry.get(SOURCE[f]) if isinstance(entry, dict) else None
        if isinstance(n, bool) or not isinstance(n, int) or n < 1:
            raise ServerReadError(
                f, f"{f}: {url} names no {SOURCE[f]}; set {f} on the record"
            )
        out[f] = n
    return out


async def resolve(id: str, kernel, *, key, validate, transport=None) -> dict | None:
    """Read the record's absent fields from the API and write them back. None when
    nothing is absent or all were written; otherwise the verb's refusal."""
    rec = kernel.get(id) or {}
    if not missing(rec) or not rec.get("model") or not rec.get("endpoint"):
        # nothing to read, or nothing to read it with — the provider names a
        # missing `model` / `endpoint` when it is built
        return None
    try:
        values = await read(rec, key=key, transport=transport)
    except ServerReadError as e:
        return refusal(BAD_ARG, str(e), arg=e.field)
    bad = validate(values, rec)
    if bad:
        return refusal(
            BAD_ARG,
            f"the API's values do not fit the record: {bad}",
            arg=next(iter(values)),
        )
    kernel.update(id, **values)
    return None
