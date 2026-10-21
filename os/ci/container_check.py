#!/usr/bin/env python3
"""Smoke-test the fantastic server's image under docker or podman (Phase 12, #158).

    python3 os/ci/container_check.py --engine docker|podman --image TAG \
        --arch amd64|arm64 --out DIR [--step 90] [--stop 30]

It runs the image with its state on named volumes, the port mapped to loopback
only, and checks, in order (the steps are numbered as in the issue):

  1  start: the container runs, and its page answers through the port map
  2  the kernel answers: `user` forwards a call to the root agent over /user/ws
  3  a canvas page loads: its document and its bundle (boot_check's page check)
  4  a terminal runs a command: a PTY job runs the login shell, which evaluates
     `echo smoke-$((6*7))` and prints `smoke-42` (the typed line does not match)
  5  stop: SIGTERM, orderly: the container exits 0 within the stop timeout
  6  restart: a new container on the same volumes answers, and the agents step 4
     made are still there

Steps 3 and 5 reuse `boot_check.py`'s page check and call helpers; the QEMU side is
not used here. Stdlib plus `websockets` (imported by the socket helpers only, so
the pure helpers import anywhere: see test_container_check.py).
"""

from __future__ import annotations

import argparse
import asyncio
import functools
import json
import socket
import subprocess
import sys
import time
from pathlib import Path

import boot_check as bc

# the port the server listens on inside the container (FANTASTIC_PORT's default)
CONTAINER_PORT = 8088
# the authority the server's Host guard accepts: the server binds 0.0.0.0
AUTHORITY = f"0.0.0.0:{CONTAINER_PORT}"
CANVAS_PAGE = "/canvas_page/file/index.html"
CANVAS_BUNDLE = "/canvas_page/file/bundle.min.js"
CANVAS_TITLE = "Aisixteen Fantastic · canvas"
# the root agent's id (the fantastic CLI's kernel_state root): the kernel's own verbs go to it
ROOT = "kernel_state"
TERM_ID = "smoke_term"
SMOKE_LINE = "echo smoke-$((6*7))\n"
SMOKE_OUTPUT = "smoke-42"
STATE_DIR = "/home/fantastic/.fantastic"
WORK_DIR = "/work"


# ── pure helpers (unit-tested) ───────────────────────────────────────────────


def run_argv(engine: str, image: str, name: str, host_port: int, state: str, work: str) -> list[str]:
    """`<engine> run`: detached, the port mapped to loopback ONLY (never published on a
    public interface: the canvas has terminals), the two state volumes named."""
    if engine not in ("docker", "podman"):
        raise ValueError(f"engine must be docker or podman, got {engine!r}")
    return [
        engine, "run", "--detach", "--name", name,
        "--publish", f"127.0.0.1:{host_port}:{CONTAINER_PORT}",
        "--volume", f"{state}:{STATE_DIR}",
        "--volume", f"{work}:{WORK_DIR}",
        image,
    ]  # fmt: skip


def stop_verdict(exit_code: int, elapsed: float, timeout: float) -> str:
    """`stop` is orderly when the container exits 0 before the engine's timeout: a
    SIGKILL is 137, and a stop that ran out the clock was killed, not stopped."""
    if exit_code != 0:
        raise bc.Fail(f"the container exited {exit_code} on stop (0 is an orderly exit; 137 is SIGKILL)")
    if elapsed >= timeout:
        raise bc.Fail(f"the stop took {elapsed:.1f} s, at or past its {timeout:g} s timeout")
    return f"exited 0 after {elapsed:.1f} s"


def parse_exit_code(text: str) -> int:
    """`inspect --format {{.State.ExitCode}}`: one integer."""
    try:
        return int(text.strip())
    except ValueError as e:
        raise bc.Fail(f"the container's exit code is {text.strip()!r}, not a number") from e


def terminal_printed(output: str) -> bool:
    """Whether the shell printed the EXPANSION. The typed line reads `smoke-$((6*7))`,
    so `smoke-42` can only come from the shell having run it."""
    return SMOKE_OUTPUT in output


