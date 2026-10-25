"""cli — the binary itself: one-shots, the daemon, the REPL on a tty, the lock,
argument coercion, and the trees it hydrates from.

This is the only family that does not go through REST: the subject IS the
process — its argv parsing, its exit code, which stream it wrote to, what it
left on disk, and whether it came back from a SIGTERM. Every case gets its own
workdir under `work/cli/<case>` with the SAME name on every runtime, so the diff
lines up case for case.

Nothing is key-sorted (that would lose every key-order item), and the
signals and the 15-second grace are unchanged: python's PTY daemon survives its
SIGTERM (A14.19), and that is a measurement.
"""

from __future__ import annotations

import hashlib
import os
import pty
import shutil
import signal
import socket
import subprocess
import time
from pathlib import Path

from families import save_kernel_log  # noqa: F401 - kept for symmetry of imports
from spec import _dump, free_port, kernel_cmd, kernel_env, read_rec, write_rec

NAME = "cli"

MANIFEST = """agents:
  - id: memory
    handler_module: file_bridge.tools
  - id: notes
    handler_module: file_bridge.tools
    allow: ['.']
    root: notes
    can_call:
      memory: [read]
"""

#: `SCR/rpm_run.py` S1–S7 — malformed and mis-parented trees, hydrated by a
#: one-shot `kernel list_agents`. Each is a question about what a kernel does
#: with a tree it did not write.
def rpm_tree(root_id: str, shape: str, F: Path) -> None:
    def w(p: Path, s: str) -> None:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(s)

    def rec(i: str, parent=None, nop: bool = False) -> str:
        d = {"id": i, "handler_module": "file_bridge.tools"}
        if not nop:
            d["parent_id"] = parent
        return _dump(d)

    def root() -> str:
        return _dump({"id": root_id, "handler_module": "kernel_state.tools", "guid": "g"})

    A = F / "agents"
    if shape in ("S1", "S2", "S3", "S6", "S7"):
        w(F / "agent.yaml", root())
    if shape == "S1":       # a child under an unparseable parent record
        w(A / "p1/agent.yaml", "{NOT_YAML")
        w(A / "p1/agents/c1/agent.yaml", rec("c1", "p1"))
        w(A / "good/agent.yaml", rec("good", root_id))
    if shape == "S2":       # a nested record claiming the ROOT as its parent
        w(A / "a/agent.yaml", rec("a", root_id))
        w(A / "a/agents/b/agent.yaml", rec("b", root_id))
    if shape == "S3":       # a record with NO parent_id at all
        w(A / "x/agent.yaml", rec("x", nop=True))
        w(A / "y/agent.yaml", rec("y", root_id))
    if shape == "S4":       # no root record, one child
        w(A / "good/agent.yaml", rec("good", root_id))
    if shape == "S5":       # an unparseable ROOT record
        w(F / "agent.yaml", "{NOT_YAML")
        w(A / "good/agent.yaml", rec("good", root_id))
    if shape == "S6":       # seven siblings — the ORDER the walk returns them in
        for n in ["zz", "mm", "aa", "kk", "bb", "Ab", "b0"]:
            w(A / f"{n}/agent.yaml", rec(n, root_id))
    if shape == "S7":       # a record-LESS directory that has children
        (A / "p2").mkdir(parents=True, exist_ok=True)
        w(A / "p2/agents/c2/agent.yaml", rec("c2", "p2"))
        w(A / "good/agent.yaml", rec("good", root_id))


