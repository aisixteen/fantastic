"""Run an MCP scenario against the `fantastic` binary — a SCRIPTED client, no model.

The peer of `host.py` for the product binary's MCP face. `host.py` drives a
kernel runtime over REST with a model in the loop; this drives `fantastic mcp`
over stdio with every call written in the scenario, so what it measures is the
binary, not a model's judgement.

    uv run python e2e/runners/mcp.py mcp                    # build src/tui, use it
    uv run python e2e/runners/mcp.py mcp --bin /path/fantastic
    FANTASTIC_BIN=/path/fantastic uv run python e2e/runners/mcp.py mcp

THE BINARY IS A PLUG, like the kernel is for `host.py`: `--bin` / `FANTASTIC_BIN`
runs any build. Without one, the runner builds `src/tui` and uses
`src/target/debug/fantastic` — the thing under test, not a guessed default.

NEVER Claude Code. A human plugs the binary into their own client; a test never
automates one (owner rule, TOS). The client below speaks MCP 2025-11-25 over
stdio because that is what Claude Code's stdio client sends — measured, once, by
a human-run capture — and that is the whole of Claude Code's part in this test.
"""

from __future__ import annotations

import json
import re
import os
import secrets
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import verdict  # noqa: E402

# The binary's user-home template store (`user_home_store`) reads FANTASTIC_HOME,
# else ~/.config/fantastic. A test must never read or write the owner's real
# home: every binary this runner launches inherits a throwaway one.
os.environ["FANTASTIC_HOME"] = tempfile.mkdtemp(prefix="fc-e2e-home-")

REPO = E2E.parent
TUI = REPO / "src" / "tui"
DEFAULT_BIN = REPO / "src" / "target" / "debug" / "fantastic"
READ_TIMEOUT = 30.0


# ── the scenario ────────────────────────────────────────────────────────────
def _scenario_path(scenario_id: str) -> Path:
    """Find a scenario by its BARE id inside its numbered `E<nn>_<name>` dir."""
    root = E2E / "tests" / "scenario"
    for d in sorted(root.iterdir()):
        if d.is_dir() and d.name.split("_", 1)[-1] == scenario_id:
            return d / "scenario.yaml"
    raise SystemExit(f"no scenario {scenario_id!r} under {root}")


def load(scenario_id: str, nonce: str) -> dict:
    import yaml

    raw = json.dumps(yaml.safe_load(_scenario_path(scenario_id).read_text()))
    return json.loads(raw.replace("$NONCE", nonce))


# ── the binary ──────────────────────────────────────────────────────────────
def resolve_bin(argv: list[str]) -> Path:
    """`--bin` wins, then `FANTASTIC_BIN`, then a fresh build of `src/tui`."""
    if "--bin" in argv:
        i = argv.index("--bin")
        if i + 1 >= len(argv):
            raise SystemExit("--bin needs a path")
        b = Path(argv[i + 1]).expanduser().resolve()
    elif os.environ.get("FANTASTIC_BIN"):
        b = Path(os.environ["FANTASTIC_BIN"]).expanduser().resolve()
    else:
        print(f"building {TUI} …", flush=True)
        r = subprocess.run(
            ["cargo", "build", "--manifest-path", str(TUI / "Cargo.toml")],
            capture_output=True,
            text=True,
        )
        if r.returncode != 0:
            print(r.stderr[-2000:])
            raise SystemExit("cargo build of src/tui failed")
        b = DEFAULT_BIN
    if not b.is_file() or not os.access(b, os.X_OK):
        raise SystemExit(f"not an executable binary: {b}")
    return b


