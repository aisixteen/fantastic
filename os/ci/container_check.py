#!/usr/bin/env python3
"""Smoke-test the fantastic server's image under docker or podman (Phase 12, #158).

    python3 os/ci/container_check.py --engine docker|podman --image TAG \
        --arch amd64|arm64 --out DIR [--step 90] [--stop 30]

It runs the image with its state on named volumes, the port mapped to loopback
only, and checks, in order (the steps are numbered as in the issue):

  1  start: the container runs, and its page answers through the port map. The image's
     HEALTHCHECK passes too: docker waits for `healthy`, podman runs the check once
  2  the kernel answers: `user` forwards a call to the root agent over /user/ws
  3  a canvas page loads: its document and its bundle (boot_check's page check)
  4  a terminal runs a command: a PTY job runs the login shell, which evaluates
     `echo smoke-$((6*7))` and prints `smoke-42` (the typed line does not match).
     The same shell writes a marker under $FANTASTIC_HOME first (the state volume)
  5  stop: SIGTERM, orderly: the container exits 0 within the stop timeout
  6  restart: a new container on the same volumes answers, the agents step 4 made
     are still there (the /work volume), and the marker is still there (the
     $FANTASTIC_HOME volume)

The port map is an identity map: the host port is FANTASTIC_PORT, so the browser's
`localhost:<port>` is the URL a user opens. The server binds 0.0.0.0 (`--host`, which
sets only its listener), and the Host guard judges by its loopback identity, so it
admits `localhost:<port>`. The checks send the Host and Origin a browser at that URL
sends.

Each container's output is kept in --out (`<name>.log`) before it is removed.
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

# the canvas page's routes, as the image serves them (the page's own kernel)
CANVAS_PAGE = "/canvas_page/file/index.html"
CANVAS_BUNDLE = "/canvas_page/file/bundle.min.js"
CANVAS_TITLE = "Aisixteen Fantastic · canvas"
# the root agent's id (the fantastic CLI's kernel_state root): the kernel's own verbs go to it
ROOT = "kernel_state"
TERM_ID = "smoke_term"
SMOKE_LINE = "echo smoke-$((6*7))\n"
SMOKE_OUTPUT = "smoke-42"
# a file the terminal writes under $FANTASTIC_HOME (the state volume), read back after the restart
HOME_MARKER = "smoke-state"
HOME_MARKER_TEXT = "kept"
STATE_DIR = "/home/fantastic/.fantastic"
WORK_DIR = "/work"


# ── pure helpers (unit-tested) ───────────────────────────────────────────────


def authority_for(port: int) -> str:
    """The Host a browser sends at the port map: `localhost:<port>`, a loopback spelling
    the guard admits on the server's port (its loopback identity, `fantastic-web`'s origin.rs)."""
    return f"localhost:{port}"


def origin_for(port: int) -> str:
    """The Origin a browser page sends on its websocket: the page's own origin."""
    return f"http://{authority_for(port)}"


def run_argv(engine: str, image: str, name: str, port: int, state: str, work: str) -> list[str]:
    """`<engine> run`: detached, the port mapped to loopback ONLY (never published on a
    public interface: the canvas has terminals) as an identity map, FANTASTIC_PORT set
    to the same port, and the two state volumes named."""
    if engine not in ("docker", "podman"):
        raise ValueError(f"engine must be docker or podman, got {engine!r}")
    return [
        engine, "run", "--detach", "--name", name,
        "--publish", f"127.0.0.1:{port}:{port}",
        "--env", f"FANTASTIC_PORT={port}",
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


def home_marker_line() -> str:
    """The shell line that writes the marker into the state volume. It names
    $FANTASTIC_HOME, so a shell without that variable writes to / and fails."""
    return f'echo {HOME_MARKER_TEXT} > "$FANTASTIC_HOME/{HOME_MARKER}"\n'


def marker_kept(text: str) -> bool:
    """Whether a read of the marker (`cat`) is the text the shell wrote."""
    return text.strip() == HOME_MARKER_TEXT


def health_ready(status: str) -> bool:
    """Whether docker's `{{.State.Health.Status}}` says the image's HEALTHCHECK passed.
    `unhealthy` is a failure: the check ran and failed. Any other status is still waiting."""
    if status == "unhealthy":
        raise bc.Fail("the image's HEALTHCHECK reports unhealthy")
    return status == "healthy"


def free_port() -> int:
    """A free port on loopback now (the engine maps it; a race is the one-off cost)."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def get_for(run: "Run"):
    """`boot_check.http_get`, dialled at the run's port map with the browser's authority."""
    return functools.partial(bc.http_get, connect=run.port, authority=run.authority)


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
        self.port = free_port()
        self.authority = authority_for(self.port)
        self.origin = origin_for(self.port)

    def start(self) -> None:
        argv = run_argv(self.engine, self.image, self.name, self.port, self.state, self.work)
        bc.log(f"{self.engine} run {self.name} (127.0.0.1:{self.port}, the browser's {self.authority})")
        engine(self.engine, *argv[1:])

    def stop(self, timeout: float) -> float:
        """SIGTERM, then the engine's own wait; the elapsed seconds."""
        t0 = time.monotonic()
        engine(self.engine, "stop", "--time", str(int(timeout)), self.name)
        return time.monotonic() - t0

    def exit_code(self) -> int:
        return parse_exit_code(engine(self.engine, "inspect", "--format", "{{.State.ExitCode}}", self.name).stdout)

    def health_status(self) -> str:
        """docker's health status: starting, healthy or unhealthy ('' with no HEALTHCHECK)."""
        return engine(self.engine, "inspect", "--format", "{{.State.Health.Status}}", self.name).stdout.strip()

    def home_marker(self) -> str:
        """The marker's text as the container sees it now ('' when it is not there)."""
        return engine(self.engine, "exec", self.name, "cat", f"{STATE_DIR}/{HOME_MARKER}", check=False).stdout

    def save_logs(self, out: Path) -> None:
        """`<engine> logs`: the kernel's output, kept beside the checker's log."""
        proc = subprocess.run([self.engine, "logs", self.name], capture_output=True, text=True, errors="replace")
        (out / f"{self.name}.log").write_text(proc.stdout + proc.stderr)

    def remove(self) -> None:
        engine(self.engine, "rm", "--force", self.name, check=False)


# ── the steps ────────────────────────────────────────────────────────────────


def step_start(run: Run, step: float) -> None:
    bc.at("1")
    run.start()
    bc.until(
        None,
        f"GET {CANVAS_PAGE} through the port map (200, the canvas title)",
        lambda: bc.page_served(get_for(run), run.port, CANVAS_PAGE, CANVAS_TITLE),
        step,
    )
    bc.log(f"step 1: the page answers at {run.authority}")
    step_health(run, step)


def step_health(run: Run, step: float) -> None:
    """Step 1, the image's HEALTHCHECK: docker runs it on its interval, so the status
    is waited for; podman runs the check command once, which exits 0 when it passes."""
    if run.engine == "docker":
        bc.until(None, "the image's HEALTHCHECK reports healthy", lambda: health_ready(run.health_status()), step)
    else:
        engine(run.engine, "healthcheck", "run", run.name)
    bc.log(f"step 1: the image's HEALTHCHECK passes under {run.engine}")


def step_kernel(run: Run, step: float) -> str:
    """Step 2: a call to the root answers over /user/ws, through the port map."""
    bc.at("2")

    def reflected():
        try:
            return asyncio.run(
                bc.forward_call(run.port, ROOT, {"type": "reflect"}, connect=run.port, authority=run.authority, origin=run.origin)
            )
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
        lambda: bc.page_served(get_for(run), run.port, CANVAS_BUNDLE, ""),
        step,
    )
    bc.log(f"step 3: the canvas page and its bundle are served ({CANVAS_TITLE!r})")


