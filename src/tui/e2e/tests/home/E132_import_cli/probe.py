"""E132 import_cli — `fantastic import` from a shell (#269).

The real binary, `cwd` the project of a running `fantastic canvas` (the command is attach-only):

a) in another, empty folder (no kernel): exit 1, `no kernel runs in this directory`, and
   nothing started (no `.fantastic` made);
b) no `--parent`: exit 2 and the usage;
c) off a terminal, no `--approve`: stdout has `would create 3 agents:`, `reaches`, `ports:`
   and `no code`; exit 0; the agent count unchanged;
d) on a pty (skipped on Windows), no `--approve`: `approve? [y/N]` is asked; `n` creates
   nothing; `y` prints `imported 3:` with the new ids, and the agents exist;
e) `--bind @memory=memory2 --approve` imports and names its ids; a second run gives the next
   ones; the agents are under the parent;
f) a refusal prints `refused (unbound_ports): … pass bind` and exits 1; a folder with code is
   `refused (code_not_allowed)` until `--allow-code`."""

from __future__ import annotations

import json
import os
import re
import select
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.blueprint import PARENT, PLAIN, STORE, agent_ids, record, world, write_blueprint, write_team  # noqa: E402
from _lib.canvas_live import Rig, verdict  # noqa: E402

BIND = ["--bind", f"@memory={STORE}"]


def cli(rig, *args: str, cwd: Path | None = None) -> subprocess.CompletedProcess:
    """`fantastic import …` with no terminal on any end."""
    return subprocess.run([str(rig.binary), "import", *args], cwd=cwd or rig.proj, env=rig.env(), stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, timeout=60)


def on_terminal(rig, answer: str, *args: str) -> tuple[int, str, bool]:
    """`fantastic import …` on a pseudo-terminal: wait for `approve? [y/N]`, type `answer`.
    Returns (exit code, everything it printed, whether it asked)."""
    import pty

    master, slave = pty.openpty()
    p = subprocess.Popen([str(rig.binary), "import", *args], cwd=rig.proj, env=rig.env(), stdin=slave, stdout=slave, stderr=slave, close_fds=True)
    os.close(slave)
    out, asked, sent = b"", False, False
    end = time.time() + 60
    while time.time() < end:
        ready, _, _ = select.select([master], [], [], 0.3)
        if ready:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break  # the child closed its end
            if not chunk:
                break
            out += chunk
        if b"approve? [y/N]" in out and not sent:
            asked = True
            os.write(master, (answer + "\n").encode())
            sent = True
        if p.poll() is not None and not ready:
            break
    try:
        code = p.wait(30)
    except subprocess.TimeoutExpired:
        p.kill()
        code = -9
    os.close(master)
    return code, out.decode(errors="replace"), asked


