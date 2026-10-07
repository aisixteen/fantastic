"""What a record leaves out is READ FROM THE SERVER (owner, 2026-10-07: "defaults can
be inferred from the endpoint").

`model`, `max_tokens`, `context_window` and `kv_slots` may be left off the record.
Before a verb that runs a turn (`TURN_VERBS`), every one still absent is read from
the server the record's `endpoint` names and WRITTEN BACK to the record — it then
holds a measured value like one the operator set: reflect shows it, a reboot keeps
it, and the server is not asked again until the field is cleared (`null`). A field
the record sets is used as set, never read.

- `model` — the ONE model the server serves (`GET {endpoint}/models`); a server
  serving several names them, and the record must pick one.
- `context_window` and `max_tokens` — that model's `max_model_len` (same list). The
  same number for both: a main turn whose `max_tokens` reaches the window sends NO
  `max_tokens`, so the server bounds the reply by what the prompt leaves.
- `kv_slots` — how many full-length conversations the server's KV cache holds:
  `floor(kv_cache_max_concurrency)` of the `vllm:cache_config_info` line on the
  server's Prometheus page — vLLM's own figure for requests of `max_model_len`
  tokens. The page is the record's `metrics_url`, or, left out, the endpoint with
  its `/v1` replaced by `/metrics` (vLLM serves both on one port). A door in front
  of the server may not forward it.

A server that does not answer refuses the verb, naming the field, the URL and what
came back. The key goes only to the endpoint's host, as on a chat request.
"""

from __future__ import annotations

import json
import math
import re
from urllib.parse import urlsplit

import httpx
from ai_core.credentials import CredentialError
from kernel._refusal import BAD_ARG, refusal

#: The fields a record may leave to the server, in the order they are read.
INFERRED = ("model", "max_tokens", "context_window", "kv_slots")
#: The verbs that run a turn of the model — the ones the server is read for.
TURN_VERBS = ("send", "inquiry", "context_recall")

_INFO = "vllm:cache_config_info"
_LABEL = re.compile(r'(\w+)="((?:[^"\\]|\\.)*)"')


class ServerReadError(RuntimeError):
    """A field the server did not answer; `field` names it."""

    def __init__(self, field: str, message: str):
        super().__init__(message)
        self.field = field


def missing(rec: dict) -> list[str]:
    """The record's fields left to the server."""
    return [f for f in INFERRED if rec.get(f) is None]


def metrics_url(rec: dict) -> str | None:
    """The record's `metrics_url`, or the endpoint's `/v1` replaced by `/metrics`;
    None when neither names one (an endpoint that does not end in `/v1`)."""
    url = rec.get("metrics_url")
    if isinstance(url, str) and url.strip():
        return url.strip()
    endpoint = str(rec.get("endpoint") or "").rstrip("/")
    if endpoint.endswith("/v1"):
        return endpoint[: -len("/v1")] + "/metrics"
    return None


async def _get(client: httpx.AsyncClient, url: str, headers: dict, field: str) -> str:
    try:
        resp = await client.get(url, headers=headers)
    except httpx.HTTPError as e:
        raise ServerReadError(
            field,
            f"{field}: GET {url} failed — the server is not reachable there; set "
            f"{field} on the record",
        ) from e
    if resp.status_code != 200:
        raise ServerReadError(
            field,
            f"{field}: GET {url} answered HTTP {resp.status_code} — the server (or a "
            f"door in front of it) does not serve it; set {field} on the record",
        )
    return resp.text


def served_model(body: str, model: str | None, url: str, field: str) -> dict:
    """The `/models` entry the record names, or the ONE the server serves."""
    try:
        data = json.loads(body).get("data")
    except (ValueError, AttributeError):
        data = None
    if not isinstance(data, list):
        raise ServerReadError(field, f"{field}: GET {url} is not a model list")
    entries = [m for m in data if isinstance(m, dict)]
    served = ", ".join(str(m.get("id")) for m in entries) or "nothing"
    if model is None:
        if len(entries) != 1:
            raise ServerReadError(
                "model",
                f"model: {url} serves {served} — not one model; set model on the record",
            )
        return entries[0]
    entry = next((m for m in entries if m.get("id") == model), None)
    if entry is None:
        raise ServerReadError(
            field, f"{field}: {url} does not serve model {model!r} — it serves {served}"
        )
    return entry


