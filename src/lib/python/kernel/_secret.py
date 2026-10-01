"""SECRET VALUES on a call — the caller marks them, the kernel never records them.

A batch item may carry `secret: [path, …]` beside `target`/`payload`/`timeout`
(owner, 2026-10-01). A path is dotted and rooted at the item: `payload.<field…>`
names a field of the payload, `result` the whole reply, `result.<field…>` a field
of it. The TARGET is handed the real payload and the CALLER the real reply; every
place the kernel RECORDS or SHOWS the call carries `<secret>` instead — the call
and settle rows (and so the memory store), the `ledger` read-back and dedup, the
state stream's `payload` and `summary`, the inbox and watcher mirrors, and any
error text the call's outcome carries.

Two limits, by design:
  * masking is not isolation — the receiver holds the value; send secrets to a
    bundle that keeps them, never to an AI agent (they would enter its prompt and
    its memory);
  * a call whose record holds placeholders cannot be answered FROM that record:
    re-sending a named execution with secrets after it left RAM is refused, loud.

Not secret-able: `payload.type` — the verb is how every row is read."""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from typing import Any

from kernel._refusal import BAD_ARG, refusal

PLACEHOLDER = "<secret>"


@dataclass
class Secret:
    """What one item asked to be kept: the payload/result paths (as key lists),
    the paths as given (recorded on the call row — paths are not secret), and the
    payload's secret VALUES (RAM only — scrubbed out of every recorded string)."""

    paths: list[str]
    payload_paths: list[list[str]]
    result_paths: list[list[str]]
    values: list[str] = field(default_factory=list)


def _step(cur: Any, key: str) -> tuple[bool, Any]:
    if isinstance(cur, dict):
        return (key in cur, cur.get(key))
    if isinstance(cur, list) and key.isascii() and key.isdigit():
        i = int(key)
        return (i < len(cur), cur[i] if i < len(cur) else None)
    return (False, None)


def _get(obj: Any, keys: list[str]) -> tuple[bool, Any]:
    cur = obj
    for k in keys:
        ok, cur = _step(cur, k)
        if not ok:
            return (False, None)
    return (True, cur)


def _strings(x: Any) -> list[str]:
    """Every string a secret value holds — the value itself, or its leaves."""
    if isinstance(x, str):
        return [x] if x else []
    if isinstance(x, dict):
        return [s for v in x.values() for s in _strings(v)]
    if isinstance(x, list):
        return [s for v in x for s in _strings(v)]
    if x is None or isinstance(x, bool):
        return []
    return [str(x)]


def check(item: dict, payload: dict) -> tuple[Secret | None, dict | None]:
    """Validate an item's `secret`: (Secret | None, refusal | None). Absent ⇒
    nothing secret. A path the payload does not have is refused — a typo would
    otherwise record the value it meant to hide."""
    raw = item.get("secret")
    if raw is None:
        return None, None

    def bad(why: str) -> tuple[None, dict]:
        return None, refusal(BAD_ARG, f"send: secret {why}", arg="secret")

    if not isinstance(raw, list) or not raw:
        return bad(f"must be a non-empty list of paths, got {raw!r}")
    payload_paths: list[list[str]] = []
    result_paths: list[list[str]] = []
    for p in raw:
        if not isinstance(p, str) or not p:
            return bad(f"path must be a non-empty string, got {p!r}")
        keys = p.split(".")
        if any(not k for k in keys):
            return bad(f"path {p!r} has an empty step")
        if keys[0] == "result":
            result_paths.append(keys[1:])
        elif keys[0] == "payload" and len(keys) > 1:
            if keys[1:] == ["type"]:
                return bad(
                    "cannot hide payload.type — the verb is how every row is read"
                )
            ok, _ = _get(payload, keys[1:])
            if not ok:
                return bad(f"path {p!r} names nothing in the payload")
            payload_paths.append(keys[1:])
        else:
            return bad(f"path {p!r} must start with 'payload.' or be 'result[.…]'")
    values = [s for kp in payload_paths for s in _strings(_get(payload, kp)[1])]
    return Secret(list(raw), payload_paths, result_paths, values), None


def _put(obj: Any, keys: list[str]) -> Any:
    """`obj` with the value at `keys` replaced by the placeholder (a copy; a
    missing path leaves it as is). An empty `keys` is the whole value."""
    if not keys:
        return PLACEHOLDER
    ok, _ = _get(obj, keys)
    if not ok:
        return obj
    out = copy.deepcopy(obj)
    cur = out
    for k in keys[:-1]:
        cur = cur[int(k)] if isinstance(cur, list) else cur[k]
    last = keys[-1]
    if isinstance(cur, list):
        cur[int(last)] = PLACEHOLDER
    else:
        cur[last] = PLACEHOLDER
    return out


def scrub(x: Any, values: list[str]) -> Any:
    """Every occurrence of a secret value inside any string of `x` → placeholder —
    an error text that quotes the key must not carry it into a row or a log."""
    if not values:
        return x
    if isinstance(x, str):
        for v in sorted(values, key=len, reverse=True):
            if v in x:
                x = x.replace(v, PLACEHOLDER)
        return x
    if isinstance(x, dict):
        return {k: scrub(v, values) for k, v in x.items()}
    if isinstance(x, list):
        return [scrub(v, values) for v in x]
    return x


def shown_payload(payload: dict, s: Secret) -> dict:
    """The payload as recorded and shown: secret fields → placeholder."""
    out = payload
    for kp in s.payload_paths:
        out = _put(out, kp)
    return scrub(out, s.values)


def shown_outcome(out: dict, s: Secret) -> dict:
    """An outcome as recorded and shown: secret result fields → placeholder, and
    every secret value (the payload's and the result's own) scrubbed from its text."""
    res = out.get("result")
    values = list(s.values)
    for kp in s.result_paths:
        ok, v = _get(res, kp) if kp else (True, res)
        if ok:
            values += _strings(v)
        res = _put(res, kp)
    # the RESULT only: `status`, `pid` and the rest are the kernel's own words — a
    # short secret must not mangle "completed" into "compl<secret>t<secret>d"
    return {**out, "result": scrub(res, values)}


def replay_refused(name_or_id: Any) -> dict:
    """The answer to re-sending an execution whose record holds placeholders."""
    return refusal(
        BAD_ARG,
        f"send: execution {name_or_id!r} carried secrets — its record holds "
        f"{PLACEHOLDER} placeholders, not values, so it cannot be answered from "
        "the record; send again under a new id",
        arg="id",
    )
