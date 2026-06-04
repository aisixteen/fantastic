"""Substrate CLI dispatch — argv -> action (one-shot subcommands + default)."""

from __future__ import annotations

import json
import sys
from typing import Any

from kernel._lock import FantasticLock, _pid_alive, _read_lock

from kernel.modes.help import _print_help
from kernel.modes.longrun import _default
from kernel.modes.parse import _parse_kv


async def dispatch_argv(kernel, argv: list[str]) -> Any:
    """argv → action. One-shot subcommands routed by first word;
    everything else falls into the default long-running mode."""
    if argv and argv[0] in ("-h", "--help", "help"):
        _print_help()
        return None

    # One-shot subcommands.
    if argv:
        cmd = argv[0]
        if cmd == "reflect":
            return await reflect(kernel, argv[1:])
        # Generic <id> <verb> [k=v ...].
        if len(argv) >= 2:
            return await call(kernel, cmd, argv[1:])
        # Single positional that isn't a subcommand and has no verb.
        print(
            "usage: fantastic <id> <verb> [k=v ...]\n"
            "       fantastic reflect [<id>]\n"
            "       fantastic                              (interactive REPL when stdin is a tty;\n"
            "                                               daemon when a web agent is persisted)",
            file=sys.stderr,
        )
        sys.exit(2)

    # Default long-running mode.
    return await _default(kernel)


async def call(kernel, target: str, rest: list[str]) -> None:
    """One-shot RPC. Always acquires the PID lock + dispatches in-
    process. Fails if another fantastic owns the dir (use kernel_bridge
    to forward to it over WS instead).

    The kernel does NOT speak HTTP for substrate calls. Browsers /
    cross-kernel callers go through the WS frame protocol exposed by
    `web` (and reused by `kernel_bridge` for in-process bridges)."""
    if not rest:
        print(
            "usage: fantastic <target_id> <verb> [k=v ...]",
            file=sys.stderr,
        )
        sys.exit(2)
    verb, *kv_args = rest
    body = {"type": verb, **_parse_kv(kv_args)}
    cur = _read_lock()
    if cur and isinstance(cur.get("pid"), int) and _pid_alive(cur["pid"]):
        print(
            f"[call] another fantastic owns this dir (pid={cur.get('pid')}). "
            "Stop it, or forward your call over the WS bridge "
            "(see kernel_bridge bundle).",
            file=sys.stderr,
        )
        sys.exit(1)
    with FantasticLock():
        # Boot the root loader so this call's mutations (create/update/
        # delete) subscribe + persist. The debounce task won't tick
        # before we exit, so drain it synchronously after the call.
        if kernel.root is not None:
            await kernel.send(kernel.root.id, {"type": "boot"})
        reply = await kernel.send(target, body)
        print(json.dumps(reply, indent=2, default=str))
        loop = getattr(kernel.root, "_fs_flush_loop", None)
        if loop is not None:
            loop.flush()
    return None


async def reflect(kernel, rest: list[str]) -> None:
    """Sugar for `<target> reflect [k=v ...]`. Default target:
    'kernel' (the tree root). The first token is the target unless it's
    a `k=v` pair, so `fantastic reflect readme=true` reflects the root
    with the flag, and `fantastic reflect <id> tree=ids` reflects that
    agent. Compose the reply with `tree=all|ids|none` (default all),
    `bundles=all|ids|none` (default none), `readme=true` (legacy
    `return_readme` still honored).

    Read-only — dispatched in-process WITHOUT the PID lock, so it
    works whether or not a daemon owns the dir. A one-shot kernel sees
    disk-backed records (the agent tree, readmes); live process-memory
    state belongs to the daemon and isn't reflected here."""
    if rest and "=" not in rest[0]:
        target, kv = rest[0], rest[1:]
    else:
        target, kv = "kernel", rest
    body = {"type": "reflect", **_parse_kv(kv)}
    reply = await kernel.send(target, body)
    print(json.dumps(reply, indent=2, default=str))
    return None
