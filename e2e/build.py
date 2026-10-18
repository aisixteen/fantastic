"""BUILD THE E2E'S ARTIFACTS — for a run elsewhere (`E2E_ARTIFACTS=<dir>`).

    cd src/lib/python && uv run python ../../../e2e/build.py --out <dir> \\
        [--profile release] [--target <triple>] [--exclude-runtime swift]

Writes into `<dir>` exactly what `run.py` takes when `E2E_ARTIFACTS=<dir>`:

  * `fantastic-<runtime>-<profile>.tar.gz` per kernel — the packaged kernel with
    its probe switches (`helpers/host/artifacts.build_kernel`, the same recipe a
    local run builds with);
  * `fantastic` — the CLI product, the binary a release ships
    (`scripts/build/cli.sh`, which also writes the shipping archive to `dist/`);
  * `screenshot` — the CLI's PTY rig, the shell's e2e driver (never shipped);
  * `fantastic-home-host` — the home kernel's test-only host (E24; never shipped);
  * `fantastic-tui-<features|none>` — the shell built with exactly the cargo
    `features` a tui scenario names (`e2e/tests/scenario/*/scenario.yaml`): a
    scenario that checks "not in this build" needs a build without that backend.

Nothing here is a second recipe: each piece is the script the local run and the
release already use, pointed at `<dir>`.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

E2E = Path(__file__).resolve().parent
sys.path.insert(0, str(E2E))

import yaml  # noqa: E402

from helpers import oslayer  # noqa: E402
from helpers.host.artifacts import build_kernel  # noqa: E402
from run import HOST_RUNTIMES  # noqa: E402

sys.path.insert(0, str(E2E / "runners"))
from e2e_build import variant_name  # noqa: E402

REPO = E2E.parent


def tui_feature_sets() -> set[tuple[str, ...]]:
    """Every cargo feature set a tui scenario names."""
    out = set()
    for f in sorted(E2E.glob("tests/scenario/*/scenario.yaml")):
        doc = yaml.safe_load(f.read_text(encoding="utf-8")) or {}
        if "tui" in (doc.get("runtimes") or []):
            out.add(tuple(sorted(doc.get("features") or [])))
    return out


def host_target() -> str:
    out = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    return next(line.split(": ", 1)[1] for line in out.splitlines() if line.startswith("host: "))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--profile", choices=["debug", "release"], default="release")
    ap.add_argument("--target", help="the CLI's rust target (default: the host's)")
    ap.add_argument("--exclude-runtime", action="append", default=[])
    a = ap.parse_args()
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    target = a.target or host_target()

    for rt in HOST_RUNTIMES:
        if rt in a.exclude_runtime:
            continue
        print(f"═══ kernel {rt} ({a.profile})", flush=True)
        build_kernel(REPO, rt, a.profile, out / f"fantastic-{rt}-{a.profile}.tar.gz")

    print(f"═══ the CLI product ({target}, {a.profile})", flush=True)
    sh = ["sh", str(REPO / "scripts/build/cli.sh"), "--profile", a.profile, "--target", target]
    subprocess.run(sh, cwd=REPO, check=True)
    built = REPO / "src" / "target" / target / a.profile
    shutil.copy2(oslayer.exe(built / "fantastic"), oslayer.exe(out / "fantastic"))

    print("═══ the PTY rig", flush=True)
    cargo = ["cargo", "build", "--locked", "-p", "fantastic-tui", "--example", "screenshot", "--target", target]
    if a.profile == "release":
        cargo.append("--release")
    subprocess.run(cargo, cwd=REPO / "src", check=True)
    shutil.copy2(oslayer.exe(built / "examples" / "screenshot"), oslayer.exe(out / "screenshot"))

    print("═══ the home host", flush=True)
    cargo = ["cargo", "build", "--locked", "-p", "fantastic-home-kernel", "--features", "host",
             "--bin", "fantastic-home-host", "--target", target]
    if a.profile == "release":
        cargo.append("--release")
    subprocess.run(cargo, cwd=REPO / "src", check=True)
    shutil.copy2(oslayer.exe(built / "fantastic-home-host"), oslayer.exe(out / "fantastic-home-host"))

    for features in sorted(tui_feature_sets()):
        name = variant_name(list(features))
        print(f"═══ {name}", flush=True)
        cargo = ["cargo", "build", "--locked", "-p", "fantastic-tui", "--target", target]
        if features:
            cargo += ["--features", ",".join(features)]
        if a.profile == "release":
            cargo.append("--release")
        subprocess.run(cargo, cwd=REPO / "src", check=True)
        shutil.copy2(oslayer.exe(built / "fantastic"), oslayer.exe(out / name))

    print(f"═══ e2e artifacts in {out}:", *sorted(p.name for p in out.iterdir()), flush=True)
    return 0


if __name__ == "__main__":
    oslayer.ensure_utf8()
    raise SystemExit(main())
