"""The one launcher for every Chrome the e2e harness starts (#97).

A Chrome that never opens its debugging port says nothing about why. So each
launch keeps Chrome's output in a file, and each poll checks the process: a
Chrome that has exited fails at once, with its status and the tail of its
output; a Chrome still running at the deadline fails with the last poll's
error and that same tail. No deadline changes and nothing is retried twice.
"""

from __future__ import annotations

import http.client
import shlex
import subprocess
import time
from pathlib import Path
from typing import Callable, TypeVar

T = TypeVar("T")

#: the lines of Chrome's own output a failure quotes
TAIL = 40
#: what a poll may fail with while Chrome is not up yet: retried until the
#: deadline. Anything else is a bug in the probe and propagates.
RETRYABLE = (OSError, ValueError, KeyError, http.client.HTTPException)


class Chrome:
    """One Chrome the harness started: its process, its command line, its port,
    and the file that holds its stdout and stderr."""

    def __init__(self, argv: list[str], out: Path, port: int):
        self.argv = argv
        self.out = out
        self.port = port
        fh = out.open("wb")
        try:
            self.proc = subprocess.Popen(argv, stdout=fh, stderr=subprocess.STDOUT)
        finally:
            fh.close()  # the child keeps its own copy of the descriptor

    def check(self, last: str) -> None:
        """Fail at once if Chrome has exited."""
        rc = self.proc.poll()
        if rc is not None:
            raise SystemExit(self.evidence(f"Chrome exited with status {rc} before opening its debugging port", last))

    def evidence(self, headline: str, last: str | None = None) -> str:
        """`headline`, how Chrome was launched, the last poll's error, and the tail of its output."""
        try:
            tail = self.out.read_text(errors="replace").splitlines()[-TAIL:]
        except OSError as e:
            tail = [f"(output unreadable: {e})"]
        lines = [headline, f"  command: {shlex.join(self.argv)}"]
        if last is not None:
            lines.append(f"  last poll error: {last}")
        lines.append(f"  output: {self.out} (last {len(tail)} lines)")
        lines.extend(f"    {line}" for line in tail)
        return "\n".join(lines)

    def wait_port(self, probe: Callable[[], T], *, timeout: float, interval: float = 0.2) -> T:
        """`probe()` until it answers. Chrome is checked on every poll."""
        deadline = time.time() + timeout
        last = "no poll yet"
        while True:
            self.check(last)
            try:
                return probe()
            except RETRYABLE as e:
                last = f"{type(e).__name__}: {e}"
            if time.time() > deadline:
                self.check(last)
                raise SystemExit(self.evidence(f"Chrome never opened its debugging port {self.port} in {timeout:.0f} s", last))
            time.sleep(interval)
