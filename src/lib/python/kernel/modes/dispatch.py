"""Substrate CLI dispatch — argv -> action (one-shot subcommands + default)."""

from __future__ import annotations

import json
import sys
from typing import Any

from kernel._lock import LockHeld, acquire_lock, release_lock

from kernel.modes.help import _print_help
from kernel.modes.longrun import _default, take_seed
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
        if cmd == "init":
            return await init(kernel, argv[1:])
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
    """One-shot RPC. Always takes the dir's lock (the daemon's flock) +
    dispatches in-process. Fails if another fantastic holds it (use
    ws_bridge to forward to it over WS instead).

    The kernel does NOT speak HTTP for substrate calls. Browsers /
    cross-kernel callers go through the WS frame protocol exposed by
    `web` (and reused by `ws_bridge` for in-process bridges)."""
    if not rest:
        print(
            "usage: fantastic <target_id> <verb> [k=v ...]",
            file=sys.stderr,
        )
        sys.exit(2)
    verb, *kv_args = rest
    body = {"type": verb, **_parse_kv(kv_args)}
    _lock_or_exit(
        "[call] another fantastic owns this dir (pid={pid}). "
        "Stop it, or forward your call over the WS bridge "
        "(see ws_bridge bundle)."
    )
    take_seed(kernel)
    try:
        # Boot the root loader so this call's mutations (create/update/
        # delete) subscribe + persist. The debounce task won't tick
        # before we exit, so drain it synchronously after the call.
        if kernel.root is not None:
            (
                await kernel.send(
                    [{"target": kernel.root.id, "payload": {"type": "boot"}}]
                )
            )[0]["result"]
        reply = (await kernel.send([{"target": target, "payload": body}]))[0]["result"]
        print(json.dumps(reply, indent=2, default=str))
        loop = getattr(kernel.root, "_fs_flush_loop", None)
        if loop is not None:
            await loop.aflush()
    finally:
        release_lock()
    return None


def _lock_or_exit(refusal: str) -> None:
    """A one-shot takes THE DAEMON'S OWN LOCK — the flock `acquire_lock` takes —
    and is refused only when a live process holds it: `refusal` (formatted with
    the pid its file names) on stderr, exit 1. There is no pid pre-check: a lock
    file naming a pid that is alive but holds no flock (a reused pid) is stale,
    and taken over."""
    try:
        acquire_lock()
    except LockHeld as e:
        print(refusal.format(pid=e.pid), file=sys.stderr)
        sys.exit(1)


async def init(kernel, rest: list[str]) -> None:
    """`fantastic init <loader_bundle> <manifest>` — seed a declarative loader.

    Creates a loader agent (e.g. `static_loader`) under the root with a
    `manifest` field pointing at the config file, then boots it — the loader
    reads the manifest and hydrates the declared roster (as operator). Example:

        fantastic init static_loader ./fantastic.yaml

    Takes the dir's lock (it mutates the tree) and persists through the root
    loader, exactly like a one-shot `create_agent`. The loader agent's id is the
    bundle name, so a second `init` is idempotent (create refuses a dup id)."""
    if len(rest) < 2:
        print(
            "usage: fantastic init <loader_bundle> <manifest>\n"
            "   e.g. fantastic init static_loader ./fantastic.yaml",
            file=sys.stderr,
        )
        sys.exit(2)
    loader_name, manifest = rest[0], rest[1]
    _lock_or_exit("[init] another fantastic owns this dir (pid={pid}). Stop it first.")
    take_seed(kernel)
    try:
        if kernel.root is not None:
            # Boot the root loader so the new records subscribe + persist.
            (
                await kernel.send(
                    [{"target": kernel.root.id, "payload": {"type": "boot"}}]
                )
            )[0]["result"]
        # create_agent boots the loader child → its own boot reads the manifest
        # and hydrates the roster. The kernel never parses YAML; the loader does.
        reply = (
            await kernel.send(
                [
                    {
                        "target": kernel.root.id,
                        "payload": {
                            "type": "create_agent",
                            # A top-level loader — the root owns it.
                            "parent_id": kernel.root.id,
                            "handler_module": f"{loader_name}.tools",
                            "id": loader_name,
                            "manifest": manifest,
                        },
                    }
                ]
            )
        )[0]["result"]
        print(json.dumps(reply, indent=2, default=str))
        loop = getattr(kernel.root, "_fs_flush_loop", None)
        if loop is not None:
            await loop.aflush()
    finally:
        release_lock()
    return None


async def reflect(kernel, rest: list[str]) -> None:
    """Sugar for `<target> reflect [k=v ...]`. Default target:
    'kernel' (the tree root). The first token is the target unless it's
    a `k=v` pair, so `fantastic reflect readme=true` reflects the root
    with the flag, and `fantastic reflect <id> tree=ids` reflects that
    agent. Compose the reply with `tree=all|ids|none` (default all),
    `bundles=all|ids|none` (default none), `readme=true`.

    Read-only — dispatched in-process WITHOUT the PID lock, so it
    works whether or not a daemon owns the dir. A one-shot kernel sees
    disk-backed records (the agent tree, readmes); live process-memory
    state belongs to the daemon and isn't reflected here."""
    if rest and "=" not in rest[0]:
        target, kv = rest[0], rest[1:]
    else:
        target, kv = "kernel", rest
    body = {"type": "reflect", **_parse_kv(kv)}
    reply = (await kernel.send([{"target": target, "payload": body}]))[0]["result"]
    print(json.dumps(reply, indent=2, default=str))
    return None
