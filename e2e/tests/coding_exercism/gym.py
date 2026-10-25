#!/usr/bin/env python3
"""The Exercism gym — the emergent coder's PASS-RATE over real broken repos.

Not an e2e test (those prove each organ once); a BENCHMARK. Runs the whole
coder loop — the E43 trio (file_bridge + a file_bridge `editor` + exec_bridge runner)
driven by the live model — over Aider's polyglot-benchmark, 225 Exercism
exercises across SIX languages, each a born-failing mini-repo with its OWN test
suite as the objective oracle. Scored by rerunning that suite + a never-touched
check on the test files. Zero kernel code; reuses the e2e helpers verbatim.

    python tests/coding_exercism/gym.py --lang python --limit 5   # cheap smoke
    python tests/coding_exercism/gym.py --lang go --only leap
    python tests/coding_exercism/gym.py --lang all --runtime rust
    python tests/coding_exercism/gym.py --langs python,go,rust     # a subset

Self-bootstraps under the kernel's uv venv for --runtime python. rust
spawns its own compiled kernel over REST (bare python is fine). Toolchains
(pytest/go/cargo/npm/cmake/gradlew) are toolchain-prechecked per language and a
missing one SKIPS that language with a reason — never a crash. The clone at
tmp/repos/polyglot-benchmark stays pristine (per-exercise workdir, cleaned).
A BENCHMARK: heavy, paid, resumable-by-log — NOT in run.py's groups.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))
from backends import backend as _resolve_backend  # noqa: E402
from helpers import Limits, connect, drive  # noqa: E402

# Generous budget: the polyglot set is the HARD subset; a from-scratch
# parser/solver needs many fix-rerun cycles. Overridable via env as usual.
GYM_LIMITS = dict(max_turns=45, idle=90.0, no_progress=12)
from helpers.kernel import home, open_kernel  # noqa: E402

BENCH = E2E.parent / "tmp" / "repos" / "polyglot-benchmark"
FB, RUNNER = "gym_fb", "gym_runner"
INTERP = sys.executable
DONE = 'end your run by calling done — emit <call name="done"></call>'
# Folders that leak the reference solution or are pure docs — never copied in.
EXCLUDE = {".meta", ".docs", ".approaches", ".git", "node_modules", "build", "target"}


def _ensure_kernel_env(runtime: str) -> None:
    """The IN-PROCESS python kernel needs its uv workspace venv (the agent
    bundles live there). If not importable, RE-EXEC self once under that
    project — idempotent, and nothing but this launch is touched. rust
    spawns its own compiled kernel over REST, so bare python is fine there."""
    if runtime != "python":
        return
    try:
        import ai_core  # noqa: F401
        return
    except ModuleNotFoundError:
        pass
    kernel = E2E.parent / "src" / "lib" / "python"
    os.execvp("uv", ["uv", "run", "--project", str(kernel), "python",
                     str(Path(__file__).resolve()), *sys.argv[1:]])


@dataclass
class Lang:
    """One language track: how to find the stub + tests, how to run the suite."""
    #: the tool that must be on PATH (or a workdir-relative script) to run at all
    precheck: str
    #: the shell-joined test command, run in the workdir; returncode 0 == green
    test_cmd: list[str]
    #: glob(s) for the STUB file(s) the model implements (relative to the ex dir)
    stub_globs: list[str]
    #: glob(s) for the PROTECTED test file(s) — snapshot + must not change
    test_globs: list[str]
    #: optional one-time setup in the workdir (e.g. npm install); [] = none
    setup_cmd: list[str] = field(default_factory=list)
    #: regexes the exec_bridge runner allows the MODEL to start (its own loop)
    allow: list[str] = field(default_factory=list)


LANGS: dict[str, Lang] = {
    "python": Lang(
        precheck=INTERP,
        test_cmd=[INTERP, "-m", "pytest", "-q"],
        stub_globs=["*.py"], test_globs=["*_test.py"],
        allow=[re.escape(INTERP) + r" -m pytest .*"],
    ),
    "go": Lang(
        precheck="go",
        test_cmd=["go", "test", "./..."],
        stub_globs=["*.go"], test_globs=["*_test.go"],
        allow=[r"go test .*"],
    ),
    "rust": Lang(
        precheck="cargo",
        test_cmd=["cargo", "test", "--", "--include-ignored"],
        stub_globs=["src/lib.rs", "src/*.rs"], test_globs=["tests/*.rs"],
        allow=[r"cargo test .*", r"cargo build .*"],
    ),
    "javascript": Lang(
        precheck="npm",
        test_cmd=["npm", "test", "--silent"],
        stub_globs=["*.js"], test_globs=["*.spec.js"],
        setup_cmd=["npm", "install", "--no-audit", "--no-fund", "--silent"],
        allow=[r"npm test.*", r"npx jest.*", r"node .*"],
    ),
    "cpp": Lang(
        precheck="cmake",
        # build with the bundled Catch, then run the produced test binary.
        test_cmd=["bash", "-lc",
                  "cmake -B build -DCMAKE_BUILD_TYPE=Debug . && "
                  "cmake --build build && ./build/$(basename $PWD)"],
        stub_globs=["*.cpp", "*.h"], test_globs=["*_test.cpp"],
        allow=[r"bash .*", r"cmake .*", r"\./build/.*", r"make.*"],
    ),
    "java": Lang(
        precheck="./gradlew",  # the bundled wrapper — no system gradle needed
        test_cmd=["./gradlew", "test", "--console=plain", "-q"],
        stub_globs=["src/main/java/*.java"], test_globs=["src/test/java/*.java"],
        allow=[r"\./gradlew .*"],
    ),
}


def practice_dir(lang: str) -> Path:
    return BENCH / lang / "exercises" / "practice"


def exercises(lang: str) -> list[Path]:
    root = practice_dir(lang)
    if not root.is_dir():
        return []
    return sorted(p for p in root.iterdir() if p.is_dir())


def toolchain_ok(lang: str, ex: Path | None = None) -> bool:
    """Is the language's runner available? A workdir-relative script (`./gradlew`)
    is checked against a sample exercise; a PATH tool with `which`."""
    p = LANGS[lang].precheck
    if p.startswith("./"):
        exs = exercises(lang)
        return bool(exs) and (exs[0] / p[2:]).exists()
    return shutil.which(p) is not None


def copy_exercise(ex: Path, dst: Path) -> None:
    """Copy the exercise INTO the workdir, minus solution/doc/build folders."""
    for item in ex.iterdir():
        if item.name in EXCLUDE or (item.is_dir() and item.name in EXCLUDE):
            continue
        target = dst / item.name
        if item.is_dir():
            shutil.copytree(item, target,
                            ignore=shutil.ignore_patterns(*EXCLUDE))
        else:
            shutil.copy2(item, target)


def enable_all_tests(lang: str, cwd: Path) -> None:
    """JS ships most tests `xit`-skipped (Exercism's learner convention), which
    makes a "pass" check only the 2 active cases — a weak oracle. Enable them ALL
    in the WORKDIR copy (the pristine clone is untouched; the never-touched check
    snapshots THIS enabled version)."""
    if lang != "javascript":
        return
    import re as _re
    for spec in cwd.glob("*.spec.js"):
        t = spec.read_text()
        t = _re.sub(r"\bxit\s*\(", "it(", t)
        t = _re.sub(r"\bxtest\s*\(", "test(", t)
        t = _re.sub(r"\b(it|test|describe)\.skip\s*\(", r"\1(", t)
        spec.write_text(t)


def instructions(ex: Path) -> str:
    d = ex / ".docs" / "instructions.md"
    return d.read_text() if d.exists() else "(no instructions)"


def match(globs: list[str], base: Path) -> list[Path]:
    out: list[Path] = []
    for g in globs:
        out += [p for p in base.glob(g)
                if p.is_file() and not any(x in p.parts for x in EXCLUDE)]
    return out


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def run_suite(lang: str, cwd: Path) -> tuple[bool, str]:
    cmd = LANGS[lang].test_cmd
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                           check=False, timeout=300,
                           env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
    except subprocess.TimeoutExpired:
        return False, "suite timed out (300s)"
    out = (r.stdout + r.stderr).strip().splitlines()
    tail = next((ln for ln in reversed(out) if ln.strip()), "")
    return r.returncode == 0, tail[:80]


async def one(ex: Path, lang: str, runtime: str) -> dict:
    L = LANGS[lang]
    t0 = time.time()

    root, core, close = await open_kernel(
        prefix=f"gym_{lang}_{ex.name}_", reach=[FB, RUNNER, "editor", "ai", "cli"])
    # open_kernel chdir'd into a fresh mkdtemp — capture it so we can REAP it
    # (with the copied exercise + any build/node_modules) after the lane. The
    # pristine clone under tmp/repos is never touched; we only copy OUT of it.
    workdir = Path(os.getcwd())
    try:
        cwd = workdir
        copy_exercise(ex, cwd)
        enable_all_tests(lang, cwd)  # JS: unskip before we snapshot the baseline
        # snapshot the PROTECTED test files FROM THE WORKDIR (post-transform) — the
        # never-touched check compares the model's result against exactly this.
        # Keyed by RELATIVE path — a basename key broke the untouched check for
        # every language with nested test files (rust tests/, java src/test/):
        # the verify looked at cwd/<basename>, found nothing, and auto-FAILED
        # perfect solutions.
        protected_before = {str(p.relative_to(cwd)): p.read_text(errors="ignore")
                            for p in match(L.test_globs, cwd)}
        stub_rel = [str(p.relative_to(cwd)) for p in match(L.stub_globs, cwd)
                    if str(p.relative_to(cwd)) not in protected_before]
        test_rel = [str(p.relative_to(cwd)) for p in match(L.test_globs, cwd)]
        if L.setup_cmd:
            subprocess.run(L.setup_cmd, cwd=cwd, capture_output=True,
                           check=False, timeout=600)

        for payload in (
            {"type": "create_agent", "parent_id": home(root), "id": FB,
             "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {"ai": "*", "rest": "*", "web": "*"}},
            {"type": "create_agent", "parent_id": home(root), "id": "editor",
             "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {"ai": "*", "rest": "*", "web": "*"}},
            {"type": "create_agent", "parent_id": home(root), "id": RUNNER,
             "handler_module": "exec_bridge.tools", "cwd": os.getcwd(), "inherit_env": ["PATH", "HOME"],
             "ingress_rule": "allow_all_ingress", "allowed": L.allow,
             "callable_by": {"ai": "*", "rest": "*", "web": "*"}},
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        # The start payload VERBATIM — command is argv0 ONLY, args separate.
        # (Teaching "the command is `<full line>`" made the model put the whole
        # line in `command`; exec_bridge shlex-joins argv, a spaced command gets
        # quoted into one token, and the allowlist can never match it.)
        # ONE config for ALL runtimes (E43 pattern): the plain exec_bridge
        # start→poll loop — no runtime-specific facade.
        start_call = json.dumps(
            {"type": "start", "command": L.test_cmd[0], "args": L.test_cmd[1:]}
        )
        task = (
            f"# {ex.name} ({lang})\n\n{instructions(ex)}\n\n"
            f"This is a {lang} exercise. Implement the solution in "
            f"{', '.join('`'+s+'`' for s in stub_rel)} using the editor "
            f"`editor` (reflect it for its verbs — `write` the whole file or "
            f"`edit` it). NEVER edit the test file(s): "
            f"{', '.join('`'+t+'`' for t in test_rel)}. Run the suite with the "
            f"runner `{RUNNER}`: send it exactly this payload — {start_call} — "
            f"it returns {{job_id}} at once; the suite runs in the background. "
            f"Then POLL {{\"type\":\"status\",\"job_id\":...}} until `status` is "
            f"no longer 'running' — NEVER end your run with a job still "
            f"running. Read its output with {{\"type\":\"tail\",\"job_id\":...}} "
            f"for failures, fix the stub, "
            "start again until the suite exits 0. "
            f"When it is green, {DONE}."
        )
        conn = await connect(
            root, core, BK,
            can_call={FB: "*", RUNNER: "*", "editor": "*", "cli": "*"},
            limits=Limits.from_env(os.environ, **GYM_LIMITS))
        await drive(conn, task, client_id="gym")
        await conn.close()

        passed, line = run_suite(lang, cwd)
        untouched = all(
            (cwd / name).exists() and (cwd / name).read_text(errors="ignore") == body
            for name, body in protected_before.items())
        return {"exercise": ex.name, "lang": lang, "runtime": runtime,
                "pass": passed and untouched, "passed": passed,
                "untouched": untouched, "secs": round(time.time() - t0),
                "detail": line}
    finally:
        await close()
        # REAP the exercise's temp workdir — no artifact survives the run.
        try:
            os.chdir(E2E)
            # Only reap something genuinely under the system temp root (macOS
            # uses /var/folders/.../T, which has no "tmp" substring) and that we
            # created (the "gym_" prefix) — never a real project dir.
            tmproot = os.path.realpath(tempfile.gettempdir())
            wd = os.path.realpath(workdir)
            if os.environ.get("FANTASTIC_GYM_KEEP"):
                print(f"  [kept workdir] {wd}")
            elif wd.startswith(tmproot) and "gym_" in Path(wd).name:
                shutil.rmtree(workdir, ignore_errors=True)
        except OSError:
            pass


def _store_result(runtime: str, row: dict) -> None:
    """Append one verdict to the campaign log — the resumable-by-log half.
    Serial path only, which covers every lane (parallel lanes are --jobs 1
    subprocesses running this same serial path)."""
    runs = E2E / "runs"
    runs.mkdir(exist_ok=True)
    rec = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S"), "runtime": runtime,
           "lang": row["lang"], "exercise": row["exercise"],
           "pass": row["pass"], "secs": row["secs"], "detail": row["detail"]}
    with open(runs / "gym_results.jsonl", "a") as f:
        f.write(json.dumps(rec) + "\n")


def _stored_verdicts(runtime: str) -> set[tuple[str, str]]:
    """(lang, exercise) pairs already verdicted for this runtime."""
    p = E2E / "runs" / "gym_results.jsonl"
    out: set[tuple[str, str]] = set()
    if p.exists():
        for line in p.read_text().splitlines():
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("runtime") == runtime:
                out.add((r.get("lang"), r.get("exercise")))
    return out


async def main() -> None:
    global BK
    ap = argparse.ArgumentParser()
    ap.add_argument("--runtime", default="python")
    ap.add_argument("--lang", default="python", help="one language, or 'all'")
    ap.add_argument("--langs", default="", help="comma list, e.g. python,go,rust")
    ap.add_argument("--limit", type=int, default=0, help="first N per language (0=all)")
    ap.add_argument("--sample", type=int, default=0, help="N RANDOM per language")
    ap.add_argument("--seed", type=int, default=0, help="random seed for --sample")
    ap.add_argument("--jobs", type=int, default=1, help="parallel LANES (subprocess-isolated, like run.py)")
    ap.add_argument("--only", default="", help="a single exercise name")
    ap.add_argument("--pick", default="",
                    help="comma list of lang/exercise pairs — the EXACT work "
                         "list, e.g. python/wordy,rust/decimal (overrides "
                         "--lang/--sample/--only)")
    ap.add_argument("--skip-stored", action="store_true",
                    help="skip pairs already verdicted for this runtime in "
                         "runs/gym_results.jsonl (campaign resume)")
    args = ap.parse_args()
    os.environ["RUNTIME"] = args.runtime
    _ensure_kernel_env(args.runtime)
    BK = _resolve_backend()

    # --pick names the exact (lang, exercise) work list — the stratified-subset
    # driver; everything else (toolchain precheck, lanes) applies unchanged.
    picks: dict[str, set[str]] = {}
    if args.pick:
        for pair in args.pick.split(","):
            lang, _, name = pair.strip().partition("/")
            if lang not in LANGS or not name:
                sys.exit(f"bad --pick entry {pair!r} — want lang/exercise")
            picks.setdefault(lang, set()).add(name)

    if picks:
        langs = list(picks)
    elif args.langs:
        langs = [x.strip() for x in args.langs.split(",") if x.strip()]
    elif args.lang == "all":
        langs = list(LANGS)
    else:
        langs = [args.lang]
    for x in langs:
        if x not in LANGS:
            sys.exit(f"unknown language {x!r} — one of {list(LANGS)}")

    # ── build the work list (lang, exercise) respecting toolchain/sample/only ──
    work: list[tuple[str, Path]] = []
    print(f"═══ gym · runtime={args.runtime} · {BK.name} · langs={langs} · jobs={args.jobs}")
    for lang in langs:
        exs = exercises(lang)
        if not exs:
            print(f"### {lang}: SKIP — no exercises at {practice_dir(lang)}")
            continue
        if not toolchain_ok(lang):
            print(f"### {lang}: SKIP — toolchain {LANGS[lang].precheck!r} not available "
                  f"({len(exs)} exercises)")
            continue
        if picks:
            exs = [e for e in exs if e.name in picks[lang]]
            missing = picks[lang] - {e.name for e in exs}
            if missing:
                sys.exit(f"--pick names unknown {lang} exercise(s): {sorted(missing)}")
        elif args.only:
            exs = [e for e in exs if e.name == args.only]
        elif args.sample:
            rng = random.Random(args.seed or None)
            exs = rng.sample(exs, min(args.sample, len(exs)))
        elif args.limit:
            exs = exs[: args.limit]
        exs.sort(key=lambda e: e.name)
        work += [(lang, e) for e in exs]

    if args.skip_stored:
        seen = _stored_verdicts(args.runtime)
        kept = [(lg, e) for lg, e in work if (lg, e.name) not in seen]
        if len(kept) != len(work):
            print(f"═══ --skip-stored: {len(work) - len(kept)} already verdicted "
                  f"for runtime={args.runtime}, skipping")
        work = kept

    print(f"═══ {len(work)} exercises queued\n")
    rows: list[dict] = []
    if args.jobs <= 1:
        # SERIAL, in-process (the only safe mode for --runtime python: the
        # in-process kernel shares cwd + ai_core module-global state keyed by
        # the agent id `ai`, so concurrent lanes would collide).
        for i, (lang, ex) in enumerate(work, 1):
            print(f"─ [{i}/{len(work)}] {lang}/{ex.name}", flush=True)
            try:
                row = await one(ex, lang, args.runtime)
            except Exception as e:  # noqa: BLE001
                row = {"exercise": ex.name, "lang": lang, "pass": False,
                       "passed": False, "untouched": True, "secs": 0,
                       "detail": f"crash: {e!r}"[:80]}
            rows.append(row)
            _store_result(args.runtime, row)
            print(f"   {'PASS' if row['pass'] else 'FAIL'}  {lang}/{ex.name}  "
                  f"{row['secs']}s  {row['detail']}")
    else:
        # PARALLEL LANES via SUBPROCESS isolation — each exercise its own
        # process (own cwd, own kernel, own ai_core globals), self-reaping its
        # workdir. This is how the in-process constraint is escaped safely.
        sem = asyncio.Semaphore(args.jobs)
        self_path = str(Path(__file__).resolve())

        async def lane(lang: str, ex: Path) -> dict:
            async with sem:
                proc = await asyncio.create_subprocess_exec(
                    sys.executable, self_path, "--runtime", args.runtime,
                    "--lang", lang, "--only", ex.name, "--jobs", "1",
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
                out, _ = await proc.communicate()
                text = out.decode(errors="ignore")
                m = re.search(rf"(PASS|FAIL)  {re.escape(lang)}/{re.escape(ex.name)}  "
                              r"(\d+)s  (.*)", text)
                ok = bool(m) and m.group(1) == "PASS"
                secs = int(m.group(2)) if m else 0
                detail = (m.group(3)[:80] if m else "no verdict line") 
                print(f"   {'PASS' if ok else 'FAIL'}  {lang}/{ex.name}  {secs}s  {detail}",
                      flush=True)
                return {"exercise": ex.name, "lang": lang, "pass": ok, "secs": secs,
                        "detail": detail, "passed": ok, "untouched": True}

        rows = list(await asyncio.gather(*(lane(lang, ex) for lang, ex in work)))

    # ── summary ──
    by_lang: dict[str, list[dict]] = {}
    for r in rows:
        by_lang.setdefault(r["lang"], []).append(r)
    print()
    for lang, rs in sorted(by_lang.items()):
        n = sum(r["pass"] for r in rs)
        print(f"  {lang:12} {n}/{len(rs)}")
    total_ok = sum(r["pass"] for r in rows)
    # Belt-and-suspenders sweep ONLY in the top-level orchestrator (jobs>1),
    # AFTER every lane finished — a lane subprocess (jobs==1) must NEVER sweep:
    # it would delete sibling lanes' LIVE workdirs (they share the temp root),
    # which is exactly the FileNotFoundError that bit the parallel run. Per-lane
    # reap (one()'s finally, its OWN path) is the subprocess's cleanup.
    if args.jobs > 1:
        _sweep_temp()
    print(f"\n═══ GYM RESULT · {args.runtime} · {total_ok}/{len(rows)} passed "
          f"across {len(langs)} language(s)")


def _sweep_temp() -> None:
    root = os.path.realpath(tempfile.gettempdir())
    for d in Path(root).glob("gym_*"):
        if d.is_dir():
            shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    asyncio.run(main())