def snapshot(wd: Path) -> dict:
    out: dict = {}
    if not wd.exists():
        return out
    for dirpath, dirnames, filenames in os.walk(wd):
        dirnames.sort()
        rel_dir = os.path.relpath(dirpath, wd)
        if rel_dir != ".":
            out[rel_dir + "/"] = {"dir": True}
        for f in sorted(filenames):
            p = Path(dirpath) / f
            rel = os.path.relpath(p, wd)
            try:
                data = p.read_bytes()
            except OSError as e:
                out[rel] = {"error": repr(e)}
                continue
            ent = {"size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
            if len(data) <= 256_000:
                try:
                    ent["text"] = data.decode("utf-8")
                except UnicodeDecodeError:
                    ent["head_hex"] = data[:64].hex()
            out[rel] = ent
    return out


def diff_snap(before: dict, after: dict) -> dict:
    return {
        "created": sorted(k for k in after if k not in before),
        "deleted": sorted(k for k in before if k not in after),
        "modified": sorted(k for k in after if k in before
                           and after[k].get("sha256") != before[k].get("sha256")),
    }


class Reader:
    """Drain one fd into (t, bytes) chunks so a daemon never blocks on a full
    pipe — a kernel blocked mid-write looks exactly like a kernel that chose to
    stop answering."""

    def __init__(self, f) -> None:
        import threading

        self.f, self.chunks = f, []
        self.th = threading.Thread(target=self._run, daemon=True)
        self.th.start()

    def _run(self) -> None:
        try:
            while True:
                b = os.read(self.f.fileno(), 65536)
                if not b:
                    break
                self.chunks.append((time.time(), b))
        except OSError:
            pass

    def split(self, t_mark):
        pre = b"".join(b for t, b in self.chunks if t_mark is None or t < t_mark)
        post = b"".join(b for t, b in self.chunks if t_mark is not None and t >= t_mark)
        return pre, post


class Cli:
    def __init__(self, ctx) -> None:
        self.ctx = ctx
        self.cmd = kernel_cmd(ctx.runtime)
        self.env = kernel_env(ctx.runtime)
        self.live: list = []

    def wd(self, name: str) -> Path:
        wd = self.ctx.work / name
        if wd.exists():
            shutil.rmtree(wd)
        wd.mkdir(parents=True)
        return wd

    def one_shot(self, wd: Path, argv: list, timeout: float = 60, stdin=None) -> dict:
        t0 = time.time()
        p = subprocess.Popen(
            [*self.cmd, *argv], cwd=wd, env=self.env,
            stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        self.live.append(p)
        timed_out = False
        try:
            out, err = p.communicate(input=stdin, timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(os.getpgid(p.pid), signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                out, err = p.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(p.pid), signal.SIGKILL)
                out, err = p.communicate()
        return {"argv": argv, "exit": p.returncode, "timed_out_then_sigterm": timed_out,
                "stdout_bytes": len(out), "stderr_bytes": len(err),
                "stdout": out.decode("utf-8", "replace"), "stderr": err.decode("utf-8", "replace"),
                "_out": out, "_err": err,
                "harness": {"duration_s": round(time.time() - t0, 2)}}

    def kill_all(self) -> None:
        for p in self.live:
            if p.poll() is None:
                try:
                    os.killpg(os.getpgid(p.pid), signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
                try:
                    p.wait(timeout=5)
                except Exception:  # noqa: BLE001
                    pass


class Daemon:
    def __init__(self, cli: Cli, wd: Path, use_pty: bool = False, argv=None) -> None:
        self.cli, self.wd, self.use_pty, self.argv = cli, wd, use_pty, argv or []
        self.t0 = time.time()
        if use_pty:
            master, slave = pty.openpty()
            self.p = subprocess.Popen([*cli.cmd, *self.argv], cwd=wd, env=cli.env,
                                      stdin=slave, stdout=slave, stderr=slave,
                                      start_new_session=True)
            os.close(slave)
            self.master = os.fdopen(master, "rb", buffering=0)
            self.r_out, self.r_err = Reader(self.master), None
        else:
            self.p = subprocess.Popen([*cli.cmd, *self.argv], cwd=wd, env=cli.env,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, start_new_session=True)
            self.r_out, self.r_err = Reader(self.p.stdout), Reader(self.p.stderr)
        cli.live.append(self.p)
        self.t_sigterm = None

    def stop(self, grace: float = 15, sig: int = signal.SIGTERM) -> dict:
        exited_before = self.p.poll() is not None
        killed = False
        if not exited_before:
            self.t_sigterm = time.time()
            try:
                os.kill(self.p.pid, sig)
            except ProcessLookupError:
                pass
            try:
                self.p.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                killed = True
                try:
                    os.killpg(os.getpgid(self.p.pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.p.wait(timeout=5)
        try:
            os.killpg(os.getpgid(self.p.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        time.sleep(0.3)
        pre_o, post_o = self.r_out.split(self.t_sigterm)
        out = {"argv": self.argv, "pty": self.use_pty, "stop_signal": signal.Signals(sig).name,
               "exit": self.p.returncode, "exited_before_sigterm": exited_before,
               "sigkill_needed": killed,
               "harness": {"alive_s_before_sigterm": round((self.t_sigterm or time.time()) - self.t0, 2)},
               ("tty_output_before_sigterm" if self.use_pty else "stdout_before_sigterm"):
                   pre_o.decode("utf-8", "replace"),
               ("tty_output_after_sigterm" if self.use_pty else "stdout_after_sigterm"):
                   post_o.decode("utf-8", "replace")}
        if self.r_err:
            pre_e, post_e = self.r_err.split(self.t_sigterm)
            out["stderr_before_sigterm"] = pre_e.decode("utf-8", "replace")
            out["stderr_after_sigterm"] = post_e.decode("utf-8", "replace")
        if self.use_pty:
            try:
                self.master.close()
            except OSError:
                pass
        return out


def port_open(port: int) -> bool:
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


COERCE = ["handler_module=file_bridge.tools", "id=coerced", "n=3", "neg=-2", "flag=true",
          "off=FALSE", "f=1.5", "exp=1e3", 'obj={"a":1,"b":[2,3]}', 'lst=[1,"x"]',
          "badobj={notjson", "s=hello", "empty=", "eq=a=b", "noeq",
          "big=12345678901234567890",
          # §5.4: the three scalars python's `int()`/`float()` take and JSON does not.
          "big2=123456789012345678901234567890", "inf=inf", "nan=nan"]


def run(ctx) -> None:  # noqa: C901 - a case list is a list
    rec = ctx.rec
    cli = Cli(ctx)
    index: list = []

    def record(name: str, r: dict) -> None:
        out = r.pop("_out", b"")
        err = r.pop("_err", b"")
        rec.cli(f"cli/{name}", r, out, err)
        index.append({"case": name, **{k: r.get(k) for k in ("argv", "exit") if k in r}})

    def simple(name: str, argv: list, pre=None, steps_after=None, stdin=None) -> None:
        wd = cli.wd(name)
        setup = pre(wd) if pre else None
        before = snapshot(wd)
        r = cli.one_shot(wd, argv, stdin=stdin)
        after = snapshot(wd)
        r = {"workdir": str(wd), "setup": setup, **r, "fantastic_before": before,
             "fantastic_after": after, "changes": diff_snap(before, after)}
        steps = []
        for a in (steps_after or []):
            b2 = snapshot(wd)
            s = cli.one_shot(wd, a)
            s.pop("_out", None)
            s.pop("_err", None)
            a2 = snapshot(wd)
            s["changes"] = diff_snap(b2, a2)
            s["fantastic_after"] = a2
            steps.append(s)
        if steps:
            r["steps_after"] = steps
        record(name, r)

    def seed_root(wd: Path) -> dict:
        log = {"reflect": cli.one_shot(wd, ["reflect"], timeout=60)}
        log["reflect"].pop("_out", None)
        log["reflect"].pop("_err", None)
        log["reflect"]["stdout"] = log["reflect"]["stdout"][:400]
        w = wd / ".fantastic"
        if not (w / "agent.yaml").exists():
            d = Daemon(cli, wd)
            for _ in range(100):
                if (w / "agent.yaml").exists():
                    break
                time.sleep(0.1)
            log["daemon_seed"] = d.stop()
        log["root_id"] = read_rec(w / "agent.yaml").get("id")
        return log

    def seed_web(wd: Path):
        log = seed_root(wd)
        port = free_port()
        w = wd / ".fantastic"
        (w / "agents/web").mkdir(parents=True, exist_ok=True)
        write_rec(w / "agents/web/agent.yaml",
                  {"id": "web", "handler_module": "web.tools", "parent_id": log["root_id"],
                   "port": port})
        log["port"] = port
        return port, log

    def write_manifest(wd: Path) -> dict:
        (wd / "x.yaml").write_text(MANIFEST)
        return {"wrote": "x.yaml", "content": MANIFEST}

    try:
        # ── no args: daemon / silent exit ──
        for name, use_pty, web, sig in (
            ("noargs_fresh", False, False, signal.SIGTERM),
            ("noargs_web", False, True, signal.SIGTERM),
            ("noargs_web_sigint", False, True, signal.SIGINT),
            ("noargs_pty_fresh", True, False, signal.SIGTERM),
            ("noargs_pty_web", True, True, signal.SIGTERM),
        ):
            wd = cli.wd(name)
            setup, port = None, None
            if web:
                port, setup = seed_web(wd)
            before = snapshot(wd)
            d = Daemon(cli, wd, use_pty=use_pty)
            t_port = None
            deadline = time.time() + (25 if web else 5)
            while time.time() < deadline:
                if d.p.poll() is not None:
                    break
                if web and port and port_open(port):
                    t_port = round(time.time() - d.t0, 2)
                    break
                time.sleep(0.1)
            time.sleep(3 if web else 0.5)
            during = snapshot(wd)
            r = d.stop(sig=sig)
            after = snapshot(wd)
            r.update({"workdir": str(wd), "setup": setup, "port": port,
                      "fantastic_before": before, "fantastic_while_running": during,
                      "fantastic_after": after,
                      "changes_while_running": diff_snap(before, during),
                      "changes": diff_snap(before, after)})
            r["harness"]["port_open_after_s"] = t_port
            record(name, r)

        # ── the REPL on a tty ──
        wd = cli.wd("repl_pty_lines")
        before = snapshot(wd)
        d = Daemon(cli, wd, use_pty=True)
        time.sleep(4)
        typed = []
        for line in ("list", "@kernel list_agents", "@kernel reflect tree=none", "@ghost reflect",
                     "bogus", "exit"):
            ent = {"harness": {"t": round(time.time() - d.t0, 2)}, "line": line,
                   "process_alive": d.p.poll() is None}
            try:
                os.write(d.master.fileno(), (line + "\n").encode())
            except OSError as e:
                ent["write_error"] = repr(e)
            typed.append(ent)
            time.sleep(1.5)
        for _ in range(100):
            if d.p.poll() is not None:
                break
            time.sleep(0.1)
        r = d.stop()
        r.update({"workdir": str(wd), "typed": typed, "fantastic_before": before,
                  "fantastic_after": snapshot(wd), "changes": diff_snap(before, snapshot(wd))})
        record("repl_pty_lines", r)

        # ── one-shots ──
        simple("reflect", ["reflect"])
        simple("reflect_kernel_ids", ["reflect", "kernel", "tree=ids"])
        simple("reflect_kernel_state_ids", ["reflect", "kernel_state", "tree=ids"])
        simple("reflect_core_ids", ["reflect", "core", "tree=ids"])
        simple("reflect_kv_only", ["reflect", "tree=none", "bundles=ids"])
        simple("reflect_readme", ["reflect", "readme=true", "tree=none"])
        simple("reflect_seeded_twice", ["reflect", "kernel", "tree=ids"], pre=seed_root)
        simple("kernel_list_agents", ["kernel", "list_agents"])
        simple("call_create_coerce_no_parent", ["kernel", "create_agent", *COERCE],
               steps_after=[["reflect", "coerced"], ["kernel", "list_agents"]])
        for parent in ("kernel", "kernel_state", "core"):
            simple(f"call_create_coerce_parent_{parent}",
                   ["kernel", "create_agent", f"parent_id={parent}", *COERCE],
                   steps_after=[["reflect", "coerced"], ["kernel", "list_agents"]])
        simple("call_reflect_kv", ["kernel", "reflect", "tree=none", "bundles=none", "readme=false"])
        simple("call_verb_only", ["kernel_state", "reflect"])
        simple("init_static_loader", ["init", "static_loader", "x.yaml"], pre=write_manifest,
               steps_after=[["kernel", "list_agents"], ["reflect", "static_loader"],
                            ["init", "static_loader", "x.yaml"]])
        simple("init_usage", ["init", "static_loader"])
        simple("init_no_args", ["init"])
        simple("init_missing_manifest", ["init", "static_loader", "missing.yaml"])
        simple("help_word", ["help"])
        simple("help_long", ["--help"])
        simple("help_short", ["-h"])
        simple("help_foo", ["help", "foo"])
        simple("boot_x", ["boot", "x"])
        simple("unknown_positional", ["bogus"])
        simple("unknown_agent_verb", ["ghost", "reflect"])
        simple("unknown_agent_verb_kv", ["ghost", "frobnicate", "k=v"])
        simple("known_agent_unknown_verb", ["kernel", "frobnicate"])
        simple("ledger_cap_flag", ["--ledger-cap", "5", "kernel", "list_agents"])

        def stale_lock(wd: Path) -> dict:
            (wd / ".fantastic").mkdir()
            write_rec(wd / ".fantastic/lock.yaml", {"pid": 999999})
            return {"wrote": ".fantastic/lock.yaml", "content": {"pid": 999999}}

        def zero_lock(wd: Path) -> dict:
            (wd / ".fantastic").mkdir()
            write_rec(wd / ".fantastic/lock.yaml", {"pid": 0})
            return {"wrote": ".fantastic/lock.yaml", "content": {"pid": 0}}

        def dotenv(wd: Path) -> dict:
            # A14.5 / D17(b): autoload exists only in python, and it loads an
            # EXPLICIT operator file — the hermetic rule allows exactly that.
            (wd / ".env").write_text("PARITY_DOTENV_VAR=from-dotenv\n")
            return {"wrote": ".env", "content": "PARITY_DOTENV_VAR=from-dotenv\n"}

        def corrupt_root(wd: Path) -> dict:
            (wd / ".fantastic").mkdir()
            (wd / ".fantastic/agent.yaml").write_text("{NOT_YAML")
            return {"wrote": ".fantastic/agent.yaml", "content": "{NOT_YAML"}

        def corrupt_child(wd: Path) -> dict:
            log = seed_root(wd)
            (wd / ".fantastic/agents/bad").mkdir(parents=True, exist_ok=True)
            (wd / ".fantastic/agents/bad/agent.yaml").write_text("{NOT_YAML")
            return {"seed_root_id": log["root_id"], "wrote": ".fantastic/agents/bad/agent.yaml"}

        def nope_bundle(wd: Path) -> dict:
            log = seed_root(wd)
            (wd / ".fantastic/agents/nope").mkdir(parents=True, exist_ok=True)
            write_rec(wd / ".fantastic/agents/nope/agent.yaml",
                      {"id": "nope", "handler_module": "nope.tools", "parent_id": log["root_id"]})
            return {"seed_root_id": log["root_id"], "handler_module": "nope.tools"}

        def memo_tree(wd: Path) -> dict:
            """`SCR/cli_verify2.py`: a hydrated `file_bridge` agent, then reflect it —
            a record written by hand must come back as the same agent."""
            log = seed_root(wd)
            (wd / ".fantastic/agents/memo").mkdir(parents=True, exist_ok=True)
            write_rec(wd / ".fantastic/agents/memo/agent.yaml",
                      {"id": "memo", "handler_module": "file_bridge.tools",
                       "parent_id": log["root_id"], "description": "hydrated by hand"})
            return {"seed_root_id": log["root_id"]}

        simple("stale_lock_call", ["kernel", "list_agents"], pre=stale_lock)
        simple("zero_lock_call", ["kernel", "list_agents"], pre=zero_lock)
        simple("dotenv_reflect", ["reflect", "tree=none"], pre=dotenv)
        simple("corrupt_root_list", ["kernel", "list_agents"], pre=corrupt_root)
        simple("corrupt_child_list", ["kernel", "list_agents"], pre=corrupt_child)
        simple("nope_bundle_list", ["kernel", "list_agents"], pre=nope_bundle,
               steps_after=[["reflect", "nope"]])
        simple("memo_hydrated_reflect", ["reflect", "memo"], pre=memo_tree,
               steps_after=[["kernel", "list_agents"]])

        # ── the rpm trees: a tree the kernel did not write ──
        for shape in ("S1", "S2", "S3", "S4", "S5", "S6", "S7"):
            def build(wd: Path, shape=shape) -> dict:
                # The root id is the same on every runtime now; the tree names it
                # literally, so a kernel that expects another id says so.
                rpm_tree("kernel_state", shape, wd / ".fantastic")
                return {"shape": shape, "files": sorted(
                    str(p.relative_to(wd)) for p in (wd / ".fantastic").rglob("*") if p.is_file())}

            simple(f"rpm_{shape}_list_agents", ["kernel", "list_agents"], pre=build)

        # `SCR/rpm_daemon.py`: a hand-written store+sload tree, booted as a DAEMON,
        # so the hydration runs in the kernel's own boot rather than a one-shot.
        wd = cli.wd("rpm_daemon_store_sload")
        F = wd / ".fantastic"
        (F / "agents/store").mkdir(parents=True, exist_ok=True)
        (F / "agents/sload").mkdir(parents=True, exist_ok=True)
        write_rec(F / "agent.yaml",
                  {"id": "kernel_state", "handler_module": "kernel_state.tools", "guid": "g"})
        write_rec(F / "agents/store/agent.yaml",
                  {"id": "store", "handler_module": "file_bridge.tools", "allow": ["."],
                   "root": ".fantastic", "parent_id": "kernel_state"})
        write_rec(F / "agents/sload/agent.yaml",
                  {"id": "sload", "handler_module": "static_loader.tools",
                   "parent_id": "kernel_state"})
        before = snapshot(wd)
        d = Daemon(cli, wd)
        time.sleep(5)
        during = snapshot(wd)
        r = d.stop()
        r.update({"workdir": str(wd), "fantastic_before": before,
                  "fantastic_while_running": during, "fantastic_after": snapshot(wd),
                  "changes_while_running": diff_snap(before, during)})
        record("rpm_daemon_store_sload", r)

        # ── lock held: a daemon owns the dir, one-shots run beside it ──
        wd = cli.wd("lock_held")
        port, setup = seed_web(wd)
        write_manifest(wd)
        before = snapshot(wd)
        d = Daemon(cli, wd)
        t_port = None
        for _ in range(250):
            if d.p.poll() is not None:
                break
            if port_open(port):
                t_port = round(time.time() - d.t0, 2)
                break
            time.sleep(0.1)
        time.sleep(2)
        running = snapshot(wd)
        steps = []
        for a in (["kernel", "list_agents"],
                  ["kernel", "create_agent", "handler_module=file_bridge.tools", "id=late"],
                  ["init", "static_loader", "x.yaml"], ["reflect", "kernel", "tree=ids"],
                  ["reflect"], []):
            b2 = snapshot(wd)
            if a:
                s = cli.one_shot(wd, a)
                s.pop("_out", None)
                s.pop("_err", None)
            else:
                d2 = Daemon(cli, wd)
                time.sleep(4)
                s = d2.stop()
            s["changes"] = diff_snap(b2, snapshot(wd))
            steps.append(s)
        r = d.stop()
        r.update({"workdir": str(wd), "setup": setup, "port": port, "steps_while_held": steps,
                  "fantastic_before": before, "fantastic_while_running": running,
                  "fantastic_after": snapshot(wd), "changes": diff_snap(before, snapshot(wd))})
        r["harness"]["port_open_after_s"] = t_port
        record("lock_held", r)
    finally:
        rec.json("cli/_index", index)
        cli.kill_all()
