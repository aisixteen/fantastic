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

A LIVE scenario declares `live: vllm` (and `costs: live`): the shell talks to the
operator's vLLM — `VLLM_ENDPOINT` and `FANTASTIC_AI_MODEL` from the environment
(`{vllm_endpoint}` / `{vllm_model}` in the scenario), the key from
`VLLM_API_KEY` or the file `VLLM_API_KEY_FILE` names, handed to the shell's
environment only (an env-backed credential store reads it) — never printed.
A run may `seed:` host records before it starts (`[{path, record}]`, paths
under the work dir), and set its own `timeout` (seconds; default 180).
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

from e2e_build import build_lock, variant_name  # noqa: E402
from helpers import oslayer, verdict  # noqa: E402

# The binary's user-home template store (`user_home_store`) reads FANTASTIC_HOME,
# else ~/.fantastic. A test must never read or write the owner's real
# home: every binary this runner launches inherits a throwaway one.
_HOME = Path(tempfile.mkdtemp(prefix="fc-e2e-home-"))
oslayer.make_private(_HOME)  # the store refuses a home that is not owner-only
os.environ["FANTASTIC_HOME"] = str(_HOME)

REPO = E2E.parent
TUI = REPO / "src" / "tui"
# e2e builds go to their OWN target dir: the dev binary (`src/target/debug/fantastic`,
# often linked as `~/.local/bin/fantastic`) keeps the features its owner built it with
E2E_TARGET = REPO / "src" / "target" / "e2e"
TARGET = E2E_TARGET / "debug"


def _scenario_path(scenario_id: str) -> Path:
    root = E2E / "tests" / "scenario"
    for d in sorted(root.iterdir()):
        if d.is_dir() and d.name.split("_", 1)[-1] == scenario_id:
            return d / "scenario.yaml"
    raise SystemExit(f"no scenario {scenario_id!r} under {root}")


def build(what: list[str]) -> None:
    r = subprocess.run(["cargo", "build", "--manifest-path", str(TUI / "Cargo.toml"), *what],
                       capture_output=True, text=True, env={**os.environ, "CARGO_TARGET_DIR": str(E2E_TARGET)})
    if r.returncode != 0:
        print(r.stderr[-2000:])
        raise SystemExit(f"cargo build {' '.join(what)} failed")


def resolve(argv: list[str], features: list[str], into: Path) -> tuple[Path, Path]:
    """(the shell binary, the PTY rig), both copied into `into`. The rig is
    always this repo's.

    The shell is built WITH EXACTLY the scenario's `features` (a scenario that
    checks "not in this build" needs a build without that backend): `--bin`
    wins; a prebuilt run (`E2E_ARTIFACTS`) takes that feature set's variant;
    else FANTASTIC_BIN; else it is built here."""
    if "--bin" in argv:
        i = argv.index("--bin")
        b = Path(argv[i + 1]).expanduser().resolve()
    elif os.environ.get("E2E_ARTIFACTS"):
        b = oslayer.exe(Path(os.environ["E2E_ARTIFACTS"]).resolve() / variant_name(features))
    elif os.environ.get("FANTASTIC_BIN"):
        b = Path(os.environ["FANTASTIC_BIN"]).expanduser().resolve()
    else:
        b = None
    # the build AND the copy under one lock: another test's build (other
    # features) cannot land in between
    with build_lock(E2E_TARGET):
        if b is None:
            what = ["--features", ",".join(features)] if features else []
            print(f"building {TUI} {' '.join(what)}…", flush=True)
            build(what)
            b = oslayer.exe(TARGET / "fantastic")
        # the rig: FANTASTIC_TUI_RIG (prebuilt, as FANTASTIC_BIN is), else built here
        if os.environ.get("FANTASTIC_TUI_RIG"):
            rig = Path(os.environ["FANTASTIC_TUI_RIG"]).resolve()
        else:
            build(["--example", "screenshot"])
            rig = oslayer.exe(TARGET / "examples" / "screenshot")
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


def live_env(kind: str) -> tuple[dict, dict]:
    """(substitutions, extra env) for a live scenario — fail loud naming what
    is missing; no endpoint, model or key is ever guessed."""
    if kind != "vllm":
        raise SystemExit(f"live: only `vllm` is wired, got {kind!r}")
    miss = [v for v in ("VLLM_ENDPOINT", "FANTASTIC_AI_MODEL") if not os.environ.get(v)]
    key = os.environ.get("VLLM_API_KEY")
    if not key and (f := os.environ.get("VLLM_API_KEY_FILE")):
        key = Path(f).expanduser().read_text().strip()
    if not key:
        miss.append("VLLM_API_KEY (or VLLM_API_KEY_FILE)")
    if miss:
        raise SystemExit(f"live vllm scenario: set {', '.join(miss)}")
    # the record's sizing is the operator's, from the environment (window, output,
    # the KV table's slots and overflow) — nothing is guessed
    sizing = {"{ctx}": "CTX", "{max_tokens}": "MAX_TOKENS", "{vllm_kv_slots}": "VLLM_KV_SLOTS",
              "{vllm_kv_overflow}": "VLLM_KV_OVERFLOW"}
    miss = [v for v in sizing.values() if not os.environ.get(v)]
    if miss:
        raise SystemExit(f"live vllm scenario: set {', '.join(miss)}")
    # a picker field is driven with ← →: its value's position in the backend's
    # own option order (`kv_overflow`: evict | reject) becomes that many `right`s
    overflow = os.environ["VLLM_KV_OVERFLOW"]
    order = ["evict", "reject"]
    if overflow not in order:
        raise SystemExit(f"VLLM_KV_OVERFLOW must be one of {order}, got {overflow!r}")
    rights = "\n".join(["key right"] * (order.index(overflow) + 1))
    return (
        {"{vllm_endpoint}": os.environ["VLLM_ENDPOINT"], "{vllm_model}": os.environ["FANTASTIC_AI_MODEL"],
         "{vllm_key}": key, "{vllm_kv_overflow_keys}": rights, **{k: os.environ[v] for k, v in sizing.items()}},
        {"VLLM_API_KEY": key},
    )