def ids_in(stdout: str) -> list[str]:
    """The ids of an `imported N: a, b, c` line."""
    m = re.search(r"^imported \d+: (.+)$", stdout, re.M)
    return [x.strip() for x in m.group(1).split(",")] if m else []


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        # a) no kernel runs here
        nowhere = rig.art / "nowhere"
        nowhere.mkdir()
        p = cli(rig, "bp", "--parent", PARENT, cwd=nowhere)
        info["no_kernel"] = [p.returncode, p.stdout, p.stderr]
        r["a1 no kernel: exit 1 and says so"] = p.returncode == 1 and "no kernel runs in this directory" in p.stderr
        r["a2 it started nothing"] = not (nowhere / ".fantastic").exists()

        rig.start()
        world(rig)
        write_team(rig, "bp")
        before = agent_ids(rig)

        # b) a bad command line
        p = cli(rig, "bp")
        info["bad_line"] = [p.returncode, p.stderr]
        r["b1 no --parent: exit 2 and the usage"] = p.returncode == 2 and "--parent" in p.stderr and "usage: fantastic import" in p.stderr
        r["b2 nothing was created"] = agent_ids(rig) == before

        # c) a preview off a terminal
        p = cli(rig, "bp", "--parent", PARENT, *BIND)
        info["preview"] = [p.returncode, p.stdout, p.stderr]
        r["c1 the preview exits 0"] = p.returncode == 0
        r["c2 it lists the agents, what they reach, the ports and the code"] = (
            "would create 3 agents:" in p.stdout and "reaches" in p.stdout and f"ports: @memory = {STORE}" in p.stdout and "no code" in p.stdout)
        r["c3 it asked nothing and created nothing"] = "approve?" not in p.stdout and agent_ids(rig) == before

        # d) on a terminal: the question
        if os.name == "nt":
            r["d skipped: no pty on Windows"] = True
            info["d"] = "skipped: a pseudo-terminal needs a POSIX system"
        else:
            code, out, asked = on_terminal(rig, "n", "bp", "--parent", PARENT, *BIND)
            info["pty_no"] = [code, out]
            r["d1 on a terminal it asks approve? [y/N]"] = asked and "would create 3 agents:" in out
            r["d2 n exits 0 and creates nothing"] = code == 0 and "imported" not in out and agent_ids(rig) == before
            code, out, asked = on_terminal(rig, "y", "bp", "--parent", PARENT, *BIND)
            info["pty_yes"] = [code, out]
            got = ids_in(out.replace("\r", ""))
            r["d3 y imports and prints the new ids"] = asked and code == 0 and len(got) == 3 and set(got) <= set(agent_ids(rig))
            r["d4 the ids are fresh _1 copies under the parent"] = set(got) == {"squad_1", "lead_1", "scout_1"} and record(rig, "squad_1").get("parent_id") == PARENT

        # e) --bind … --approve
        n = len(agent_ids(rig))
        p = cli(rig, "bp", "--parent", PARENT, *BIND, "--approve")
        info["approved"] = [p.returncode, p.stdout, p.stderr]
        got = ids_in(p.stdout)
        r["e1 --approve imports and names the ids"] = p.returncode == 0 and len(got) == 3 and p.stdout.startswith("imported 3:")
        r["e2 the agents exist under the parent"] = len(agent_ids(rig)) == n + 3 and set(got) <= set(agent_ids(rig)) and any(record(rig, i).get("parent_id") == PARENT for i in got)
        p2 = cli(rig, "bp", "--parent", PARENT, *BIND, "--approve")
        got2 = ids_in(p2.stdout)
        info["approved_again"] = [p2.returncode, p2.stdout]
        r["e3 a second run gives other ids"] = p2.returncode == 0 and len(got2) == 3 and not set(got) & set(got2)

        # f) refusals
        n = agent_ids(rig)
        p = cli(rig, "bp", "--parent", PARENT, "--approve")
        info["unbound"] = [p.returncode, p.stdout]
        r["f1 an unbound port: refused, exit 1, pass bind"] = p.returncode == 1 and "refused (unbound_ports):" in p.stdout and "pass bind" in p.stdout
        write_blueprint(rig, "withfile", "agents:\n  - id: x\n    handler_module: ram_store.tools\n", {"x/tools.py": "def hello():\n    return 1\n"})
        p = cli(rig, "withfile", "--parent", PARENT, "--approve")
        info["code"] = [p.returncode, p.stdout]
        r["f2 code without --allow-code: refused, exit 1"] = p.returncode == 1 and "refused (code_not_allowed):" in p.stdout and "allow_code: true" in p.stdout
        r["f3 nothing was created"] = agent_ids(rig) == n
        p = cli(rig, "withfile", "--parent", PARENT, "--allow-code", "--approve")
        info["code_allowed"] = [p.returncode, p.stdout]
        r["f4 --allow-code lets it in"] = p.returncode == 0 and p.stdout.startswith("imported 1:") and "x_1" in agent_ids(rig)
        write_blueprint(rig, "plain", PLAIN)
        p = cli(rig, "plain", "--parent", "no_such_agent", "--approve")
        r["f5 a parent that does not exist: refused, exit 1"] = p.returncode == 1 and "refused (not_found):" in p.stdout
    finally:
        rig.stop()
    rig.summary["checks"], rig.summary["info"] = r, info
    failed = [k for k, v in r.items() if not v]
    print(json.dumps(info, indent=1, default=str)[:4000])
    return verdict(*rig.finish(bool(r) and not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
