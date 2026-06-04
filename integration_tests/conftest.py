"""Shared pytest fixtures for fantastic integration tests.

Provides:
- `free_port()`            grab an OS-assigned ephemeral port
- `python_binary`           path to the canonical Python kernel binary
- `swift_binary`            path to the Swift kernel binary
- `parity_tmp(name)`        per-test scratch dir under ./tmp/
- `python_kernel(workdir, port)`    spawn a Python daemon
- `swift_kernel(workdir, port)`     spawn a Swift daemon

Each spawn fixture returns a `KernelProc` from
`helpers.kernel_proc`. Tests are responsible for sequencing
(workdir seeding → spawn → wait_ready → verb dispatches → cleanup).
The proc is auto-terminated on test teardown.
"""

from __future__ import annotations

import socket
import sys
import uuid
from pathlib import Path
from typing import Callable

import pytest

# Make sibling `helpers/` importable regardless of where pytest runs.
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from helpers.kernel_proc import KernelProc  # noqa: E402
from helpers.launcher import (  # noqa: E402
    ContainerLauncher,
    LocalLauncher,
    resolve_engine,
)

# Repo root (one level up from integration_tests/).
_REPO_ROOT = _HERE.parent

# Target selection — `local` (default) runs locally-built binaries; `container`
# runs the universal image (python/rust only) so the SAME tests validate the
# shipped container. `FANTASTIC_IMAGE` overrides the image tag.
_TARGET = __import__("os").environ.get("FANTASTIC_TARGET", "local").strip().lower()
_IMAGE = __import__("os").environ.get("FANTASTIC_IMAGE", "fantastic:latest")


def _container_launcher_or_skip(runtime: str) -> ContainerLauncher:
    """Build a ContainerLauncher for `runtime`, or skip if the engine/image
    isn't available (keeps `FANTASTIC_TARGET=container` runs self-gating)."""
    import subprocess

    engine = resolve_engine()
    if engine is None:
        pytest.skip("FANTASTIC_TARGET=container but no podman/docker found")
    present = subprocess.run([engine, "image", "inspect", _IMAGE], capture_output=True, text=True)
    if present.returncode != 0:
        pytest.skip(
            f"FANTASTIC_TARGET=container but image {_IMAGE!r} not built "
            f"(run `sh container/build.sh`)"
        )
    return ContainerLauncher(_IMAGE, runtime, engine)


@pytest.fixture(scope="session")
def python_binary():
    """A launcher for the Python kernel.

    `FANTASTIC_TARGET=local` (default) → a `LocalLauncher` over the built venv
    binary (skips if not built). `FANTASTIC_TARGET=container` → a
    `ContainerLauncher` running the universal image's python runtime. The name
    is kept (`python_binary`) so no test signature changes; seeding + the
    spawn fixtures use the launcher's `cli` / `start_daemon`.
    """
    if _TARGET == "container":
        return _container_launcher_or_skip("python")
    candidate = _REPO_ROOT / "python" / ".venv" / "bin" / "fantastic"
    if not candidate.exists():
        pytest.skip(f"python kernel binary not built: {candidate} (run `cd python && uv sync`)")
    return LocalLauncher(candidate)


@pytest.fixture(scope="session")
def swift_binary():
    """A launcher for the Swift kernel — LOCAL ONLY.

    Swift's HTTP server is Network.framework-only, so there is no Linux
    container for it; under `FANTASTIC_TARGET=container` swift tests skip.
    Searches the canonical `swift build` output plus the isolated
    dev-iteration path and picks whichever is **newest by mtime** (avoids
    silently running a stale binary that masked missing verbs before).
    """
    if _TARGET == "container":
        pytest.skip(
            "swift has no Linux container (Network.framework HTTP); skipped under container target"
        )
    candidates = [
        _REPO_ROOT / "swift" / ".build" / "debug" / "fantastic",
        Path("/tmp/swift-fm-build/debug/fantastic"),
    ]
    existing = [c for c in candidates if c.exists()]
    if not existing:
        pytest.skip(
            f"swift kernel binary not built: tried {[str(c) for c in candidates]} "
            f"(run `cd swift && swift build`)"
        )
    return LocalLauncher(max(existing, key=lambda p: p.stat().st_mtime))


