#!/usr/bin/env python
"""The acceptance harness: `preflight | stage | capture`.

    PY=$R/src/lib/python/.venv/bin/python
    $PY $R/e2e/parity/run.py preflight --runtimes python,rust,swift
    $PY $R/e2e/parity/run.py stage --runtime rust          # the ONLY path that builds
    $PY $R/e2e/parity/run.py capture --runtime rust --families all --out <dir>

THREE SUBCOMMANDS, ONE RULE EACH:

  `preflight` FAILS LOUD AND NEVER REPAIRS. Rebuilding behind the operator's
  back, or running whatever happens to be on disk — both make a capture a statement about an
  unknown binary. This checks and names the command to run; it fixes nothing.

  `stage` is the only thing allowed to build, and it is explicit — an acceptance
  capture that rebuilds has changed the subject mid-measurement.

  `capture` never builds, never syncs, never sources a credential file, and runs
  one runtime at a time with the families in sequence. Every model endpoint it
  reaches is a stub under `e2e/parity/stubs/`, so a full run spends nothing.
"""

from __future__ import annotations

import argparse
import importlib
import json
import os
import subprocess
import sys
import time
import traceback
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import spec  # noqa: E402
from families import Ctx  # noqa: E402
from record import Recorder  # noqa: E402

FAMILIES = [
    "tree",
    "store",
    "wire",
    "cli",
    "ctx",
    "bundle_verbs",
    "kernel_verbs",
    "bridges",
    "ai_paths",
    "providers",
]


# ── preflight ───────────────────────────────────────────────────────────────
def check_staged(rt: str) -> dict:
    """(a) the staged artifact is newer than every watched source file.

    The check `SCR/parity_capture_wire.py:68-70` made by hand, here for every
    runtime — and it names `run.py stage` rather than running it.
    """
    from helpers.host import artifacts

    if rt == "python":
        cmd = spec.kernel_cmd("python")
        missing = [c for c in cmd if not Path(c).exists()]
        return {"check": "artifact", "runtime": rt,
                "ok": not missing, "cmd": cmd,
                "detail": f"missing: {missing}" if missing else "source tree"}
    stamp = spec.DIST / rt / ".staged"
    binary = spec.DIST / rt / "fantastic_kernel"
    if not stamp.exists() or not binary.exists():
        return {"check": "artifact", "runtime": rt, "ok": False,
                "detail": f"no staged artifact — run: run.py stage --runtime {rt}"}
    newest = artifacts._newest_source(spec.REPO, rt)
    ok = stamp.stat().st_mtime >= newest
    return {"check": "artifact", "runtime": rt, "ok": ok,
            "detail": "current" if ok else
                      f"STALE by {newest - stamp.stat().st_mtime:.0f}s — "
                      f"run: run.py stage --runtime {rt}"}


