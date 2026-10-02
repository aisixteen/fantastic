"""Run a shell scenario against the `fantastic` binary — a SCRIPTED KEYBOARD, no model.

The shell is driven in a pseudo-terminal by its own rig (`src/tui/e2e/
screenshot.rs`): keys in, vt100 frames out as text. Every key and every expected
frame is in the scenario, so what is measured is the binary.

    uv run python e2e/runners/tui.py tui                   # build src/tui + the rig
    uv run python e2e/runners/tui.py tui --bin /path/fantastic

THE BINARY IS A PLUG (`--bin` / `FANTASTIC_BIN`), exactly as in `runners/mcp.py`.
A scenario may name cargo `features:` for the build it needs; the built binary
is COPIED into the run's temp dir, so a parallel scenario rebuilding with other
features cannot swap it underneath.

A scenario may also declare `stub: {vllm: {chain: [...]}}` (the parity harness's
scripted OpenAI chat endpoint, `parity/stubs/openai_chat.py`) or
`stub: {responses: {chain: [...]}}`: the stub starts on 127.0.0.1, `{stub_url}` in a
script becomes its URL, and `env:` is added to the shell's environment (a stub key,
never a real one). No model, no network.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from helpers import verdict  # noqa: E402

# The binary's user-home template store (`user_home_store`) reads FANTASTIC_HOME,
# else ~/.config/fantastic. A test must never read or write the owner's real
# home: every binary this runner launches inherits a throwaway one.
os.environ["FANTASTIC_HOME"] = tempfile.mkdtemp(prefix="fc-e2e-home-")

REPO = E2E.parent
TUI = REPO / "src" / "tui"
TARGET = REPO / "src" / "target" / "debug"


def _scenario_path(scenario_id: str) -> Path:
    root = E2E / "tests" / "scenario"
    for d in sorted(root.iterdir()):
        if d.is_dir() and d.name.split("_", 1)[-1] == scenario_id:
            return d / "scenario.yaml"
    raise SystemExit(f"no scenario {scenario_id!r} under {root}")


def build(what: list[str]) -> None:
    r = subprocess.run(["cargo", "build", "--manifest-path", str(TUI / "Cargo.toml"), *what],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr[-2000:])
        raise SystemExit(f"cargo build {' '.join(what)} failed")


def resolve(argv: list[str], features: list[str], into: Path) -> tuple[Path, Path]:
    """(the shell binary, the PTY rig), both copied into `into`. The rig is
    always this repo's."""
    if "--bin" in argv:
        i = argv.index("--bin")
        b = Path(argv[i + 1]).expanduser().resolve()
    elif os.environ.get("FANTASTIC_BIN"):
        b = Path(os.environ["FANTASTIC_BIN"]).expanduser().resolve()
    else:
        what = ["--features", ",".join(features)] if features else []
        print(f"building {TUI} {' '.join(what)}…", flush=True)
        build(what)
        b = TARGET / "fantastic"
    build(["--example", "screenshot"])
    rig = TARGET / "examples" / "screenshot"
    out = []
    for p in (b, rig):
        if not p.is_file() or not os.access(p, os.X_OK):
            raise SystemExit(f"not an executable: {p}")
        dst = into / p.name
        shutil.copy2(p, dst)
        out.append(dst)
    return out[0], out[1]


