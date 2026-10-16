"""THE LAUNCHER — run the suite, any backend, N at a time.

    cd src/lib/python && uv run python ../../../e2e/run.py --lanes 6

It reads `e2e/suite/*.yaml` (what to measure), resolves ONE backend from
`e2e/backends/` (how to reach a model), and runs each test as its own process.
Nothing about a test knows which model answered it.

Three properties worth stating, because each replaces something that bit:

  * ONE VERDICT TOKEN. Probes print `E2E-VERDICT: PASS|FAIL`. A probe that
    prints none is reported `no verdict` — never guessed into a pass. The old
    five wording dialects meant a reworded probe silently became unscoreable.
  * LANES ARE A SHARED RESOURCE. The GPU behind the endpoint holds a fixed
    number of sequences and other people's traffic shares it. `--lanes` is a promise
    about someone else's latency, not a throughput dial.
  * EVERY RUN KEEPS ITS LOG. `runs/<stamp>/<group>.<id>.log` holds the full
    transcript. A failure you cannot re-read is a failure you cannot diagnose —
    and these cost real turns, so re-running to look again is not free.
"""

from __future__ import annotations

import argparse
import asyncio
import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from backends import THINK_BUDGET
from backends import backend as resolve_backend  # noqa: E402
from helpers import oslayer  # noqa: E402
from helpers.verdict import MARKER  # noqa: E402

from helpers.host.driver import ensure_runtime  # noqa: E402

E2E = Path(__file__).resolve().parent
#: The CLI's own live tests (`src/tui/e2e/tests/<group>/<id>/test.yaml`).
CLI_TESTS = E2E.parent / "src" / "tui" / "e2e" / "tests"
REPO = E2E.parent
PYROOT = REPO / "src" / "lib" / "python"

#: Where each runner is invoked from. A native suite runs in ITS OWN tree.
CWDS = {
    "e2e": E2E,
    "pyroot": PYROOT,
    "rust": REPO / "src" / "lib" / "rust",
    "swift": REPO / "src" / "lib" / "swift",
}


#: A test's declared limits → the env its runner reads. ONE table, used by both
#: the real run and the virtual one — two copies would let `--dry-run` promise
#: something the run does not do, which is the one thing a dry run must never do.
#:
#: `max_tokens` is NOT here. The think budget is a property of the model plug and
#: lives as ONE constant, `backends.THINK_BUDGET` — see the note there. It was a
#: per-test knob and 25 files carried their own value; one carried none and
#: silently invalidated its own A/B.
def limit_env(t: dict) -> dict:
    """A test's declared limits, rendered into the env its runner reads.

    ONE builder for the real run and the virtual one. `run.py` promises a dry run
    never advertises something the run does not do; rendering the env separately
    lets a variable added to one path go missing from the other.
    """
    out = {k: str(v) for k, v in (t.get("env") or {}).items()}
    lim = t.get("limits") or {}
    for src_key, env_key in LIMIT_ENV:
        if src_key in lim:
            out[env_key] = str(lim[src_key])
    # THE THINK BUDGET — one constant, every test (see `backends.THINK_BUDGET`).
    # A test does not declare it; an explicit `MAX_TOKENS=…` in the environment
    # still wins for one deliberate run.
    out.setdefault("MAX_TOKENS", os.environ.get("MAX_TOKENS") or str(THINK_BUDGET))
    return out


LIMIT_ENV = (
    ("idle_seconds", "IDLE"),
    ("max_turns", "MAX_TURNS"),
    ("no_progress_turns", "NO_PROGRESS"),
    ("turn_chars", "TURN_CHARS"),
    ("turn_seconds", "TURN_SECONDS"),
)


#: runtime → how to run a shared scenario. Adding a runtime is adding a ROW; no
#: scenario changes.
#:
#: ONE RUNNER, ONE FACE. Every host runtime is SPAWNED and driven over REST by
#: `runners/host.py`; the substrate is a plug (`KERNEL`), exactly as the model
#: is a plug in `backends/`.
#:
#: An in-process runner could call `root.send(...)` directly — name an
#: execution, act as operator, register bespoke in-process fixtures — none of
#: which is reachable over HTTP, so the same scenario would not be the same test
#: on every runtime. Driving all three through one face keeps them comparable.
#: The runtimes the host driver spawns — the ones with something to build.
HOST_RUNTIMES = ("python", "rust", "swift")