@pytest.fixture(scope="session")
def rust_binary():
    """A launcher for the Rust kernel — `LocalLauncher` (built binary, newest
    of release/debug by mtime) or a `ContainerLauncher` under the container
    target."""
    if _TARGET == "container":
        return _container_launcher_or_skip("rust")
    candidates = [
        _REPO_ROOT / "rust" / "target" / "release" / "fantastic",
        _REPO_ROOT / "rust" / "target" / "debug" / "fantastic",
    ]
    existing = [c for c in candidates if c.exists()]
    if not existing:
        pytest.skip(
            f"rust kernel binary not built: tried {[str(c) for c in candidates]} "
            f"(run `cd rust && cargo build`)"
        )
    return LocalLauncher(max(existing, key=lambda p: p.stat().st_mtime))


@pytest.fixture
def free_port() -> Callable[[], int]:
    """Returns a callable that grabs a fresh OS-assigned port each
    invocation. The socket is closed before returning so the port is
    available to the daemon by the time spawn() happens — there's a
    small race window between close + bind but it's negligible in
    practice on localhost.
    """

    def _get() -> int:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]
        finally:
            s.close()

    return _get


@pytest.fixture
def parity_tmp(request) -> Callable[[str], Path]:
    """Returns a callable that mints per-test scratch directories
    under `./tmp/`. The test owns the lifetime — preserved on
    failure for inspection; cleaned on success unless
    `INTEGRATION_KEEP_TMP=1` is set.
    """
    keep = "INTEGRATION_KEEP_TMP" in __import__("os").environ
    created: list[Path] = []

    def _mint(name: str) -> Path:
        # `<test_name>_<short_uuid>` under integration_tests/tmp/
        base = _HERE / "tmp" / f"{name}_{uuid.uuid4().hex[:8]}"
        base.mkdir(parents=True, exist_ok=True)
        created.append(base)
        return base

    yield _mint

    # Teardown — preserve on failure for inspection.
    if request.node.rep_call.failed if hasattr(request.node, "rep_call") else False:
        return
    if keep:
        return
    import shutil

    for d in created:
        shutil.rmtree(d, ignore_errors=True)


# Hook for `parity_tmp` to detect failures (preserve workdirs on failure).
@pytest.hookimpl(tryfirst=True, hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    rep = outcome.get_result()
    setattr(item, f"rep_{rep.when}", rep)


@pytest.fixture
async def python_kernel(python_binary):
    """Context-managed factory: yields `spawn_one(workdir, port)`.

    Caller does:
        async def test(python_kernel, parity_tmp, free_port):
            workdir = parity_tmp("mytest") / "A"
            workdir.mkdir(parents=True, exist_ok=True)
            seed_core(workdir); seed_web(workdir, port); seed_web_ws(workdir)
            port = free_port()
            kernel = await python_kernel(workdir, port)
            ... use kernel ...
    All spawns are auto-terminated on fixture teardown.
    """
    spawned: list[KernelProc] = []

    async def _spawn(workdir: Path, port: int) -> KernelProc:
        kp = python_binary.start_daemon(workdir, port, label="python")
        spawned.append(kp)
        await kp.wait_ready()
        return kp

    yield _spawn

    for kp in spawned:
        kp.terminate()


@pytest.fixture
async def swift_kernel(swift_binary):
    """Mirror of `python_kernel` but for the Swift daemon."""
    spawned: list[KernelProc] = []

    async def _spawn(workdir: Path, port: int) -> KernelProc:
        kp = swift_binary.start_daemon(workdir, port, label="swift")
        spawned.append(kp)
        await kp.wait_ready()
        return kp

    yield _spawn

    for kp in spawned:
        kp.terminate()


@pytest.fixture
async def rust_kernel(rust_binary):
    """Mirror of `python_kernel` but for the Rust daemon."""
    spawned: list[KernelProc] = []

    async def _spawn(workdir: Path, port: int) -> KernelProc:
        kp = rust_binary.start_daemon(workdir, port, label="rust")
        spawned.append(kp)
        await kp.wait_ready()
        return kp

    yield _spawn

    for kp in spawned:
        kp.terminate()
