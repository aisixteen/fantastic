"""SECRET VALUES on a call — the caller marks them, the kernel never records them.

A batch item may carry `secret: [mark, …]` beside `target`/`payload`/`timeout`
(owner, 2026-10-01). A mark is rooted at the item: `payload` + a field of the
payload, `result` (the whole reply), or `result` + a field of it. The TARGET is
handed the real payload and the CALLER the real reply; every place the kernel
RECORDS or SHOWS the call carries `<secret>` instead — the call and settle rows (and
so the memory store), the `ledger` read-back and dedup, the state stream's `payload`
and `summary`, the inbox and watcher mirrors, and any error text the call's outcome
carries.

A mark is spelled one of two ways (one list may mix them):
  * a DOTTED STRING — `payload.key`, `result`, `result.fields.api_key` — split on
    its dots; at a list, a step of ASCII digits is an index;
  * an ARRAY OF STEPS (owner, 2026-10-03) — `["payload", "a.b"]`,
    `["result", "items", 0]` — exact, with the step rules of the binary frame's
    `_binary_path`: a STRING step is an object key taken literally (it may hold a
    `.`), an INTEGER step (non-negative) a list index. It is the only way to mark a
    key that holds a dot.

Two limits, by design:
  * masking is not isolation — the receiver holds the value; send secrets to a
    bundle that keeps them, never to an AI agent (they would enter its prompt and
    its memory);
  * a call whose record holds placeholders cannot be answered FROM that record:
    re-sending a named execution with secrets after it left RAM is refused, loud.

Not secret-able: `payload.type` — the verb is how every row is read; and any
payload field of `create_agent`/`update_agent` — the kernel writes those into the
agent's RECORD (its `agent_created`/`agent_updated` events, `agent.yaml`), where no
mark can hold. A value an agent must keep belongs in a `credential_store`, its
record naming the ref (owner-approved 2026-10-03, audit S2)."""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from typing import Any, NamedTuple

from kernel._refusal import BAD_ARG, refusal

PLACEHOLDER = "<secret>"

# The verbs whose every payload field (`type` aside) lands in an agent's record —
# the id, the handler, the placement, the meta. A mark there is refused (`check`).
_RECORD_VERBS = frozenset({"create_agent", "update_agent"})


class _Path(NamedTuple):
    """One mark below its root (`payload` / `result`): the steps, and whether they
    are EXACT (an array mark: a str step is only an object key, an int step only a
    list index) or DOTTED (a str step is a key, or at a list an index when it is
    ASCII digits)."""

    keys: list
    exact: bool


@dataclass
class Secret:
    """What one item asked to be kept: the payload/result paths, the marks as
    given (recorded on the call row — marks are not secret), and the payload's
    secret VALUES (RAM only — scrubbed out of every recorded string)."""

    paths: list
    payload_paths: list[_Path]
    result_paths: list[_Path]
    values: list[str] = field(default_factory=list)


def _step(cur: Any, key: Any, exact: bool) -> tuple[bool, Any]:
    if isinstance(cur, dict):
        if exact and not isinstance(key, str):
            return (False, None)
        return (key in cur, cur.get(key))
    if isinstance(cur, list):
        if exact:
            if type(key) is not int:
                return (False, None)
            i = key
        elif key.isascii() and key.isdigit():
            i = int(key)
        else:
            return (False, None)
        return (i < len(cur), cur[i] if i < len(cur) else None)
    return (False, None)


def _get(obj: Any, path: _Path) -> tuple[bool, Any]:
    cur = obj
    for k in path.keys:
        ok, cur = _step(cur, k, path.exact)
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


def _dotted(p: str) -> tuple[str | None, _Path | None, str | None]:
    """A dotted-string mark → (root, path below it, why-not)."""
    if not p:
        return None, None, f"path must be a non-empty string, got {p!r}"
    keys = p.split(".")
    if any(not k for k in keys):
        return None, None, f"path {p!r} has an empty step"
    if keys[0] == "result":
        return "result", _Path(keys[1:], False), None
    if keys[0] == "payload" and len(keys) > 1:
        return "payload", _Path(keys[1:], False), None
    return None, None, f"path {p!r} must start with 'payload.' or be 'result[.…]'"