SCENARIO_RUNNERS = {
    # ABSOLUTE path: cwd is the python tree, where `runners/` does not exist.
    "python": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/host.py")],
        "cwd": "pyroot",
        "env": {"KERNEL": "python"},
    },
    "rust": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/host.py")],
        "cwd": "pyroot",
        "env": {"KERNEL": "rust"},
    },
    "swift": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/host.py")],
        "cwd": "pyroot",
        "env": {"KERNEL": "swift"},
    },
    # THE PRODUCT BINARY'S MCP FACE. Not a kernel runtime: `fantastic mcp`
    # hosts the rust kernel in-process and speaks MCP over stdio, and the runner
    # is a SCRIPTED MCP client — no model anywhere. Only scenarios that name it
    # in `runtimes:` run here (E15), and they run nowhere else.
    "mcp": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/mcp.py")],
        "cwd": "pyroot",
    },
    # THE SHELL ITSELF: `fantastic` in a pseudo-terminal, driven by a scripted
    # keyboard through the shell's own PTY rig. Like `mcp`, not a kernel
    # runtime, and only scenarios that name it run here (E16).
    "tui": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/tui.py")],
        "cwd": "pyroot",
    },
    # THE CANVAS: the ts kernel bundle served by the rust canvas host, driven
    # in a real headless Chrome over CDP (E19). No model.
    "canvas": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/canvas.py")],
        "cwd": "pyroot",
    },
    # THE CANVAS'S ONE USER: one host, a headless Chrome per browser storage (E21). No model.
    "canvas_user": {
        "cmd": ["uv", "run", "python", "-u", str(E2E / "runners/canvas_user.py")],
        "cwd": "pyroot",
    },
}


def load_suite() -> list[dict]:
    """Every test is a FOLDER: its own md, its own kernel roster, its own knobs.

    Discovery is `tests/*/*/test.yaml` — adding a test is adding a directory, and
    there is no central list to forget to update.
    """
    import yaml

    out = []
    # SHARED SCENARIOS × RUNTIMES. One definition in `scenarios/`, executed by
    # each runtime's runner — not a copied probe file per runtime: copies drift,
    # and drifted results are not comparable, which is the only reason to run a
    # scenario on every runtime.
    runtimes = [
        r.strip()
        for r in os.environ.get("RUNTIMES", "python,rust,swift").split(",")
        if r.strip()
    ]
    for sf in sorted(E2E.glob("tests/scenario/*/scenario.yaml")):
        doc = yaml.safe_load(sf.read_text()) or {}
        # A scenario that names its runtimes runs ONLY on those, and the
        # model-driven scenarios never run on a runtime they did not name: the
        # `mcp` face is not a kernel runtime, so E10–E14 must not cross into
        # it and E15 must not cross out of it.
        declared = doc.get("runtimes")
        for rt in declared or [r for r in runtimes if r not in ("mcp", "tui")]:
            spec = SCENARIO_RUNNERS[rt]
            # DERIVED OVERRIDE. A scenario is shared, but a runtime can need a
            # different budget for the SAME work — the compiled kernels drive
            # `exec_bridge` through start/poll, which costs turns the in-process
            # path does not. `overrides: {rust: {limits: {...}}}` adjusts the
            # BUDGET without forking the scenario; anything that changed the
            # TASK would make the columns incomparable, which is the whole thing
            # this layout exists to prevent.
            over = (doc.get("overrides") or {}).get(rt) or {}
            limits = {**(doc.get("limits") or {}), **(over.get("limits") or {})}
            # The dir's `E<nn>_` prefix rides on the test id so a result line
            # cites the directory it came from. Selection still accepts the bare
            # name (see `select`), so `--id dialog` keeps working.
            code = sf.parent.name.split("_", 1)[0]
            camel = "".join(
                w if i == 0 else w.capitalize()
                for i, w in enumerate(doc["id"].split("_"))
            )
            sel = spec.get("select")
            args = [sel.format(id=doc["id"], camel=camel)] if sel else [doc["id"]]
            out.append(
                {
                    **doc,
                    "id": f"{code}_{doc['id']}:{rt}",
                    "group": "scenario",
                    "dir": sf.parent,
                    "cmd": spec["cmd"],
                    "cwd_key": spec["cwd"],
                    "extra_env": spec.get("env", {}),
                    "args": args,
                    "runtime": rt,
                    # A scripted scenario spends no turns and says so.
                    "costs": doc.get("costs", "turns"),
                    "limits": limits,
                    "env": {**(doc.get("env") or {}), **(over.get("env") or {})},
                    "title": f"{doc.get('title', '')} [{rt}]",
                }
            )

    # Two roots: the kernel suite's own, and the CLI's (`src/tui/e2e/tests` —
    # tests of the product, driven by the same launcher and the same plug).
    for f in sorted([*E2E.glob("tests/*/*/test.yaml"), *CLI_TESTS.glob("*/*/test.yaml")]):
        doc = yaml.safe_load(f.read_text()) or {}
        d = f.parent
        # A probe that declares `runtimes:` is run ONCE PER RUNTIME, the same
        # cross-product the scenarios get. `RUNTIME` is the plug the probe reads
        # (`helpers.kernel.open_kernel`): in process for python, spawned and
        # driven over REST otherwise. A probe with no `runtimes:` is python-only
        # and says so by omission rather than by silence.
        rts = doc.get("runtimes")
        if rts:
            for rt in rts:
                out.append(
                    {
                        **doc,
                        "id": f"{doc['id']}:{rt}",
                        "dir": d,
                        "module": d / doc.get("runner", "probe.py"),
                        "costs": doc.get("costs", "turns"),
                        "runtime": rt,
                        "env": {**(doc.get("env") or {}), "RUNTIME": rt},
                        "title": f"{doc.get('title', '')} [{rt}]",
                    }
                )
        else:
            out.append(
                {
                    **doc,
                    "dir": d,
                    "module": d / doc.get("runner", "probe.py"),
                    "costs": doc.get("costs", "turns"),
                }
            )
        if st := doc.get("selftest"):
            out.append(
                {
                    **doc,
                    **st,
                    "dir": d,
                    "id": f"{doc['id']}_selftest",
                    "module": d / st.get("runner", "selftest.py"),
                    "costs": st.get("costs", "free"),
                }
            )
    return out


