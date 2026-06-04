"""Subprocess wrapper for fantastic kernel daemons.

Spawns either:
- the Python kernel (`python/.venv/bin/fantastic`)
- the Swift kernel (`swift/.build/debug/fantastic`)

Both speak the same daemon CLI: no-args invocation in a workdir
loads `.fantastic/agents/<id>/agent.json` files + boots web (if
seeded) + blocks. The wrapper polls for HTTP readiness, exposes a
`call(target, verb, **args)` shortcut, and terminates cleanly on
context exit.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import httpx

from .ws import ws_call


@dataclass
class KernelProc:
    """Live fantastic kernel daemon — a local subprocess OR a container.

    Local: `proc` is the Popen handle. Container: `proc` is None and
    `container`/`engine` name the running container (podman/docker). The
    public surface (`wait_ready`/`call`/`terminate`) is identical either way,
    so tests are target-agnostic — the `Launcher` (see `launcher.py`) decides
    which kind it builds.
    """

    binary: Path
    workdir: Path
    port: int  # the port the web agent was seeded with — we know it ahead of spawn
    proc: subprocess.Popen | None = None
    label: str = ""  # human-readable tag for logs ("python" / "swift")
    # Container mode: the container name + the engine that runs it (else None).
    container: str | None = None
    engine: str | None = None

    # Captured output buffers (filled by wait_ready / shutdown).
    stdout: str = field(default="", init=False)
    stderr: str = field(default="", init=False)

    def _container_running(self) -> bool | None:
        """True/False if this is a container (running state); None if local."""
        if not self.container or not self.engine:
            return None
        r = subprocess.run(
            [self.engine, "inspect", "-f", "{{.State.Running}}", self.container],
            capture_output=True,
            text=True,
        )
        return r.stdout.strip() == "true"

    async def wait_ready(self, timeout: float = 60.0) -> None:
        """Poll HTTP `/` until the kernel's web server is bound.

        Raises `RuntimeError` if the kernel exits before becoming
        ready, or if the timeout elapses.

        The ceiling is generous because the Swift DEBUG binary cold-starts
        slowly (~20s measured; dyld + unoptimized boot), and the suite spawns
        several kernels under CPU contention — a tighter bound flakes on the
        first cold Swift spawn of a run. A Swift RELEASE build cold-starts in
        well under a second; prefer it for CI to keep this fast. The poll
        returns the instant the port binds, so this ceiling only caps the
        worst case, it doesn't slow the common path.
        """
        url = f"http://127.0.0.1:{self.port}/"
        deadline = time.monotonic() + timeout
        i = 0
        async with httpx.AsyncClient(timeout=1.0) as client:
            while time.monotonic() < deadline:
                i += 1
                # Has the daemon died early?  Local: poll the Popen. Container:
                # check the engine's running state (cheap, but only ~1/s to keep
                # the poll light) so a crashed container fails fast, not at timeout.
                if self.proc is not None and self.proc.poll() is not None:
                    rc = self.proc.returncode
                    self._drain_output()
                    raise RuntimeError(
                        f"{self.label} kernel exited early (rc={rc}) before binding "
                        f"port {self.port}\n--- stdout ---\n{self.stdout}\n"
                        f"--- stderr ---\n{self.stderr}"
                    )
                if self.container and i % 10 == 0 and self._container_running() is False:
                    self._drain_output()
                    raise RuntimeError(
                        f"{self.label} container '{self.container}' exited before binding "
                        f"port {self.port}\n--- logs ---\n{self.stderr}"
                    )
                try:
                    r = await client.get(url)
                    if r.status_code < 500:
                        return
                except httpx.TransportError:
                    # Any transport-level hiccup means "not ready yet, retry":
                    # ConnectError (port not bound), ReadTimeout, AND
                    # RemoteProtocolError — podman/docker forward the mapped
                    # port the instant the container starts, so a connection
                    # made before the app binds is accepted then closed without
                    # a response. `TransportError` is the base of all three.
                    pass
                await asyncio.sleep(0.1)
        # Timeout — surface what we know.
        self._drain_output()
        raise RuntimeError(
            f"{self.label} kernel didn't bind port {self.port} within {timeout}s\n"
            f"--- stdout ---\n{self.stdout}\n--- stderr ---\n{self.stderr}"
        )

    async def call(self, agent_id: str, verb: str, **args: Any) -> dict[str, Any]:
        """One-shot WS call against a local `agent_id` on this kernel.
        `verb` is the dispatched verb name; `args` are the verb's
        payload kwargs. Note the helper uses `agent_id=` not
        `target=` so callers can pass `target=...` as a verb arg
        (e.g. `bridge.forward(target=..., payload=...)`)."""
        return await ws_call(self.port, agent_id, verb, **args)

    def terminate(self) -> None:
        """Best-effort graceful shutdown.

        Container: `engine stop -t 8` (SIGTERM via tini → release lock.json,
        drain the server) then `rm -f` by name — never `kill()` the
        container-internal pid. Local: SIGTERM, then SIGKILL after grace.
        """
        if self.container:
            self._drain_output()  # capture logs before removal
            if self.engine:
                subprocess.run(
                    [self.engine, "stop", "-t", "8", self.container],
                    capture_output=True,
                    text=True,
                )
                subprocess.run(
                    [self.engine, "rm", "-f", self.container],
                    capture_output=True,
                    text=True,
                )
            return
        if self.proc is None or self.proc.poll() is not None:
            self._drain_output()
            return
        try:
            self.proc.terminate()
            self.proc.wait(timeout=3.0)
        except subprocess.TimeoutExpired:
            try:
                self.proc.kill()
                self.proc.wait(timeout=2.0)
            except Exception:
                pass
        except Exception:
            pass
        self._drain_output()

    def _drain_output(self) -> None:
        # Container: pull the engine logs into stderr (no Popen to communicate).
        if self.container:
            if not self.engine:
                return
            try:
                r = subprocess.run(
                    [self.engine, "logs", self.container],
                    capture_output=True,
                    text=True,
                    timeout=5.0,
                )
                self.stdout = r.stdout or ""
                self.stderr = r.stderr or r.stdout or ""
            except Exception:
                pass
            return
        if self.proc is None:
            return
        try:
            out, err = self.proc.communicate(timeout=0.5)
            self.stdout = (out or b"").decode("utf-8", errors="replace")
            self.stderr = (err or b"").decode("utf-8", errors="replace")
        except Exception:
            pass


def spawn(
    binary: Path,
    workdir: Path,
    port: int,
    *,
    label: str = "",
    extra_env: dict[str, str] | None = None,
) -> KernelProc:
    """Spawn a fantastic daemon in `workdir` with stdin detached so
    the daemon doesn't try to run a REPL. Returns the live proc;
    caller invokes `await proc.wait_ready(...)` before sending verbs.
    """
    env = os.environ.copy()
    if extra_env:
        env.update(extra_env)

    proc = subprocess.Popen(
        [str(binary)],
        cwd=str(workdir),
        env=env,
        stdin=subprocess.DEVNULL,  # ensure no tty → no REPL composition
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        # Put daemon in its own process group so terminate() doesn't
        # kill the pytest runner.
        preexec_fn=os.setpgrp if hasattr(os, "setpgrp") else None,
    )
    return KernelProc(binary=binary, workdir=workdir, port=port, proc=proc, label=label)
