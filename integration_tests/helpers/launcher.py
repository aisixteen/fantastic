"""Launcher abstraction — run the fantastic kernel either as a LOCAL binary
subprocess or inside the universal CONTAINER image (podman/docker), selected by
the `FANTASTIC_TARGET` env var (`local` default, or `container`).

Both launchers expose the SAME two operations, so the integration suite is
target-agnostic — seeding helpers and the spawn fixtures call these, never a
raw binary path:

  - `cli(workdir, argv, *, timeout)`  → a one-shot CLI invocation (seeding /
        reflect); returns a `subprocess.CompletedProcess` (text mode).
  - `start_daemon(workdir, port, ...)` → a live `KernelProc` reachable at
        `127.0.0.1:<port>`.

LOCAL is byte-identical to the historical path (`subprocess.run([binary, …],
cwd=workdir)` + the original `spawn()`), so the default test run is unchanged.

CONTAINER mounts `workdir` at `/work` and maps `127.0.0.1:<port>:<port>`.
Crucially, the seeding one-shots run INSIDE the container too (entrypoint
bypassed via `--entrypoint <bin>`): a rootless container runs as uid 1000, and
a `.fantastic/` dir seeded on the host would be unwritable by it — so every
record is created by the container's own uid, start to finish.

Swift has no Linux container (its HTTP server is Network.framework-only), so
only `python` and `rust` have a container launcher; swift fixtures skip cleanly
under `FANTASTIC_TARGET=container`.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from .kernel_proc import KernelProc, spawn

# Container-internal binary paths — must match container/entrypoint.sh defaults
# (FANTASTIC_PY / FANTASTIC_RUST).
_CONTAINER_BIN = {
    "python": "/opt/fantastic/venv/bin/fantastic",
    "rust": "/opt/fantastic/bin/fantastic-rust",
}


class LocalLauncher:
    """Run the kernel from a locally-built binary (the historical path)."""

    kind = "local"

    def __init__(self, binary: Path) -> None:
        self.binary = Path(binary)
        self.runtime = ""

    def cli(
        self, workdir: Path, argv: list[str], *, timeout: float = 15.0
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(self.binary), *argv],
            cwd=str(workdir),
            capture_output=True,
            text=True,
            timeout=timeout,
        )

    def start_daemon(
        self,
        workdir: Path,
        port: int,
        *,
        label: str = "",
        extra_env: dict[str, str] | None = None,
    ) -> KernelProc:
        return spawn(self.binary, Path(workdir), port, label=label, extra_env=extra_env)


class ContainerLauncher:
    """Run the kernel inside the universal container image via podman/docker."""

    kind = "container"

    def __init__(self, image: str, runtime: str, engine: str) -> None:
        if runtime not in _CONTAINER_BIN:
            raise ValueError(f"no container launcher for runtime {runtime!r} (python|rust only)")
        self.image = image
        self.runtime = runtime
        self.engine = engine
        # `.binary` mirrors LocalLauncher for any caller that wants the path.
        self.binary = Path(_CONTAINER_BIN[runtime])

    def cli(
        self, workdir: Path, argv: list[str], *, timeout: float = 15.0
    ) -> subprocess.CompletedProcess:
        # One-shot in the container: bypass the dispatch entrypoint and run the
        # binary directly against the bind-mounted /work. Floor the timeout —
        # a cold `podman run` adds startup overhead a local exec doesn't.
        eff = timeout if timeout >= 60.0 else 60.0
        # podman/docker treat a RELATIVE -v source as a named volume, not a bind
        # mount — always resolve to an absolute host path.
        wd = Path(workdir).resolve()
        cmd = [
            self.engine,
            "run",
            "--rm",
            "-v",
            f"{wd}:/work",
            "-w",
            "/work",
            "--entrypoint",
            str(self.binary),
            self.image,
            *argv,
        ]
        return subprocess.run(cmd, capture_output=True, text=True, timeout=eff)

    def start_daemon(
        self,
        workdir: Path,
        port: int,
        *,
        label: str = "",
        extra_env: dict[str, str] | None = None,
    ) -> KernelProc:
        name = f"ftit-{self.runtime}-{port}-{os.getpid()}"
        wd = Path(workdir).resolve()  # absolute → real bind mount (not a named volume)
        # Clear any stale container with this name (best effort).
        subprocess.run([self.engine, "rm", "-f", name], capture_output=True, text=True)
        # Integration daemons run with the head OFF: tests exercise the call
        # surface, not the descriptive page, and with head off `/` is the
        # DYNAMIC agent index (it dispatches a kernel reflect to render) — so a
        # 200 on `/` is a real "kernel is up + serving" signal for wait_ready,
        # not a static file served the instant uvicorn binds.
        env: dict[str, str] = {"FANTASTIC_HEAD": "off"}
        env.update(extra_env or {})
        cmd = [
            self.engine,
            "run",
            "-d",
            "--name",
            name,
            "-p",
            f"127.0.0.1:{port}:{port}",
            "-v",
            f"{wd}:/work",
            "-e",
            f"FANTASTIC_RUNTIME={self.runtime}",
            "-e",
            f"FANTASTIC_PORT={port}",
        ]
        for k, v in env.items():
            cmd += ["-e", f"{k}={v}"]
        cmd.append(self.image)
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError(
                f"container start failed ({self.runtime} on :{port}): {r.stderr or r.stdout}"
            )
        return KernelProc(
            binary=self.binary,
            workdir=Path(workdir),
            port=port,
            proc=None,
            label=label or self.runtime,
            container=name,
            engine=self.engine,
        )


# Either kind of launcher.
Launcher = LocalLauncher | ContainerLauncher


def as_launcher(x: Launcher | Path | str) -> Launcher:
    """Coerce a binary Path/str into a `LocalLauncher`; pass launchers through.

    Lets seeding helpers accept either a launcher (the new way) or a raw binary
    path (back-compat), so call sites that still pass a `Path` keep working.
    """
    if isinstance(x, (LocalLauncher, ContainerLauncher)):
        return x
    return LocalLauncher(Path(x))


def resolve_engine() -> str | None:
    """First available container engine (podman preferred), or None."""
    from shutil import which

    for engine in ("podman", "docker"):
        if which(engine) is not None:
            return engine
    return None