_GUESSED_ENDPOINT = re.compile(
    r"""os\.environ\.get\(\s*["'][A-Z_]+["']\s*,\s*["']https?://"""
)


def check_no_guessed_endpoints() -> list[str]:
    """No `os.environ.get(VAR, "http://…")` anywhere in the harness. A loopback
    model-server default hides in a branch nobody runs until a probe copies it and
    every runtime fails on a server nobody wired. Loopback for the harness's OWN
    spawned kernel is fine; a guessed EXTERNAL endpoint is not."""
    out: list[str] = []
    for sub in ("tests", "backends", "helpers", "runners"):
        for f in (E2E / sub).rglob("*.py"):
            for n, line in enumerate(f.read_text().splitlines(), 1):
                if _GUESSED_ENDPOINT.search(line):
                    out.append(f"{f.relative_to(E2E)}:{n}: {line.strip()[:90]}")
    return out


def check_rosters(tests) -> list[str]:
    """`kernel.yaml` must still describe what `probe.py` builds.

    A declared roster nobody verifies drifts the first time a fixture changes, and
    then it is worse than no document: it is a confident wrong answer to "what does
    this test wire up". Re-extracted from the source every run, using the SAME
    extractor that wrote it (helpers/roster.py) — a second copy here would drift
    against the generator and report mismatches that exist only in the reader.
    """
    import yaml

    from helpers.roster import roster

    drift = []
    for t in tests:
        if t.get("cmd"):
            continue  # a native suite owns its own fixture; no probe.py to diff
        kf = t["dir"] / "kernel.yaml"
        pf = t["dir"] / "probe.py"
        # A js probe builds its tree in process and has no python source to
        # extract from; its kernel.yaml says so in prose instead.
        if not kf.exists() or not pf.exists():
            continue
        declared = (yaml.safe_load(kf.read_text()) or {}).get("roster") or []
        actual = roster(pf)
        if declared != actual:
            drift.append(
                f"{t['group']}/{t['id']}: kernel.yaml declares "
                f"{len(declared)} agents, probe.py builds {len(actual)}"
                if len(declared) != len(actual)
                else f"{t['group']}/{t['id']}: kernel.yaml no longer matches probe.py"
            )
    return drift