def free_port() -> int:
    """A free port on loopback now (the engine maps it; a race is the one-off cost)."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def get_for(host_port: int):
    """`boot_check.http_get`, dialled at the port map with the container's own authority."""
    return functools.partial(bc.http_get, connect=host_port, authority=AUTHORITY)


# ── the engine ───────────────────────────────────────────────────────────────


def engine(engine_name: str, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    proc = subprocess.run([engine_name, *args], capture_output=True, text=True)
    if check and proc.returncode != 0:
        raise bc.Fail(f"{engine_name} {' '.join(args)}: exit {proc.returncode}: {proc.stderr.strip()}")
    return proc


class Run:
    """One container of the image: started, reached, stopped, and removed at the end."""

    def __init__(self, engine_name: str, image: str, name: str, state: str, work: str):
        self.engine, self.image, self.name, self.state, self.work = engine_name, image, name, state, work
        self.host_port = free_port()

    def start(self) -> None:
        argv = run_argv(self.engine, self.image, self.name, self.host_port, self.state, self.work)
        bc.log(f"{self.engine} run {self.name} (127.0.0.1:{self.host_port} -> {CONTAINER_PORT})")
        engine(self.engine, *argv[1:])

    def stop(self, timeout: float) -> float:
        """SIGTERM, then the engine's own wait; the elapsed seconds."""
        t0 = time.monotonic()
        engine(self.engine, "stop", "--time", str(int(timeout)), self.name)
        return time.monotonic() - t0

    def exit_code(self) -> int:
        return parse_exit_code(engine(self.engine, "inspect", "--format", "{{.State.ExitCode}}", self.name).stdout)

    def remove(self) -> None:
        engine(self.engine, "rm", "--force", self.name, check=False)


# ── the steps ────────────────────────────────────────────────────────────────


def step_start(run: Run, step: float) -> None:
    bc.at("1")
    run.start()
    bc.until(
        None,
        f"GET {CANVAS_PAGE} through the port map (200, the canvas title)",
        lambda: bc.page_served(get_for(run.host_port), CONTAINER_PORT, CANVAS_PAGE, CANVAS_TITLE),
        step,
    )
    bc.log(f"step 1: the page answers on 127.0.0.1:{run.host_port}")


def step_kernel(run: Run, step: float) -> str:
    """Step 2: a call to the root answers over /user/ws, through the port map."""
    bc.at("2")

    def reflected():
        try:
            return asyncio.run(bc.forward_call(CONTAINER_PORT, ROOT, {"type": "reflect"}, connect=run.host_port, authority=AUTHORITY))
        except bc.ws_errors() as e:
            bc.retrying("reflect", e)
            return None

    reply = bc.until(None, "the root answers reflect over /user/ws", reflected, step)
    if isinstance(reply, dict) and "error" in reply:
        raise bc.Fail(f"reflect on {ROOT} answered an error: {reply['error']!r}")
    bc.log(f"step 2: {ROOT} reflects: {sorted(reply)[:8] if isinstance(reply, dict) else reply!r}")
    return ROOT


def step_canvas(run: Run, step: float) -> None:
    """Step 3: the canvas document and its bundle (boot_check's page check)."""
    bc.at("3")
    bc.until(
        None,
        f"GET {CANVAS_BUNDLE} through the port map (200)",
        lambda: bc.page_served(get_for(run.host_port), CONTAINER_PORT, CANVAS_BUNDLE, ""),
        step,
    )
    bc.log(f"step 3: the canvas page and its bundle are served ({CANVAS_TITLE!r})")


def step_terminal(run: Run, root: str, step: float) -> str:
    """Step 4: a PTY job runs the login shell; the shell evaluates the line."""
    bc.at("4")

    def call(target: str, payload: dict):
        return asyncio.run(bc.forward_call(CONTAINER_PORT, target, payload, connect=run.host_port, authority=AUTHORITY))

    # a terminal sits under `user`, as the app's terminal does (kern/terminal.rs)
    made = call(root, {
        "type": "create_agent", "parent_id": "user", "handler_module": "exec_bridge.tools", "id": TERM_ID,
        "shell": True, "cwd": WORK_DIR, "inherit_env": ["*"], "ingress_rule": "allow_all_ingress",
    })
    if isinstance(made, dict) and "error" in made:
        raise bc.Fail(f"create_agent {TERM_ID} answered {made['error']!r}")
    started = call(TERM_ID, {"type": "start", "pty": {"cols": 100, "rows": 30}})
    job = started.get("job_id") if isinstance(started, dict) else None
    if not isinstance(job, int):
        raise bc.Fail(f"start on {TERM_ID} answered {started!r}, not a job")
    call(TERM_ID, {"type": "write", "job_id": job, "bytes": SMOKE_LINE})
    output = bc.until(
        None,
        f"the terminal prints {SMOKE_OUTPUT!r}",
        lambda: (lambda o: o if terminal_printed(o) else None)(
            (call(TERM_ID, {"type": "output", "job_id": job}) or {}).get("output", "")
        ),
        step,
    )
    bc.log(f"step 4: the terminal (job {job}) printed {SMOKE_OUTPUT!r}: {output[-80:]!r}")
    return TERM_ID


def step_stop(run: Run, timeout: float) -> None:
    """Step 5: SIGTERM, the kernel drains and exits 0 well inside the timeout."""
    bc.at("5")
    elapsed = run.stop(timeout)
    verdict = stop_verdict(run.exit_code(), elapsed, timeout)
    bc.log(f"step 5: stop: {verdict}")


def step_restart(run2: Run, root: str, term: str, step: float) -> None:
    """Step 6: a new container on the same volumes answers; the agents are still there."""
    bc.at("6")
    run2.start()
    bc.until(
        None,
        f"GET {CANVAS_PAGE} after the restart (200, the canvas title)",
        lambda: bc.page_served(get_for(run2.host_port), CONTAINER_PORT, CANVAS_PAGE, CANVAS_TITLE),
        step,
    )

    def listed():
        try:
            reply = asyncio.run(bc.forward_call(CONTAINER_PORT, root, {"type": "list_agents"}, connect=run2.host_port, authority=AUTHORITY))
        except bc.ws_errors() as e:
            bc.retrying("list_agents", e)
            return None
        return reply if isinstance(reply, (dict, list)) and term in json.dumps(reply) else None

    bc.until(None, f"{term} listed after the restart (the state volume kept it)", listed, step)
    bc.log(f"step 6: the restarted container answers, and {term} is still there")


def main() -> int:
    ap = argparse.ArgumentParser(description=(__doc__ or "").split("\n")[0])
    ap.add_argument("--engine", choices=["docker", "podman"], required=True)
    ap.add_argument("--image", required=True)
    ap.add_argument("--arch", choices=["amd64", "arm64"], required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--step", type=float, default=90, help="each wait (s)")
    ap.add_argument("--stop", type=float, default=30, help="the engine's stop timeout (s)")
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    bc._log_file = a.out / f"container_check-{a.engine}-{a.arch}.log"
    tag = f"fantastic-smoke-{a.engine}-{a.arch}-{int(time.time())}"
    state, work = f"{tag}-state", f"{tag}-work"
    first = Run(a.engine, a.image, f"{tag}-1", state, work)
    second = Run(a.engine, a.image, f"{tag}-2", state, work)
    try:
        step_start(first, a.step)
        root = step_kernel(first, a.step)
        step_canvas(first, a.step)
        term = step_terminal(first, root, a.step)
        step_stop(first, a.stop)
        step_restart(second, root, term, a.step)
        second.stop(a.stop)
    except bc.Fail as e:
        bc.log(f"FAIL at step {bc.STEP}: {e}")
        return 1
    except Exception as e:  # a bug here is a failure too
        bc.log(f"FAIL at step {bc.STEP}: {type(e).__name__}: {e}")
        return 1
    finally:
        for r in (first, second):
            r.remove()
        for v in (state, work):
            engine(a.engine, "volume", "rm", "--force", v, check=False)
    bc.log("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