def max_model_len(entry: dict, url: str, field: str) -> int:
    n = entry.get("max_model_len")
    if isinstance(n, bool) or not isinstance(n, int) or n < 1:
        raise ServerReadError(
            field,
            f"{field}: {url} names no max_model_len for {entry.get('id')!r}; set "
            f"{field} on the record",
        )
    return n


def kv_slots(body: str, url: str) -> int:
    """`floor(kv_cache_max_concurrency)` of the ONE `vllm:cache_config_info` line."""
    lines = [ln for ln in body.splitlines() if ln.startswith(_INFO + "{")]
    if len(lines) != 1:
        raise ServerReadError(
            "kv_slots",
            f"kv_slots: {url} carries {len(lines)} {_INFO} lines, not one — set "
            "kv_slots on the record",
        )
    labels = dict(_LABEL.findall(lines[0]))
    try:
        slots = math.floor(float(labels["kv_cache_max_concurrency"]))
    except (KeyError, ValueError):
        raise ServerReadError(
            "kv_slots",
            f"kv_slots: {url} names no kv_cache_max_concurrency — set kv_slots on "
            "the record",
        ) from None
    if slots < 1:
        raise ServerReadError(
            "kv_slots",
            f"kv_slots: {url} holds {labels['kv_cache_max_concurrency']} full-length "
            "conversations, under one — set kv_slots on the record",
        )
    return slots


def _same_host(a: str, b: str) -> bool:
    return urlsplit(a).netloc == urlsplit(b).netloc


async def read(rec: dict, *, key, transport=None, timeouts=(5.0, 900.0)) -> dict:
    """The values for the record's absent fields, read from the server. Raises
    `ServerReadError` naming the first field it could not read."""
    want = missing(rec)
    endpoint = str(rec.get("endpoint") or "").rstrip("/")
    try:
        secret = await key(endpoint)
    except CredentialError as e:
        raise ServerReadError(want[0], str(e)) from e
    out: dict = {}
    async with httpx.AsyncClient(
        timeout=httpx.Timeout(timeouts[1], connect=timeouts[0]), transport=transport
    ) as client:
        if {"model", "max_tokens", "context_window"} & set(want):
            field = next(f for f in want if f != "kv_slots")
            url = f"{endpoint}/models"
            headers = {"Authorization": f"Bearer {secret}"} if secret else {}
            entry = served_model(
                await _get(client, url, headers, field), rec.get("model"), url, field
            )
            if "model" in want:
                out["model"] = entry.get("id")
            for f in ("max_tokens", "context_window"):
                if f in want:
                    out[f] = max_model_len(entry, url, f)
        if "kv_slots" in want:
            url = metrics_url(rec)
            if url is None:
                raise ServerReadError(
                    "kv_slots",
                    f"kv_slots: the endpoint {endpoint} does not end in /v1, so the "
                    "server's metrics page is not known — set metrics_url or kv_slots "
                    "on the record",
                )
            headers = (
                {"Authorization": f"Bearer {secret}"}
                if secret and _same_host(url, endpoint)
                else {}
            )
            out["kv_slots"] = kv_slots(
                await _get(client, url, headers, "kv_slots"), url
            )
    return out


async def resolve(id: str, kernel, *, key, validate, transport=None) -> dict | None:
    """Read the record's absent fields from the server and write them back. None
    when nothing is absent or all were written; otherwise the verb's refusal."""
    rec = kernel.get(id) or {}
    if not missing(rec):
        return None
    timeouts = (
        float(rec.get("connect_timeout") or 5.0),
        float(rec.get("read_timeout") or 900.0),
    )
    try:
        values = await read(rec, key=key, transport=transport, timeouts=timeouts)
    except ServerReadError as e:
        return refusal(BAD_ARG, str(e), arg=e.field)
    # What the server gave passes the same write check as the operator's values.
    bad = validate(values, rec)
    if bad:
        return refusal(
            BAD_ARG,
            f"the server's values do not fit the record: {bad}",
            arg=next(iter(values)),
        )
    kernel.update(id, **values)
    return None
