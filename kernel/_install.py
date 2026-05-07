"""`fantastic install <project>` and `fantastic install-bundle <spec>`."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path


def cmd_install(project_dir: str, packages: list[str]) -> None:
    """Bootstrap a per-project detached venv at <project_dir>/.venv via
    `uv venv`, install requested packages with `uv pip install`, and
    point any `python_runtime` agent records under
    <project_dir>/.fantastic/agents/ at it (`venv: .venv`).

    The result: that project's python_runtime agents run code in their
    OWN environment (deps isolated from the kernel's substrate venv),
    while projects without `venv` set keep using sys.executable.

    No CLI to uv: shells out to `uv venv` + `uv pip install`. uv must
    be on PATH (you got here by running `uv run kernel.py`, so it is).
    """
    proj = Path(project_dir).expanduser().resolve()
    if not proj.is_dir():
        print(f"[install] not a directory: {proj}", file=sys.stderr)
        sys.exit(2)
    if shutil.which("uv") is None:
        print("[install] uv not on PATH; install uv first", file=sys.stderr)
        sys.exit(2)

    venv_dir = proj / ".venv"
    print(f"[install] uv venv {venv_dir}", file=sys.stderr)
    r = subprocess.run(
        ["uv", "venv", str(venv_dir)],
        cwd=proj,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        print(r.stderr, file=sys.stderr)
        sys.exit(r.returncode)
    print(r.stdout.strip() or "  venv created", file=sys.stderr)

    if packages:
        cmd = [
            "uv",
            "pip",
            "install",
            "--python",
            str(venv_dir / "bin" / "python"),
            *packages,
        ]
        print(f"[install] {' '.join(cmd)}", file=sys.stderr)
        r = subprocess.run(cmd, cwd=proj, capture_output=True, text=True)
        if r.returncode != 0:
            print(r.stderr, file=sys.stderr)
            sys.exit(r.returncode)
        print(r.stdout.strip() or "  packages installed", file=sys.stderr)

    # Update any python_runtime agent records in this project to use
    # the new venv (relative path so the project remains portable).
    agents_dir = proj / ".fantastic" / "agents"
    updated = 0
    if agents_dir.is_dir():
        for entry in sorted(agents_dir.iterdir()):
            agent_json = entry / "agent.json"
            if not agent_json.exists():
                continue
            try:
                rec = json.loads(agent_json.read_text())
            except (json.JSONDecodeError, OSError):
                continue
            if rec.get("handler_module") != "python_runtime.tools":
                continue
            if rec.get("venv") == ".venv":
                continue
            rec["venv"] = ".venv"
            agent_json.write_text(json.dumps(rec, indent=2))
            updated += 1
            print(f"  updated {rec['id']} → venv=.venv", file=sys.stderr)
    print(
        f"[install] done — venv={venv_dir} | python_runtime records updated: {updated}",
        file=sys.stderr,
    )


def cmd_install_bundle(spec: str, into: str | None) -> None:
    """`fantastic install-bundle <spec> [--into <project>]`

    Install a third-party fantastic bundle (or any pip-installable
    package that declares a `fantastic.bundles` entry point) into a
    Python environment so the kernel discovers it via
    `importlib.metadata.entry_points`.

    `spec` is anything `uv pip install` accepts:
        git+https://github.com/user/fantastic-bundle
        git+https://github.com/user/repo@v0.2.1
        git+https://github.com/user/repo@some-branch
        git+https://github.com/user/repo@a3f2b1c
        git+ssh://git@github.com/user/private-bundle
        any-pypi-package-name
        ./path/to/local/bundle

    Targets:
      no flag         — install into the kernel's own venv
                        (sys.executable's environment).
      --into <proj>   — install into <proj>/.venv. The project must
                        already have a venv; run `fantastic install
                        <proj>` first if it doesn't, or pass
                        `--create` to make one on the fly.

    After install, restart any running `kernel.py serve`s — entry
    points are scanned at process start, so a fresh kernel picks up
    the new bundle and lists it under `available_bundles` in the
    /_kernel/reflect primer.
    """
    if shutil.which("uv") is None:
        print("[install-bundle] uv not on PATH; install uv first", file=sys.stderr)
        sys.exit(2)

    if into:
        proj = Path(into).expanduser().resolve()
        venv = proj / ".venv"
        py = venv / "bin" / "python"
        if not py.exists():
            print(
                f"[install-bundle] no .venv at {venv}\n"
                f"  -> run: fantastic install {proj}     (creates .venv)\n"
                f"     then retry this command.",
                file=sys.stderr,
            )
            sys.exit(2)
        target = str(py)
        target_label = f"{proj}/.venv"
    else:
        target = sys.executable
        target_label = "kernel venv (sys.executable)"

    cmd = ["uv", "pip", "install", "--python", target, spec]
    print(f"[install-bundle] target = {target_label}", file=sys.stderr)
    print(f"[install-bundle] {' '.join(cmd)}", file=sys.stderr)
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr.strip(), file=sys.stderr)
        sys.exit(r.returncode)
    if r.stdout.strip():
        print(r.stdout.strip(), file=sys.stderr)
    print(
        "[install-bundle] done. Restart any running `fantastic serve` "
        "so the new entry point is discovered.",
        file=sys.stderr,
    )