def _array(p: list) -> tuple[str | None, _Path | None, str | None]:
    """An array-of-steps mark → (root, path below it, why-not)."""
    if not p:
        return None, None, "path [] has no steps"
    for s in p:
        if not isinstance(s, str) and (type(s) is not int or s < 0):
            return (
                None,
                None,
                f"path {p!r} step {s!r} is neither a string (an object key) nor a "
                "non-negative integer (a list index)",
            )
    if p[0] == "result":
        return "result", _Path(list(p[1:]), True), None
    if p[0] == "payload" and len(p) > 1:
        return "payload", _Path(list(p[1:]), True), None
    return (
        None,
        None,
        f"path {p!r} must start with 'payload' and a step under it, or with 'result'",
    )


def check(item: dict, payload: dict) -> tuple[Secret | None, dict | None]:
    """Validate an item's `secret`: (Secret | None, refusal | None). Absent ⇒
    nothing secret. A payload mark the payload does not have is refused — a typo
    would otherwise record the value it meant to hide."""
    raw = item.get("secret")
    if raw is None:
        return None, None

    def bad(why: str) -> tuple[None, dict]:
        return None, refusal(BAD_ARG, f"send: secret {why}", arg="secret")

    if not isinstance(raw, list) or not raw:
        return bad(f"must be a non-empty list of paths, got {raw!r}")
    payload_paths: list[_Path] = []
    result_paths: list[_Path] = []
    verb = payload.get("type")
    for p in raw:
        if isinstance(p, str):
            root, path, why = _dotted(p)
        elif isinstance(p, list):
            root, path, why = _array(p)
        else:
            return bad(f"path must be a dotted string or an array of steps, got {p!r}")
        if why is not None:
            return bad(why)
        if root == "result":
            result_paths.append(path)
            continue
        if path.keys == ["type"]:
            return bad("cannot hide payload.type — the verb is how every row is read")
        if verb in _RECORD_VERBS:
            return bad(
                f"path {p!r} names a field {verb} writes into the agent record: a "
                "record field cannot be secret — use a credential_store ref"
            )
        ok, _ = _get(payload, path)
        if not ok:
            return bad(f"path {p!r} names nothing in the payload")
        payload_paths.append(path)
    values = [s for kp in payload_paths for s in _strings(_get(payload, kp)[1])]
    # the marks AS GIVEN, an array copied — what the call row records and what the
    # handler reads back as `current_secret()`
    marks = [list(p) if isinstance(p, list) else p for p in raw]
    return Secret(marks, payload_paths, result_paths, values), None


def _put(obj: Any, path: _Path) -> Any:
    """`obj` with the value at `path` replaced by the placeholder (a copy; a
    missing path leaves it as is). No steps is the whole value."""
    keys = path.keys
    if not keys:
        return PLACEHOLDER
    ok, _ = _get(obj, path)
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


def _text(x: Any) -> str | None:
    """The text a dict key or a non-string scalar has as a secret: a string is
    itself, a number or a bool its `str(x)` (`True`/`False`); None has none."""
    if isinstance(x, str):
        return x
    if isinstance(x, (bool, int, float)):
        return str(x)
    return None


def scrub(x: Any, values: list[str]) -> Any:
    """Every occurrence of a secret value inside any string of `x` → placeholder —
    an error text that quotes the key must not carry it into a row or a log. A
    dict KEY, a NUMBER and a BOOL whose text EQUALS a secret value are the
    placeholder too — a reply keyed by the token it was given, a PIN echoed as an int
    (owner-approved 2026-10-03, audit S7). Equal, not contained: a key is
    structure, and `type` must survive a one-letter secret. Two keys that scrub
    alike keep the later value."""
    if not values:
        return x
    if isinstance(x, str):
        for v in sorted(values, key=len, reverse=True):
            if v in x:
                x = x.replace(v, PLACEHOLDER)
        return x
    if isinstance(x, dict):
        return {
            (PLACEHOLDER if _text(k) in values else k): scrub(v, values)
            for k, v in x.items()
        }
    if isinstance(x, list):
        return [scrub(v, values) for v in x]
    if _text(x) in values:
        return PLACEHOLDER
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
        ok, v = _get(res, kp)
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