def seed(cwd: Path, items: list[dict], subst: dict) -> None:
    """Write host records (YAML) under the work dir, substitutions applied."""
    import yaml

    for it in items:
        text = json.dumps(it["record"])
        for k, v in subst.items():
            text = text.replace(k, v)
        p = cwd / it["path"]
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(yaml.safe_dump(json.loads(text), sort_keys=False))


def kernel_rows_on_disk(store: Path) -> int:
    """The KERNEL'S execution rows (call / settle / task / flush) in `memory.db`.
    Its store is RAM (`kernel_memory`): the file holds AI rows
    only, so this must be 0."""
    import sqlite3

    f = store / "memory.db"
    if not f.exists():
        return 0
    db = sqlite3.connect(f"file:{f}?mode=ro", uri=True)
    try:
        return db.execute("SELECT COUNT(*) FROM rows WHERE json_extract(doc, '$.kind') IN ('call', 'settle', 'task', 'flush')").fetchone()[0]
    finally:
        db.close()


def rows_text(store: Path) -> str:
    """Every row's document in the memory store, joined — for text checks."""
    import sqlite3

    f = store / "memory.db"
    if not f.exists():
        return ""
    db = sqlite3.connect(f"file:{f}?mode=ro", uri=True)
    try:
        return "\n".join(r[0] for r in db.execute("SELECT doc FROM rows"))
    finally:
        db.close()


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
        elif key == "file_contains":
            # {path: [text, …]} — each file holds each text (a saved set-up's model)
            for p, words in want.items():
                f = cwd / p
                body = f.read_text() if f.is_file() else ""
                bad += [f"{p} lacks {w!r}" for w in words if w not in body] if body else [f"missing {p}"]
        elif key == "key_stored_is":
            # every api_key in the run's credential file equals $want's value
            # (length + equality only — a key is never printed)
            import yaml

            f = cwd / "home" / "agents" / "ai_agent" / "keys" / "credentials.yaml"
            refs = ((yaml.safe_load(oslayer.read_text(f)) or {}).get("refs") or {}) if f.is_file() else {}
            got = [(e.get("fields") or {}).get("api_key", "") for e in refs.values()]
            want_v = os.environ.get(want, "")
            if not got:
                bad.append(f"no key stored in {f.name}")
            for v in got:
                if v != want_v:
                    bad.append(f"the stored key is not ${want}: {len(v)} chars vs {len(want_v)}")
        elif key == "rows_contain":
            text = rows_text(store)
            bad += [f"no memory row holds {w!r}" for w in want if w not in text]
        elif key == "no_kernel_rows":
            n = kernel_rows_on_disk(store)
            if want and n:
                bad.append(f"memory.db holds {n} kernel execution rows (the kernel's store is RAM)")
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
    # a binary still running (a shell the script could not stop) cannot be deleted
    # on Windows: the verdict is the run's, not the cleanup's
    with tempfile.TemporaryDirectory(prefix="e2e-tui-", ignore_cleanup_errors=True) as tmp:
        binary, rig = resolve(argv[1:], sc.get("features") or [], Path(tmp))
        print(f"{path.parent.name} · {sc.get('title')}\n  binary: {binary}", flush=True)
        subst = {}
        if sc.get("stub"):
            stub, url = start_stub(sc["stub"], Path(tmp))
            subst["{stub_url}"] = url
            print(f"  stub: {url}", flush=True)
        extra_env = {k: str(v) for k, v in (sc.get("env") or {}).items()}
        if sc.get("live"):
            more, env_add = live_env(sc["live"])
            subst.update(more)
            extra_env.update(env_add)
            print(f"  live: {sc['live']} · {os.environ['FANTASTIC_AI_MODEL']}", flush=True)
        cwd = Path(tmp) / "work"
        cwd.mkdir()
        for run in sc["runs"]:
            name = run["name"]
            if run.get("seed"):
                seed(cwd, run["seed"], subst)
            frames_dir = Path(tmp) / f"frames-{name}"
            script = Path(tmp) / f"{name}.script"
            text = run["script"]
            for k, v in subst.items():
                text = text.replace(k, v)
            # `{name}` in the script: the run's `vars`, each this OS's (`for_os`)
            for k, v in (run.get("vars") or {}).items():
                text = text.replace("{" + k + "}", oslayer.for_os(v))
            script.write_text(text)
            env = {**os.environ, **extra_env, "FT_CWD": str(cwd),
                   "FT_ROWS": str(screen.get("rows", 30)), "FT_COLS": str(screen.get("cols", 110))}
            r = subprocess.run([str(rig), str(binary), str(script), str(frames_dir)],
                               env=env, capture_output=True, text=True, timeout=run.get("timeout", 180))
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
        # E2E_TUI_KEEP=<dir>: keep the run's work dir there (debugging a run)
        if keep_dir := os.environ.get("E2E_TUI_KEEP"):
            import shutil as sh

            dst = Path(keep_dir) / path.parent.name
            for sub in ("home", ".fantastic/agents"):
                if (cwd / sub).is_dir():
                    sh.copytree(cwd / sub, dst / sub, dirs_exist_ok=True)
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