# ── a stdio MCP client, 2025-11-25 ──────────────────────────────────────────
class Client:
    def __init__(self, binary: Path, argv: list[str], cwd: Path):
        self.proc = subprocess.Popen(
            [str(binary), "mcp", *argv],
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.next_id = 0
        self._stderr: list[str] = []
        threading.Thread(target=self._drain_stderr, daemon=True).start()

    def _drain_stderr(self):
        for line in self.proc.stderr:
            self._stderr.append(line)

    @property
    def stderr(self) -> str:
        return "".join(self._stderr)

    def _write(self, msg: dict):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _read(self) -> dict:
        box: list = []
        t = threading.Thread(target=lambda: box.append(self.proc.stdout.readline()), daemon=True)
        t.start()
        t.join(READ_TIMEOUT)
        if not box or not box[0]:
            raise RuntimeError(f"no reply within {READ_TIMEOUT}s; stderr: {self.stderr[-400:]}")
        return json.loads(box[0])

    def call(self, method: str, params: dict | None = None) -> dict:
        self.next_id += 1
        rid = self.next_id
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        while True:
            msg = self._read()
            if msg.get("id") == rid:
                return msg  # notifications for other requests are skipped

    def notify(self, method: str, params: dict | None = None):
        self._write({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def close(self, timeout: float = 30.0) -> int:
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            return -9


# ── expectations ────────────────────────────────────────────────────────────
def dig(obj, path: str):
    for part in path.split("."):
        if isinstance(obj, dict) and part in obj:
            obj = obj[part]
        else:
            return _MISSING
    return obj


_MISSING = object()


def tool_body(reply: dict) -> tuple[str, object, bool]:
    """(raw text, parsed body or None, isError) of a tools/call reply."""
    res = reply.get("result") or {}
    text = "".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
    try:
        body = json.loads(text)
    except Exception:
        body = None
    return text, body, bool(res.get("isError"))


def check(step: dict, reply: dict, state: dict) -> list[str]:
    exp = step.get("expect") or {}
    bad: list[str] = []
    text, body, is_err = tool_body(reply) if "tool" in step else ("", None, False)

    for key, want in exp.items():
        if key == "tool_names":
            got = [t.get("name") for t in (reply.get("result") or {}).get("tools", [])]
            if got != want:
                bad.append(f"tools {got} != {want}")
        elif key == "is_error":
            if is_err != want:
                bad.append(f"isError {is_err} != {want}: {text[:160]}")
        elif key == "text_contains":
            if want not in text:
                bad.append(f"text lacks {want!r}: {text[:160]}")
        elif key == "body_has_keys":
            missing = [k for k in want if not (isinstance(body, dict) and k in body)]
            if missing:
                bad.append(f"body lacks keys {missing}")
        elif key == "item0_verbs_include":
            item = body[0] if isinstance(body, list) and body else {}
            res = item.get("result") if isinstance(item, dict) else None
            verbs = (res or {}).get("verbs", {}) if isinstance(res, dict) else {}
            missing = [v for v in want if v not in verbs]
            if missing:
                bad.append(f"docs verbs lack {missing}: {str(res)[:160]}")
        elif key == "verbs_include":
            verbs = (body or {}).get("verbs", {}) if isinstance(body, dict) else {}
            missing = [v for v in want if v not in verbs]
            if missing:
                bad.append(f"verbs lack {missing}")
        elif key == "capture_root":
            if isinstance(body, dict) and body.get("id"):
                state["root"] = body["id"]
            else:
                bad.append("no root id in the primer")
        elif key.startswith("item0."):
            item = body[0] if isinstance(body, list) and body else {}
            got = dig(item, key[len("item0."):])
            if got != want:
                bad.append(f"{key} {got!r} != {want!r}: {text[:160]}")
        else:
            got = dig(reply, key)
            if want == "present":
                if got is _MISSING:
                    bad.append(f"{key} missing")
            elif got != want:
                bad.append(f"{key} {'missing' if got is _MISSING else repr(got)} != {want!r}")
    return bad


def store_rows(store: Path) -> list[dict]:
    """Every row in the memory store's file (`memory.db`), oldest id first. A
    row the store keeps under an agent's own key gets that agent as `agent`;
    the kernel's execution rows (call / settle / task) already carry theirs."""
    import sqlite3

    f = store / "memory.db"
    if not f.exists():
        return []
    db = sqlite3.connect(f"file:{f}?mode=ro", uri=True)
    try:
        out = []
        for owner, doc in db.execute("SELECT owner, doc FROM rows"):
            r = json.loads(doc)
            r.setdefault("agent", owner.split("|", 1)[-1])
            out.append(r)
    finally:
        db.close()
    return sorted(out, key=lambda r: r.get("id", 0))


def row_count(store: Path, agent: str | None = None) -> int:
    return sum(1 for r in store_rows(store) if agent is None or r.get("agent") == agent)


def row_has(row: dict, want: dict) -> bool:
    """Every dotted key in `want` equals the row's value there."""
    for key, v in want.items():
        cur = row
        for part in key.split("."):
            cur = cur.get(part) if isinstance(cur, dict) else None
        if cur != v:
            return False
    return True


def after(run: dict, cwd: Path, code: int, client: Client, state: dict) -> list[str]:
    exp = run.get("after_exit") or {}
    store = cwd / ".fantastic"
    bad: list[str] = []
    for key, want in exp.items():
        if key == "exit_code":
            if code != want:
                bad.append(f"exit {code} != {want}; stderr: {client.stderr[-300:]}")
        elif key == "lock_released":
            if (store / "lock.yaml").exists() == want:
                bad.append("lock.yaml left behind" if want else "lock.yaml missing")
        elif key == "files_exist":
            bad += [f"missing {p}" for p in want if not (cwd / p).exists()]
        elif key == "files_absent":
            bad += [f"unexpected {p}" for p in want if (cwd / p).exists()]
        elif key == "store_has":
            # THE MEMORY STORE: each spec must match at least one row (dotted keys).
            rows = store_rows(store)
            bad += [f"memory.db has no row matching {w}" for w in want if not any(row_has(r, w) for r in rows)]
        elif key == "record":
            for rel, fields in want.items():
                f = cwd / rel
                if not f.exists():
                    bad.append(f"missing record {rel}")
                    continue
                import yaml

                rec = yaml.safe_load(f.read_text())
                for fk, fv in fields.items():
                    if fk.endswith("_has"):
                        m = rec.get(fk[: -len("_has")]) or {}
                        miss = [x for x in fv if x not in m]
                        if miss:
                            bad.append(f"{rel}: {fk[:-4]} lacks {miss}")
                    elif rec.get(fk) != fv:
                        bad.append(f"{rel}: {fk} {rec.get(fk)!r} != {fv!r}")
        elif key == "rows_by":
            n = row_count(store, want["agent"])
            if n < want["at_least"]:
                bad.append(f"rows by {want['agent']}: {n} < {want['at_least']}")
        elif key == "rows_grew":
            n = row_count(store)
            if want and n <= state.get("rows", 0):
                bad.append(f"memory.db did not grow ({state.get('rows', 0)} → {n})")
        elif key == "mcp_records":
            n = len(list(store.rglob("agents/mcp/agent.yaml")))
            if n != want:
                bad.append(f"{n} mcp records, want {want}")
        elif key == "stderr_contains":
            if want not in client.stderr:
                bad.append(f"stderr lacks {want!r}: {client.stderr[-300:]}")
        elif key == "lock_held_by_host":
            pass  # checked while the host still runs
        elif key == "blueprint_never_names":
            bp = (TUI / "blueprint.yaml").read_text()
            if f"id: {want}" in bp:
                bad.append(f"blueprint.yaml names {want}")
        else:
            bad.append(f"unknown after_exit check {key!r}")
    state["rows"] = row_count(store)
    return bad


# ── main ────────────────────────────────────────────────────────────────────
def main(argv: list[str]) -> int:
    if not argv or argv[0].startswith("-"):
        raise SystemExit("usage: mcp.py <scenario-id> [--bin PATH]")
    scenario_id = argv[0]
    binary = resolve_bin(argv[1:])
    nonce = secrets.token_hex(6)
    sc = load(scenario_id, nonce)
    print(f"E15 · {sc.get('title', scenario_id)}\n  binary: {binary}", flush=True)

    failures: list[str] = []
    state: dict = {}
    with tempfile.TemporaryDirectory(prefix="e2e-mcp-") as tmp:
        cwd = Path(tmp)
        for name, body in (sc.get("fixture") or {}).items():
            (cwd / name).write_text(body)

        for run in sc["runs"]:
            print(f"\n── run: {run['name']}  argv={run.get('argv', [])}", flush=True)
            # A HOST: a kernel already running in the directory (`fantastic
            # <host_argv>`), which `fantastic mcp` must attach to.
            host = None
            if "host_argv" in run:
                host = subprocess.Popen([str(binary), *run["host_argv"]], cwd=cwd,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                sock = cwd / ".fantastic" / "cli" / "mcp.sock"
                for _ in range(300):
                    if sock.exists() or host.poll() is not None:
                        break
                    time.sleep(0.1)
                if not sock.exists():
                    failures.append(f"{run['name']}/host: no {sock.relative_to(cwd)} (host exit {host.poll()})")
            client = Client(binary, run.get("argv", []), cwd)
            for i, step in enumerate(run.get("steps", [])):
                label = step.get("call") or step.get("notify") or f"tool {step.get('tool')}"
                try:
                    if "notify" in step:
                        client.notify(step["notify"], step.get("params"))
                        print(f"  · {label}")
                        continue
                    if "tool" in step:
                        reply = client.call("tools/call", {"name": step["tool"], "arguments": step.get("args", {})})
                    else:
                        reply = client.call(step["call"], step.get("params"))
                    bad = check(step, reply, state)
                except Exception as e:  # a dead process is a failure, not a crash of the runner
                    bad = [f"{type(e).__name__}: {e}"]
                mark = "✓" if not bad else "✕"
                print(f"  {mark} {i:02d} {label}" + ("" if not bad else "  — " + " · ".join(bad)), flush=True)
                failures += [f"{run['name']}/{i:02d} {label}: {b}" for b in bad]
            code = client.close()
            if host is not None:
                # the lock is still the host's: the client ran no kernel
                lock = yaml.safe_load((cwd / ".fantastic" / "lock.yaml").read_text()) or {}
                if run.get("after_exit", {}).get("lock_held_by_host") and lock.get("pid") != host.pid:
                    failures.append(f"{run['name']}/after: lock pid {lock.get('pid')} != host {host.pid}")
                host.send_signal(signal.SIGTERM)
                try:
                    hc = host.wait(30)
                except subprocess.TimeoutExpired:
                    host.kill()
                    hc = -9
                if hc != 0:
                    failures.append(f"{run['name']}/host: exit {hc}")
            bad = after(run, cwd, code, client, state)
            print(f"  {'✓' if not bad else '✕'} after exit (code {code})" + ("" if not bad else "  — " + " · ".join(bad)), flush=True)
            failures += [f"{run['name']}/after: {b}" for b in bad]

    if failures:
        print("\nFAILURES:")
        for f in failures:
            print("  · " + f)
    return 0 if verdict(not failures, f"{len(failures)} failure(s)" if failures else "all runs green") else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
