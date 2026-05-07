"""REPL: parse `add`/`delete`/`@<id> ...` lines, dispatch via Kernel."""

from __future__ import annotations

import asyncio
import json
import shlex
from typing import Any

from kernel._bundles import _find_bundle_module, _seed_singletons
from kernel._kernel import Kernel


def _coerce(v: str) -> Any:
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
    if (v.startswith("{") and v.endswith("}")) or (
        v.startswith("[") and v.endswith("]")
    ):
        try:
            return json.loads(v)
        except json.JSONDecodeError:
            pass
    return v


def _parse_at(line: str) -> tuple[str, dict]:
    """Parse `@<id> <text>` or `@<id> <verb> [k=v ...]` → (id, payload).

    Rules:
      `@<id>`                          → {type:"send", text:""}
      `@<id> <single_word>`            → {type:<word>}              (no-arg verb)
      `@<id> <verb> k=v k=v ...`       → {type:<verb>, **kv}        (verb call)
      `@<id> <multi word text>`        → {type:"send", text:"..."}  (free text)
    """
    rest = line[1:].strip()
    if not rest:
        return "", {}
    parts = shlex.split(rest)
    target = parts[0]
    args = parts[1:]
    if not args:
        return target, {"type": "send", "text": ""}
    # Single token, no `=` → bare verb call
    if len(args) == 1 and "=" not in args[0]:
        return target, {"type": args[0]}
    # Any kv present → verb + kv (first arg is verb)
    if any("=" in a for a in args):
        verb = args[0] if "=" not in args[0] else "send"
        kv_args = args[1:] if "=" not in args[0] else args
        kv = {k: _coerce(v) for k, v in (a.split("=", 1) for a in kv_args if "=" in a)}
        return target, {"type": verb, **kv}
    # Multiple words, no kv → free text send
    text = rest[len(target) :].strip()
    return target, {"type": "send", "text": text}


async def _print_result(result: Any) -> None:
    if result is None:
        return
    if isinstance(result, dict):
        if "error" in result:
            print(f"  error: {result['error']}")
            return
        # short hand: created agent
        if "id" in result and "handler_module" in result:
            print(f"  created {result['id']}")
            return
    try:
        print(f"  {json.dumps(result, indent=2, default=str)}")
    except (TypeError, ValueError):
        print(f"  {result}")


async def _read_line(prompt: str) -> str:
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, input, prompt)


async def cmd_repl() -> None:
    k = Kernel()
    await _seed_singletons(k)
    while True:
        try:
            line = await _read_line("fantastic> ")
        except (EOFError, KeyboardInterrupt):
            print()
            break
        line = line.strip()
        if not line:
            continue
        if line in ("exit", "quit"):
            break
        if line == "list":
            for a in k.list():
                tag = " (singleton)" if a.get("singleton") else ""
                print(f"  {a['id']}{tag}  →  {a['handler_module']}")
            continue
        if line.startswith("add "):
            parts = shlex.split(line[4:])
            if not parts:
                print("  usage: add <bundle> [k=v ...]")
                continue
            bundle = parts[0]
            handler_module = _find_bundle_module(bundle)
            if handler_module is None:
                print(f"  unknown bundle {bundle!r}")
                continue
            meta: dict[str, Any] = {}
            for p in parts[1:]:
                if "=" in p:
                    k2, v = p.split("=", 1)
                    meta[k2] = _coerce(v)
            r = await k.send(
                "core",
                {"type": "create_agent", "handler_module": handler_module, **meta},
            )
            await _print_result(r)
            continue
        if line.startswith("delete "):
            r = await k.send("core", {"type": "delete_agent", "id": line[7:].strip()})
            await _print_result(r)
            continue
        if line.startswith("@"):
            target, payload = _parse_at(line)
            if not target:
                print("  usage: @<id> <text> | @<id> <verb> k=v ...")
                continue
            r = await k.send(target, payload)
            await _print_result(r)
            continue
        print(f"  unknown command: {line!r}  (try: list, add <bundle>, @<id> ...)")