def check_entry_points() -> dict:
    """(b) every value in python's bundle entry-point group imports.

    Python is the ORACLE; a catalog entry that does not import makes python's
    `bundles=` answer list a bundle no kernel can load, and the ports are then
    diffed against a venv rather than against python. It fails today on
    `yaml_state` (a stale editable install in `src/lib/python/.venv`) — D12, and
    NOT repaired here: the choice between `uv sync` and running on
    `e2e/dist/python` is the owner's.
    """
    py = spec.kernel_cmd("python")[0]
    code = (
        "import json\n"
        "from importlib.metadata import entry_points\n"
        "import importlib\n"
        "bad = []\n"
        "names = []\n"
        "for e in entry_points(group='fantastic.bundles'):\n"
        "    names.append(e.name)\n"
        "    mod = e.value.rsplit('.', 1)[0] if e.value.endswith('.tools') else e.value\n"
        "    try:\n"
        "        importlib.import_module(e.value)\n"
        "    except Exception as x:\n"
        "        bad.append([e.name, e.value, type(x).__name__ + ': ' + str(x)])\n"
        "print(json.dumps({'names': sorted(names), 'bad': bad}))\n"
    )
    r = subprocess.run([py, "-c", code], capture_output=True, text=True,
                       env={"PATH": "/usr/bin:/bin"})
    try:
        got = json.loads(r.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {"check": "entry_points", "ok": False,
                "detail": f"could not read the group: {(r.stderr or r.stdout)[-400:]}"}
    return {"check": "entry_points", "ok": not got["bad"], "bundles": got["names"],
            "detail": "all import" if not got["bad"] else
                      f"D12 — these do not import: {got['bad']}"}


def check_no_leftovers() -> dict:
    left = spec.leftover_kernels()
    return {"check": "no_leftover_kernels", "ok": not left,
            "detail": "clean" if not left else f"still running under the work tree: {left}"}


def preflight(runtimes: list[str]) -> int:
    checks = [check_staged(rt) for rt in runtimes]
    if "python" in runtimes:
        checks.append(check_entry_points())
    checks.append(check_no_leftovers())
    spec.RUNS.mkdir(parents=True, exist_ok=True)
    (spec.RUNS / "preflight.json").write_text(json.dumps(checks, indent=1))
    bad = [c for c in checks if not c["ok"]]
    for c in checks:
        mark = "ok  " if c["ok"] else "FAIL"
        print(f"  {mark} {c['check']}{'/' + c['runtime'] if c.get('runtime') else ''}: {c['detail']}")
    print(f"\npreflight: {len(checks) - len(bad)}/{len(checks)} pass")
    return 1 if bad else 0


# ── stage ───────────────────────────────────────────────────────────────────
def stage(rt: str) -> int:
    """The ONLY path that builds. `scripts/build/<rt>.sh --profile debug`
    through `helpers/host/artifacts.stage`, so the harness owns no second
    recipe and an acceptance artifact is packaged exactly like a release."""
    if rt == "python":
        print("python runs from its source tree (spec.kernel_cmd) — nothing to stage")
        return 0
    from helpers.host import artifacts
    from helpers.host.driver import runtime as driver_runtime

    t0 = time.time()
    _, sp = driver_runtime(spec.REPO, rt)
    sp = artifacts.stage(spec.REPO, rt, sp)
    print(f"staged {rt} in {time.time() - t0:.0f}s → {sp['cmd'][0]}")
    return 0


# ── capture ─────────────────────────────────────────────────────────────────
def capture(rt: str, families: list[str], out: Path, slow: bool, slot: str) -> int:
    spec.set_slot(slot)
    if rt not in spec.RUNTIMES:
        raise SystemExit(f"unknown runtime {rt!r}")
    cmd = spec.kernel_cmd(rt)
    if not Path(cmd[0]).exists():
        raise SystemExit(f"{rt}: no artifact at {cmd[0]} — run: run.py stage --runtime {rt}")
    spec.prepare_run_root()
    out.mkdir(parents=True, exist_ok=True)
    summary = {"runtime": rt, "cmd": cmd, "slow": slow, "families": {}}
    failed = []
    for name in families:
        mod = importlib.import_module(f"families.{name}")
        rec = Recorder(out / name, name, rt)
        work, link = spec.family_workdir(name)
        ports = spec.Ports()
        ctx = Ctx(runtime=rt, rec=rec, ports=ports, work=work, link=link, slow=slow)
        t0 = time.time()
        print(f"  [{rt}] {name} …", flush=True)
        try:
            mod.run(ctx)
        except BaseException as e:  # noqa: BLE001 - recorded, then the run fails
            rec.harness_error(f"family {name}", f"{type(e).__name__}: {e}")
            rec.text(f"traceback_{name}.txt", traceback.format_exc())
            failed.append(name)
        meta = {
            "runtime": rt,
            "family": name,
            "cmd": cmd,
            "workdir_real": str(work),
            "workdir_link": str(link),
            "ports": ports.as_map(),
            "ctx_window": spec.CTX_WINDOW,
            "ctx_reserve": spec.CTX_RESERVE,
            "ctx_window_tiny": spec.CTX_WINDOW_TINY,
            "ai_window": spec.AI_WINDOW,
            "ai_reserve": spec.AI_RESERVE,
            "ai_fill_chars": spec.AI_FILL_CHARS,
            **ctx.meta,
        }
        info = rec.finish(meta)
        info["harness"] = {"duration_s": round(time.time() - t0, 1)}
        summary["families"][name] = info
        print(f"  [{rt}] {name}: {info['files']} files, {info['notes']} notes, "
              f"{info['harness_errors']} harness errors, {info['harness']['duration_s']}s", flush=True)
        st = spec.REPO / f"tmp/lane_parity.{slot}.status"
        try:
            with st.open("a") as fh:
                fh.write(f"{time.strftime('%H:%M:%S')} capture {rt}/{name}: "
                         f"{info['files']} files, {info['harness_errors']} errors, "
                         f"{info['harness']['duration_s']}s\n")
        except OSError:
            pass
    # MERGED under a lock: one process per family writes the same summary.
    import fcntl
    with (out / "summary.lock").open("w") as lk:
        fcntl.flock(lk, fcntl.LOCK_EX)
        sp = out / "summary.json"
        if sp.exists():
            prev = json.loads(sp.read_text())
            summary["families"] = {**prev.get("families", {}), **summary["families"]}
            summary["slow"] = summary["slow"] or prev.get("slow", False)
        sp.write_text(json.dumps(summary, indent=1))
    left = spec.leftover_kernels(families)
    if left:
        (out / "leftover_kernels.json").write_text(json.dumps(left, indent=1))
        print(f"  LEFTOVER KERNELS: {left}")
    total_err = sum(f["harness_errors"] for f in summary["families"].values())
    print(f"\ncapture {rt}: {len(families)} families, {total_err} harness errors"
          + (f", FAILED: {failed}" if failed else ""))
    return 1 if (failed or total_err or left) else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("preflight")
    p.add_argument("--runtimes", default="python,rust,swift")

    s = sub.add_parser("stage")
    s.add_argument("--runtime", required=True)

    c = sub.add_parser("capture")
    c.add_argument("--runtime", required=True)
    c.add_argument("--families", default="all",
                   help="comma list, or 'all'. A SUBSET IS FOR ITERATION ONLY — "
                        "acceptance is always 'all' (§5.8 step 4).")
    c.add_argument("--out", required=True)
    c.add_argument("--slot", required=True,
                   help="two characters naming this capture's own work root "
                        "(`runs/parity/w/<slot>/`) — captures with different slots "
                        "run side by side")
    c.add_argument("--slow", action="store_true",
                   help="adds A13.18 (45s socket idle) and the 2 MB body")

    a = ap.parse_args()
    if a.cmd == "preflight":
        return preflight([r for r in a.runtimes.split(",") if r])
    if a.cmd == "stage":
        return stage(a.runtime)
    fams = FAMILIES if a.families == "all" else [f for f in a.families.split(",") if f]
    unknown = [f for f in fams if f not in FAMILIES]
    if unknown:
        raise SystemExit(f"unknown families {unknown} — known: {FAMILIES}")
    return capture(a.runtime, fams, Path(a.out), a.slow, a.slot)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    sys.exit(main())