def step_terminal(run: Run, root: str, step: float) -> str:
    """Step 4: a PTY job runs the login shell; the shell evaluates the line."""
    bc.at("4")

    def send(target: str, payload: dict):
        return asyncio.run(
            bc.forward_call(run.port, target, payload, connect=run.port, authority=run.authority, origin=run.origin)
        )

    def call(target: str, payload: dict):
        try:
            return send(target, payload)
        except bc.ws_errors() as e:
            raise bc.Fail(f"{payload.get('type')} on {target} over /user/ws: {e}") from e

    def output() -> str:
        """The terminal's output so far; a transient websocket error is retried by the wait."""
        try:
            return (send(TERM_ID, {"type": "output", "job_id": job}) or {}).get("output", "")
        except bc.ws_errors() as e:
            bc.retrying("output", e)
            return ""

    def printed():
        text = output()
        return text if terminal_printed(text) else None

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
    # the marker first: the shell runs its lines in order, so `smoke-42` printed after it
    # means the marker is written
    call(TERM_ID, {"type": "write", "job_id": job, "bytes": home_marker_line()})
    call(TERM_ID, {"type": "write", "job_id": job, "bytes": SMOKE_LINE})
    output_text = bc.until(None, f"the terminal prints {SMOKE_OUTPUT!r}", printed, step)
    bc.log(f"step 4: the terminal (job {job}) printed {SMOKE_OUTPUT!r}: {output_text[-80:]!r}")
    return TERM_ID


def step_stop(run: Run, timeout: float) -> None:
    """Step 5: SIGTERM, the kernel drains and exits 0 well inside the timeout."""
    bc.at("5")
    elapsed = run.stop(timeout)
    verdict = stop_verdict(run.exit_code(), elapsed, timeout)
    bc.log(f"step 5: stop: {verdict}")


def step_restart(run2: Run, root: str, term: str, step: float) -> None:
    """Step 6: a new container on the same volumes answers; the agents and the marker are still there."""
    bc.at("6")
    run2.start()
    bc.until(
        None,
        f"GET {CANVAS_PAGE} after the restart (200, the canvas title)",
        lambda: bc.page_served(get_for(run2), run2.port, CANVAS_PAGE, CANVAS_TITLE),
        step,
    )

    def listed():
        try:
            reply = asyncio.run(
                bc.forward_call(run2.port, root, {"type": "list_agents"}, connect=run2.port, authority=run2.authority, origin=run2.origin)
            )
        except bc.ws_errors() as e:
            bc.retrying("list_agents", e)
            return None
        return reply if isinstance(reply, (dict, list)) and term in json.dumps(reply) else None

    bc.until(None, f"{term} listed after the restart (the /work volume kept it)", listed, step)
    bc.until(
        None,
        f"{HOME_MARKER} kept under $FANTASTIC_HOME after the restart (the state volume kept it)",
        lambda: marker_kept(run2.home_marker()),
        step,
    )
    bc.log(f"step 6: the restarted container answers, {term} is still there, and {HOME_MARKER} is kept")


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
        # the kernel's own output first: a failed step is debugged from it
        for r in (first, second):
            r.save_logs(a.out)
            r.remove()
        for v in (state, work):
            engine(a.engine, "volume", "rm", "--force", v, check=False)
    bc.log("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
