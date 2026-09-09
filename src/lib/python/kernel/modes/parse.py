"""CLI value parsing helpers (k=v coercion, REPL @-line parsing)."""

from __future__ import annotations

import json
import shlex
from typing import Any


def coerce_scalar(v: str) -> Any:
    """ONE scalar rule for text a caller typed — the CLI's `k=v` and the model's
    `<k>v</k>` alike: `true`/`false` → bool, an integer → int, a decimal → float,
    anything else → the string as written. No JSON, no quoting: a value that has to
    stay text is text, and a verb that needs a type gets the typed value or refuses."""
    if v.lower() in ("true", "false"):
        return v.lower() == "true"
    try:
        return int(v)
    except ValueError:
        pass
    try:
        return float(v)
    except ValueError:
        pass
    return v


def _coerce(v: str) -> Any:
    """CLI `k=v`: the shared scalar rule, plus a JSON object/list for the OPERATOR
    (out of the model↔harness path, which carries no JSON at all)."""
    typed = coerce_scalar(v)
    if not isinstance(typed, str):
        return typed
    if (v.startswith("{") and v.endswith("}")) or (
        v.startswith("[") and v.endswith("]")
    ):
        try:
            return json.loads(v)
        except json.JSONDecodeError:
            pass
    return v


def _parse_kv(args: list[str]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for a in args:
        if "=" not in a:
            continue
        k, v = a.split("=", 1)
        out[k] = _coerce(v)
    return out


def _parse_at(line: str) -> tuple[str, dict]:
    """Parse `@<id> <text>` or `@<id> <verb> [k=v ...]` → (id, payload)."""
    rest = line[1:].strip()
    if not rest:
        return "", {}
    parts = shlex.split(rest)
    target = parts[0]
    args = parts[1:]
    if not args:
        return target, {"type": "send", "text": ""}
    if len(args) == 1 and "=" not in args[0]:
        return target, {"type": args[0]}
    if any("=" in a for a in args):
        verb = args[0] if "=" not in args[0] else "send"
        kv_args = args[1:] if "=" not in args[0] else args
        kv = {k: _coerce(v) for k, v in (a.split("=", 1) for a in kv_args if "=" in a)}
        return target, {"type": verb, **kv}
    text = rest[len(target) :].strip()
    return target, {"type": "send", "text": text}
