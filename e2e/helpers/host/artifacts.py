"""Build + PACKAGE each kernel exactly the way the release does, then run the
packaged thing.

Launching a kernel straight out of its build tree
(`src/lib/rust/target/debug/fantastic_kernel`,
`src/lib/python/main.py` beside its own `.venv`) would test a layout no user
ever has. Anything the release's packaging step drops — a bundle missing from
the tarball, an entry point that only resolves because the source tree happens
to be the cwd — is invisible until someone installs a release and it does not
boot.

The recipes are NOT duplicated here: each runtime is built by
`scripts/build/<runtime>.sh`, the SAME script the release is built with, called
with `--profile debug` instead of `--profile release`. Same command, same product
name, same working directory, same tarball layout — only the optimiser (and, for
ts, the minifier) is off. One recipe, so the two cannot drift.

The scenario PROBE switches are the only other divergence, and they are additive
and deliberately NOT in a release — a shipped kernel cannot play `dispenser`.
Each runtime declares its own in `PROBE_ENV`, except python's, which is the
launcher it RUNS (`e2e/probes/scenario_probes.py`: the probes registered, then
the packaged `main.py`'s own entry).

Artifacts land under `e2e/dist/<runtime>/`, rebuilt only when a source file is
newer than the tarball — a no-op run costs a stat walk, not a toolchain call.

BUILD OR USE BUILT — one switch, `E2E_ARTIFACTS`:
  * unset: each kernel is built here, `E2E_PROFILE` (`debug` unless set);
  * `E2E_ARTIFACTS=<dir>`: nothing is built — the tarballs in `<dir>` are staged
    as they are: `fantastic-<runtime>-<profile>.tar.gz`, what
    `scripts/build/<runtime>.sh --profile <profile>` wrote with `PROBE_ENV` set
    (CI's build job). A missing one fails loud; there is no building instead.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tarfile
import time
from pathlib import Path

from helpers import oslayer

#: Where staged artifacts live. Wipe-and-rebuild safe; nothing else reads it.
DIST = "e2e/dist"

#: The build profile of the kernels under test (built here, or the prebuilt ones
#: `E2E_ARTIFACTS` names).
PROFILE = os.environ.get("E2E_PROFILE", "debug")


def prebuilt_dir() -> Path | None:
    """The prebuilt artifacts' directory (`E2E_ARTIFACTS`), or None: build here."""
    d = os.environ.get("E2E_ARTIFACTS")
    return Path(d).resolve() if d else None

#: Source trees whose mtime decides whether a rebuild is needed. Deliberately
#: coarse: a stat walk is cheap next to a toolchain that has to be woken anyway.
WATCH = {
    "python": ["src/lib/python"],
    "rust": ["src/lib/rust/crates", "src/lib/rust/Cargo.toml"],
}

#: The scenario-probe switches. Additive and deliberately NOT in a release: a
#: shipped kernel cannot play `dispenser`. Python's is the launcher it RUNS
#: (see the driver's `cmd`), so it has no build switch here.
PROBE_ENV = {
    "rust": {"FANTASTIC_BUILD_FEATURES": "scenario-probes"},
}

_SKIP_DIRS = {
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".ruff_cache",
    "target",
    ".build",
    "node_modules",
    ".git",
    "dist",
}


def _newest_source(repo: Path, name: str) -> float:
    newest = 0.0
    for rel in WATCH[name]:
        root = repo / rel
        if root.is_file():
            newest = max(newest, root.stat().st_mtime)
            continue
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in _SKIP_DIRS]
            for f in filenames:
                if f.endswith((".pyc", ".o")):
                    continue
                try:
                    newest = max(newest, (Path(dirpath) / f).stat().st_mtime)
                except OSError:
                    pass
    return newest


def _run(cmd: list[str], cwd: Path, what: str, env: dict | None = None) -> None:
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env=env)
    if r.returncode != 0:
        raise SystemExit(f"{what} failed:\n{(r.stderr or r.stdout)[-1200:]}")


def _extract(tarball: Path, into: Path) -> None:
    if into.exists():
        shutil.rmtree(into)
    into.mkdir(parents=True)
    with tarfile.open(tarball) as tf:
        tf.extractall(into, filter="data")


def build_kernel(repo: Path, name: str, profile: str, out: Path) -> None:
    """`scripts/build/<name>.sh --profile <profile> --out <out>` with the probe
    switches on — the ONE recipe for a kernel under test, whether `stage` builds
    it here or `e2e/build.py` builds it for another machine."""
    _run(
        ["sh", str(repo / "scripts/build" / f"{name}.sh"), "--profile", profile, "--out", str(out)],
        repo,
        f"scripts/build/{name}.sh --profile {profile}",
        env=dict(os.environ, **PROBE_ENV.get(name, {})),
    )


def stage(repo: Path, name: str, spec: dict) -> dict:
    """Build (or take the prebuilt) → package → extract, and return `spec`
    pointing at the artifact.

    The build itself is `scripts/build/<name>.sh --profile <PROFILE>` — the SAME
    script the release is built with. e2e owns no second recipe, so the two
    cannot drift. With `E2E_ARTIFACTS` the tarball that script wrote elsewhere
    is taken as it is.

    Idempotent: if the staged artifact is newer than its source — the watched
    source files, or the prebuilt tarball — nothing runs.
    """
    dist = repo / DIST / name
    stamp = dist / ".staged"
    prebuilt = prebuilt_dir()
    if prebuilt is not None:
        tarball = prebuilt / f"fantastic-{name}-{PROFILE}.tar.gz"
        if not tarball.is_file():
            raise SystemExit(f"E2E_ARTIFACTS: no {tarball.name} in {prebuilt}")
        newest = tarball.stat().st_mtime
    else:
        tarball = repo / DIST / f"{name}-{PROFILE}.tar.gz"
        newest = _newest_source(repo, name)
    # The stamp names the tarball it was staged from: a debug staging is never
    # taken for a release run, nor a local build for a prebuilt one.
    if (
        stamp.exists()
        and stamp.read_text().splitlines()[:1] == [str(tarball)]
        and stamp.stat().st_mtime >= newest
    ):
        return _point_at(repo, name, spec, dist)

    if prebuilt is None:
        build_kernel(repo, name, PROFILE, tarball)
    _extract(tarball, dist)
    if name == "python":
        # The release ships SOURCE; installing it is `uv sync`. Syncing inside
        # the EXTRACTED copy is what makes a bundle the tarball forgot fail here
        # rather than on a user's machine.
        _run(["uv", "sync"], dist / "src/lib/python", "uv sync (packaged python kernel)")
    else:
        oslayer.exe(dist / "fantastic_kernel").chmod(0o755)
    stamp.write_text(f"{tarball}\n{time.time()}\n")
    return _point_at(repo, name, spec, dist)


def _point_at(repo: Path, name: str, spec: dict, dist: Path) -> dict:
    spec = dict(spec)
    if name == "python":
        # The PACKAGED venv runs the repo's probe launcher; its `import main`
        # resolves to the packaged `main.py` (the venv's editable install).
        py = dist / "src/lib/python"
        spec["cmd"] = [str(oslayer.venv_python(py / ".venv")), str(repo / "e2e/probes/scenario_probes.py")]
    else:
        spec["cmd"] = [str(oslayer.exe(dist / "fantastic_kernel"))]
    return spec