def select(tests, groups, ids, backend_name, include_free, runtimes=(), excluded=()):
    """Narrow the batch along any axis, in any combination.

    A scenario id matches BOTH its bare name and its per-runtime instance:
    `--id dialog` picks `dialog:python`, `dialog:rust`, … while
    `--id dialog:rust` picks exactly one. Without that, the cross-product would
    be selectable only by typing every instance out, and the obvious command —
    "run dialog" — would silently select nothing, which is worse than an error.
    """
    picked = []
    for t in tests:
        if groups and t["group"] not in groups:
            continue
        if runtimes and t.get("runtime") not in runtimes:
            continue
        if t.get("runtime") in excluded:
            continue
        # Accept any of: the full id (`E10_dialog:rust`), the numbered stem
        # (`E10_dialog`), the bare scenario name (`dialog`), or the code alone
        # (`E10`). A citable index is only useful if you can cite it.
        if ids:
            stem = t["id"].split(":")[0]
            bare = stem.split("_", 1)[-1]
            code = stem.split("_", 1)[0]
            if not ({t["id"], stem, bare, code} & set(ids)):
                continue
        if (ob := t.get("only_backend")) and ob != backend_name:
            continue
        if not include_free and t.get("costs") == "free":
            continue
        picked.append(t)
    return picked


# LONG TESTS SHARE THE MODEL. On one RTX6000, six long lanes push single turns past the
# proxy's 180 s limit; at 1–2 lanes they do not. A test marked `long: true` in its test.yaml takes one of these two
# slots FIRST, then a lane; short tests use the lanes freely.
LONG_LANES = 2

# LOAD TESTS RUN ALONE. A test marked `solo: true` in its test.yaml MEASURES the
# machine — frame rate, GPU, a time bound — so another test beside it changes its
# numbers. Solo tests run after
# the rest, one at a time, nothing else running.

# THE HARD STOP: 15 minutes per test. A test still running then is
# stopped (its whole process group, kernel included) and scored FAIL with the reason
# in its log — a run that needs longer is a finding, not a wait. A test that names its
# own `hard_stop_minutes` in its test.yaml gets that instead.
HARD_STOP_S = 15 * 60


async def run_one(
    t: dict,
    bk,
    logdir: Path,
    sem: asyncio.Semaphore,
    results: dict,
    long_sem: "asyncio.Semaphore | None" = None,
):
    name = f"{t['group']}.{t['id']}"
    if t.get("long") and long_sem is not None:
        async with long_sem:
            await _run_one(t, bk, logdir, sem, results, name)
    else:
        await _run_one(t, bk, logdir, sem, results, name)