class _Rec:
    """The parity stubs record every request; here into the run's temp dir."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def raw(self, rel: str, data: bytes) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def json(self, rel: str, obj) -> None:
        p = self.root / f"{rel}.json"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(obj, indent=1))


def start_stub(spec: dict, root: Path):
    """The scripted model endpoint the scenario names — `vllm` (the parity
    harness's OpenAI chat stub) or `responses` (an OpenAI Responses stub). Returns
    (stub, url): for `vllm` the `/v1` base a vllm_backend record names as its
    `endpoint`; for `responses` the FULL `…/v1/responses` URL, as that backend's
    `endpoint` takes it verbatim."""
    import socket

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    if "responses" in spec:
        from _responses_stub import ResponsesStub  # noqa: E402

        stub = ResponsesStub(port, spec["responses"]["chain"])
        stub.start()
        return stub, stub.url()

    if "vllm" not in spec:
        raise SystemExit(f"stub: name `vllm` or `responses`, got {sorted(spec)}")
    sys.path.insert(0, str(E2E / "parity"))
    from stubs.openai_chat import Handler, OpenAIChatStub  # noqa: E402

    stub = OpenAIChatStub(_Rec(root), "stub", port)
    stub.set_chain("scenario", spec["vllm"]["chain"])
    stub.start(Handler)
    return stub, stub.base_url()


def rows(store: Path) -> int:
    """Rows in the memory store's file (`memory.db`), every owner."""
    import sqlite3

    f = store / "memory.db"
    if not f.exists():
        return 0
    db = sqlite3.connect(f"file:{f}?mode=ro", uri=True)
    try:
        return db.execute("SELECT COUNT(*) FROM rows").fetchone()[0]
    finally:
        db.close()


def check_frame(label: str, text: str, want: dict) -> list[str]:
    bad = []
    for word in want.get("contains", []):
        if word not in text:
            bad.append(f"frame {label}: lacks {word!r}")
    for word in want.get("lacks", []):
        if word in text:
            bad.append(f"frame {label}: shows {word!r}")
    if "contains_block_title" in want:
        has = text.count("█") >= 40  # the FANTASTIC block title is hundreds of cells
        if has != want["contains_block_title"]:
            bad.append(f"frame {label}: block title {'missing' if want['contains_block_title'] else 'present'}")
    return bad


def after(run: dict, cwd: Path, out: str, state: dict) -> list[str]:
    exp = run.get("after_exit") or {}
    store = cwd / ".fantastic"
    bad = []
    code = next((ln.split(":", 1)[1].strip() for ln in out.splitlines() if ln.startswith("exit:")), "none")
    for key, want in exp.items():
        if key == "exit_code":
            if code != str(want):
                bad.append(f"exit {code} != {want}")
        elif key == "dir_untouched":
            left = sorted(p.name for p in cwd.iterdir())
            if want and left:
                bad.append(f"the directory was written before any key: {left}")
        elif key == "lock_released":
            if (store / "lock.yaml").exists() == want:
                bad.append("lock.yaml left behind")
        elif key == "files_exist":
            bad += [f"missing {p}" for p in want if not (cwd / p).exists()]
        elif key == "files_absent":
            bad += [f"unexpected {p}" for p in want if (cwd / p).exists()]
        elif key == "rows_grew":
            n = rows(store)
            if want and n <= state.get("rows", 0):
                bad.append(f"memory.db did not grow ({state.get('rows', 0)} → {n})")
        else:
            bad.append(f"unknown after_exit check {key!r}")
    state["rows"] = rows(store)
    return bad


def main(argv: list[str]) -> int:
    import yaml

    if not argv or argv[0].startswith("-"):
        raise SystemExit("usage: tui.py <scenario-id> [--bin PATH]")
    path = _scenario_path(argv[0])
    sc = yaml.safe_load(path.read_text())
    screen = sc.get("screen") or {}

    failures: list[str] = []
    state: dict = {}
    stub = None
    with tempfile.TemporaryDirectory(prefix="e2e-tui-") as tmp:
        binary, rig = resolve(argv[1:], sc.get("features") or [], Path(tmp))
        print(f"{path.parent.name} · {sc.get('title')}\n  binary: {binary}", flush=True)
        subst = {}
        if sc.get("stub"):
            stub, url = start_stub(sc["stub"], Path(tmp))
            subst["{stub_url}"] = url
            print(f"  stub: {url}", flush=True)
        extra_env = {k: str(v) for k, v in (sc.get("env") or {}).items()}
        cwd = Path(tmp) / "work"
        cwd.mkdir()
        for run in sc["runs"]:
            name = run["name"]
            frames_dir = Path(tmp) / f"frames-{name}"
            script = Path(tmp) / f"{name}.script"
            text = run["script"]
            for k, v in subst.items():
                text = text.replace(k, v)
            script.write_text(text)
            env = {**os.environ, **extra_env, "FT_CWD": str(cwd),
                   "FT_ROWS": str(screen.get("rows", 30)), "FT_COLS": str(screen.get("cols", 110))}
            r = subprocess.run([str(rig), str(binary), str(script), str(frames_dir)],
                               env=env, capture_output=True, text=True, timeout=180)
            out = r.stdout + r.stderr
            # E2E_TUI_FRAMES=<dir>: keep each run's frames there (debugging a script)
            if keep := os.environ.get("E2E_TUI_FRAMES"):
                import shutil

                shutil.copytree(frames_dir, Path(keep) / path.parent.name / name, dirs_exist_ok=True)
            bad = []
            for label, want in (run.get("frames") or {}).items():
                hits = sorted(frames_dir.glob(f"*_{label}.txt"))
                if not hits:
                    bad.append(f"no frame {label!r}")
                    continue
                bad += check_frame(label, hits[-1].read_text(), want)
            bad += after(run, cwd, out, state)
            print(f"  {'✓' if not bad else '✕'} {name}" + ("" if not bad else "  — " + " · ".join(bad)), flush=True)
            failures += [f"{name}: {b}" for b in bad]
        if stub is not None:
            n = len(stub.requests)
            print(f"  stub answered {n} request(s)", flush=True)
            if (want := sc["stub"].get("min_requests")) and n < want:
                failures.append(f"stub: {n} request(s) < {want}")
            if (auth := sc["stub"].get("auth")) is not None:
                bad_auth = [r.get("auth") for r in stub.requests if r.get("auth") != auth]
                if bad_auth:
                    failures.append(f"stub: {len(bad_auth)} request(s) without {auth!r}: {bad_auth[:2]}")
            stub.stop()

    if failures:
        print("\nFAILURES:")
        for f in failures:
            print("  · " + f)
    ok = not failures
    verdict(ok, "all runs green" if ok else f"{len(failures)} failure(s)")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
