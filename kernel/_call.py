"""One-shot RPC: prefer live `serve` over HTTP, else in-process kernel."""

from __future__ import annotations

import json
import sys
from typing import Any

from kernel._bundles import _seed_singletons
from kernel._kernel import Kernel
from kernel._lock import _pid_alive, _read_lock


async def cmd_call(target: str, verb: str, kv: dict[str, Any]) -> None:
    """One-shot RPC: prefer the live `serve` if one is running so callers
    see live process-memory state (PTYs, uvicorn, in-flight tasks).

    Resolution order:
      1. `.fantastic/lock.json` exists with an alive pid + port → POST
         `http://localhost:<port>/<target>/call` with body `{type, ...kv}`.
      2. Otherwise, spawn an in-process Kernel (no boot fanout) and
         dispatch directly. Stateful agents will report empty state in
         this path — that's expected and documented.
    """
    payload = {"type": verb, **kv}
    cur = _read_lock()
    if cur and isinstance(cur.get("pid"), int) and _pid_alive(cur["pid"]):
        port = cur.get("port")
        if isinstance(port, int):
            url = f"http://localhost:{port}/{target}/call"
            try:
                import urllib.error
                import urllib.request

                req = urllib.request.Request(
                    url,
                    data=json.dumps(payload).encode("utf-8"),
                    headers={"content-type": "application/json"},
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=30) as resp:
                    body = resp.read().decode("utf-8")
                # Pretty-print the live serve's reply.
                try:
                    print(json.dumps(json.loads(body), indent=2, default=str))
                except json.JSONDecodeError:
                    print(body)
                return
            except urllib.error.URLError as e:
                # Lock exists + pid alive but HTTP failed (port shifted,
                # uvicorn not yet bound, etc.). Fall through to in-process.
                print(
                    f"  [call] live serve at :{port} unreachable ({e}); "
                    "falling back to in-process kernel.",
                    file=sys.stderr,
                )
    k = Kernel()
    await _seed_singletons(k, boot_all=False)
    reply = await k.send(target, payload)
    print(json.dumps(reply, indent=2, default=str))


async def cmd_reflect(target: str = "kernel") -> None:
    await cmd_call(target, "reflect", {})