async def _run_one(t: dict, bk, logdir: Path, sem: asyncio.Semaphore, results: dict, name: str):
    async with sem:
        log = logdir / f"{name}.log"
        # The knobs a test owns, rendered into the env its runner already reads.
        env = {**os.environ, **limit_env(t)}
        # THE METER (E2E_METER=1): this test's model traffic goes through the
        # run's forwarder under the test's own name — see helpers/meter.py.
        if meter := os.environ.get("E2E_METER_URL"):
            env["VLLM_ENDPOINT"] = f"{meter}/m/{name}"
        began = time.monotonic()
        stop_s = int(t.get("hard_stop_minutes") or HARD_STOP_S // 60) * 60
        results[name] = {"state": "running", "since": began}
        with log.open("w") as fh:
            fh.write(
                f"# {name} · {t['title']}\n# backend={bk.name} model={bk.model}\n\n"
            )
            fh.flush()
            # The runner's EXTENSION picks the interpreter. A js probe drives
            # the browser kernel under node (no build step — node strips the
            # types), a python one drives the host kernel under uv.
            args = [str(a) for a in (t.get("args") or [])]
            if t.get("cmd"):
                # A SCENARIO. Every runtime runs it through the one host runner,
                # so the command comes from the runner table rather than from a
                # file extension.
                cmd = [*t["cmd"], *args]
                cwd = str(CWDS[t["cwd_key"]])
                env.update({k: str(v) for k, v in (t.get("extra_env") or {}).items()})
            elif t["module"].suffix in (".js", ".mjs"):
                cmd = ["node", str(t["module"]), *args]
                cwd = str(E2E)
            else:
                cmd = ["uv", "run", "python", "-u", str(t["module"]), *args]
                cwd = str(PYROOT)
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                cwd=cwd,
                env=env,
                stdout=fh,
                stderr=asyncio.subprocess.STDOUT,
                **oslayer.group_kwargs(),  # a lane is a process group: killed whole, never orphaned
            )
            try:
                try:
                    rc = await asyncio.wait_for(proc.wait(), stop_s)
                except TimeoutError:
                    oslayer.stop_group(proc.pid)
                    try:
                        rc = await asyncio.wait_for(proc.wait(), 20)
                    except TimeoutError:
                        oslayer.kill_group(proc.pid)
                        rc = await proc.wait()
                    fh.write(
                        f"\n{MARKER} FAIL — HARD STOP: still running after "
                        f"{stop_s // 60} min\n"
                    )
                    fh.flush()
            except (asyncio.CancelledError, KeyboardInterrupt):
                # THE LANE DIES WITH THE RUN: Ctrl-C or a cancelled batch must not
                # leave the probe and its spawned kernel running.
                oslayer.stop_group(proc.pid)
                raise
        took = time.monotonic() - began
        body = log.read_text()
        # Every scenario runner now reports through the shared verdict marker,
        # so there is no per-runtime reporting path to keep in step.
        if False:
            detail = ""
            if m := re.findall(r"^\s*(?:store\.read|.*side effects).*$", body, re.M):
                detail = " · ".join(x.strip() for x in m[-2:])[:120]
            # A ZERO-TEST RUN IS NOT A PASS. Both `cargo test` and `swift test`
            # exit 0 when a filter matches nothing, which turned a mis-spelled
            # selector into a green row. Require evidence that the scenario
            # actually ran.
            ran = re.search(r"Test run with (\d+) test", body)
            empty = (ran and ran.group(1) == "0") or "0 passed; 0 failed" in body
            if empty:
                state = "no verdict"
                detail = "filter matched NO tests — nothing ran"
            else:
                state = "pass" if rc == 0 else "fail"
            results[name] = {
                "state": state,
                "note": detail,
                "took": took,
                "rc": rc,
                "log": log,
            }
            print(
                f"  {'PASS' if rc == 0 else 'FAIL'}  {name:<34} {took:6.0f}s  {detail}",
                flush=True,
            )
            return

        if skip := re.search(r"^E2E-SKIP:\s*(.+)$", body, re.M):
            # A scenario a runtime CANNOT play is skipped by name. Reporting it
            # as a pass would inflate the table; as a fail would blame the run
            # for a boundary that was declared up front.
            results[name] = {
                "state": "skip",
                "note": skip.group(1)[:120],
                "took": took,
                "rc": rc,
                "log": log,
            }
            print(f"  SKIP  {name:<34} {took:6.0f}s  {skip.group(1)[:80]}", flush=True)
            return
        hits = re.findall(
            rf"^{re.escape(MARKER)}\s*(PASS|FAIL)(?:\s+—\s+(.*))?$", body, re.M
        )
        if hits:
            state, note = ("pass" if hits[-1][0] == "PASS" else "fail"), hits[-1][1]
        else:
            # NOT a fail. "The probe never said" and "the probe said no" are
            # different facts and collapsing them invents a result.
            state, note = "no verdict", f"exit={rc}"
        results[name] = {
            "state": state,
            "note": note,
            "took": took,
            "rc": rc,
            "log": log,
        }
        mark = {"pass": "PASS", "fail": "FAIL", "no verdict": "????"}[state]
        print(f"  {mark}  {name:<34} {took:6.0f}s  {note or ''}", flush=True)


class _NoBackend:
    """The plug for a run that selects only free tests: nothing is reached."""

    name = "none"
    model = "-"

    def blockers(self) -> list[str]:
        return []


async def start_meter(logdir: Path):
    """Spawn helpers/meter.py in front of `VLLM_ENDPOINT`; per-request usage
    lands in `<logdir>/meter.jsonl`. Opt-in (`E2E_METER=1`), vLLM only."""
    import socket
    import urllib.request

    upstream = os.environ.get("VLLM_ENDPOINT")
    if not upstream:
        raise SystemExit("E2E_METER=1 needs VLLM_ENDPOINT — the meter forwards to it")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    proc = await asyncio.create_subprocess_exec(
        "uv", "run", "python", "-u", str(E2E / "helpers" / "meter.py"),
        "--port", str(port), "--upstream", upstream,
        "--out", str(logdir / "meter.jsonl"),
        cwd=str(PYROOT),
    )
    url = f"http://127.0.0.1:{port}"
    for _ in range(100):
        try:
            await asyncio.to_thread(
                lambda: urllib.request.urlopen(f"{url}/health", timeout=1).read()
            )
            break
        except OSError:
            await asyncio.sleep(0.2)
    else:
        proc.terminate()
        raise SystemExit("meter did not come up")
    os.environ["E2E_METER_URL"] = url
    print(f"═══ meter: {url} → {logdir / 'meter.jsonl'}", flush=True)
    return proc


async def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--lanes", type=int, default=int(os.environ.get("LANES", "6")))
    ap.add_argument(
        "--group",
        action="append",
        default=[],
        help="scenario | agentic | agentic_advanced | contract | backend",
    )
    ap.add_argument(
        "--id",
        action="append",
        default=[],
        help="a test id; a bare scenario name selects every runtime of it",
    )
    ap.add_argument(
        "--runtime",
        action="append",
        default=[],
        help="python | rust | swift — narrow the cross-product",
    )
    ap.add_argument(
        "--exclude-runtime",
        action="append",
        default=[],
        help="drop one runtime's cells, keep everything else (CI: no swift on Windows)",
    )
    ap.add_argument(
        "--free", action="store_true", help="include the free harness self-tests"
    )
    ap.add_argument("--only-free", action="store_true")
    ap.add_argument("--list", action="store_true")
    ap.add_argument(
        "--dry-run",
        action="store_true",
        help="VIRTUAL RUN — resolve everything and print the exact command, env "
        "overrides and log path for each test, then spend nothing. These "
        "tests cost real model turns, so being able to see the whole plan "
        "before paying for it is not a convenience.",
    )
    a = ap.parse_args()

    # A FREE-ONLY run spends no model turns, so it needs no model plug: naming
    # a BACKEND for it would be a lie about what the run uses. Any run that can
    # select a paid test still resolves it, and still fails loud when unset.
    bk = _NoBackend() if a.only_free and not os.environ.get("BACKEND") else resolve_backend()
    tests = load_suite()
    if a.only_free:
        tests = [t for t in tests if t.get("costs") == "free"]
        picked = select(tests, a.group, a.id, bk.name, True, a.runtime, a.exclude_runtime)
    else:
        picked = select(tests, a.group, a.id, bk.name, a.free, a.runtime, a.exclude_runtime)

    if a.list:
        for t in picked:
            print(f"{t['group']:<18} {t['id']:<20} {t['costs']:<6} {t['title']}")
        return 0

    # A probe with no `verdict(...)` call scores `no verdict` FOREVER and nobody
    # notices, because "the launcher never said" reads like "still working". One
    # probe shipped that way for a whole run; catching it costs a grep.
    if mute := [
        f"{t['group']}/{t['id']}: {t['module'].name} never calls verdict()"
        for t in picked
        if t["group"] != "scenario"
        and t.get("module") is not None
        and "verdict(" not in t["module"].read_text()
    ]:
        print("REFUSING TO RUN — these probes emit no machine verdict:")
        for m in mute:
            print("  · " + m)
        return 1

    if guessed := check_no_guessed_endpoints():
        print("REFUSING TO RUN — a guessed network endpoint is coded into the harness:")
        for g in guessed:
            print(f"  · {g}")
        print("  the operator names hosts (connection env); a default that reaches a")
        print(
            "  network measures the wrong thing. Remove the default, require the var."
        )
        return 1
    if drift := check_rosters(picked):
        print("REFUSING TO RUN — a test's kernel.yaml no longer matches its probe:")
        for d in drift:
            print("  · " + d)
        print("  regenerate the roster, or fix the probe, then re-run.")
        return 1

    if blockers := bk.blockers():
        print(
            "REFUSING TO RUN — these would fail for a reason that is not the substrate:"
        )
        for b in blockers:
            print("  · " + b)
        return 1
    if not picked:
        print("nothing selected")
        return 1

    if a.dry_run:
        print(
            f"═══ VIRTUAL RUN · {len(picked)} tests · {a.lanes} lanes "
            f"· {bk.name}/{bk.model}\n"
        )
        for t in picked:
            env_over = limit_env(t)
            env_over.update({k: str(v) for k, v in (t.get("extra_env") or {}).items()})
            args = " ".join(str(x) for x in (t.get("args") or []))
            if t.get("cmd"):
                where = CWDS[t["cwd_key"]].name
                print(f"  {t['group']}.{t['id']}")
                print(f"      [{where}] {' '.join(t['cmd'])} {args}".rstrip())
            else:
                runner = (
                    "node"
                    if t["module"].suffix in (".js", ".mjs")
                    else "uv run python -u"
                )
                print(f"  {t['group']}.{t['id']}")
                print(f"      {runner} {t['module'].relative_to(E2E)} {args}".rstrip())
            if env_over:
                print(
                    "      env: "
                    + " ".join(f"{k}={v}" for k, v in sorted(env_over.items()))
                )
            print(f"      log: runs/<stamp>/{t['group']}.{t['id']}.log")
        print(f"\n  nothing was run · {len(picked)} tests would spend model turns")
        return 0

    stamp = time.strftime("%Y%m%d-%H%M%S")
    logdir = E2E / "runs" / stamp
    logdir.mkdir(parents=True, exist_ok=True)
    print(f"═══ {len(picked)} tests · {a.lanes} lanes · {bk.name}/{bk.model}")
    print(f"═══ logs: {logdir}\n", flush=True)

    # BUILD EACH KERNEL ONCE, HERE — not once per cell.
    #
    # `ensure_runtime` inside every cell would run concurrent builds that
    # serialise on the build lock (SwiftPM's, for swift), and that time would be
    # charged to the cells — making a runtime look slower than its per-turn
    # latency, and failing long scenarios under many lanes.
    prebuilt = sorted(
        {t["runtime"] for t in picked if t.get("runtime") in HOST_RUNTIMES}
    )
    for rt in prebuilt:
        t0 = time.monotonic()
        ensure_runtime(REPO, rt)
        print(f"═══ built {rt} in {time.monotonic() - t0:.1f}s", flush=True)
    meter = None
    if os.environ.get("E2E_METER") == "1":
        meter = await start_meter(logdir)
    if prebuilt:
        # Cells inherit this and skip their own build; a missing binary still
        # fails loud there rather than being silently tolerated.
        os.environ["E2E_PREBUILT"] = ",".join(prebuilt)
        print(flush=True)

    sem = asyncio.Semaphore(a.lanes)
    long_sem = asyncio.Semaphore(min(LONG_LANES, a.lanes))
    results: dict = {}
    # Written every time a test settles, so a watcher tailing this file sees
    # progress without parsing 14 transcripts.
    began = time.monotonic()
    await asyncio.gather(
        *(run_one(t, bk, logdir, sem, results, long_sem) for t in picked if not t.get("solo"))
    )
    for t in (t for t in picked if t.get("solo")):
        await run_one(t, bk, logdir, sem, results, long_sem)

    if meter is not None:
        meter.terminate()
        await meter.wait()

    counts: dict[str, int] = {}
    for r in results.values():
        counts[r["state"]] = counts.get(r["state"], 0) + 1
    print("\n" + "═" * 72)
    for name in sorted(results):
        r = results[name]
        print(
            f"  {r['state']:<10} {name:<34} {r.get('took', 0):6.0f}s  {r.get('note') or ''}"
        )
    print("═" * 72)
    print(
        "  "
        + " · ".join(f"{k}={v}" for k, v in sorted(counts.items()))
        + f"  ·  {time.monotonic() - began:.0f}s wall"
    )
    (logdir / "SUMMARY.txt").write_text(
        "\n".join(
            f"{results[n]['state']}\t{n}\t{results[n].get('note') or ''}"
            for n in sorted(results)
        )
        + "\n"
    )
    # A SKIP is a declared boundary, not a failure: a runtime that cannot play
    # a role says so up front. Counting it against the run would push toward
    # approximating the role with something that behaves differently, which is
    # how a table stops meaning anything.
    return 0 if counts.get("fail", 0) + counts.get("no verdict", 0) == 0 else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
