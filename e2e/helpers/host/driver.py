"""Drive a kernel over its REST surface, FROM e2e.

The scenario is data (`e2e/tests/scenario/E*/scenario.yaml`); this is the e2e
runner's transport for EVERY host runtime — python, rust and swift alike.

ONE FACE FOR EVERY RUNTIME. There used to be an in-process python runner
sitting beside this one, and it made python look better than the compiled
kernels for a reason that had nothing to do with the kernels: it called
`root.send(...)` directly, so it could name an execution, impersonate the
operator, and register bespoke in-process fixtures. None of that is reachable
over HTTP, so `user_actions` "passed" on python and "skipped" on rust while the
two runs were not the same test. Python is spawned and driven over REST here
like everything else, and a limit found here is a limit for all three.

THE ONE REMAINING LIMIT, stated rather than worked around:

  * NO SENDER IMPERSONATION. `web_rest` carries no "act as", so every send is
    from the surface agent (`rest`) — an AGENT, gated, needing each target
    granted explicitly. There is no wildcard target token, by decision.

Fixtures are NOT bespoke: `memory` is the shipped `text_file` and `compute`
the shipped `exec_bridge` on every runtime. The three stateful roles come from
a TEST-ONLY probe bundle that a default build does not carry — a rust feature,
a swift define, and a `PYTHONPATH` entry. A role with no implementation is
REFUSED by name, never approximated.
"""

from __future__ import annotations

import atexit
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

from . import artifacts

BASE = {"$self": "*", "$parent": "*", "$children": "*"}
#: PRIVILEGED verbs lose their wildcard cover — named explicitly, as everywhere.
#: `rows_read`/`rows_write` are hydration's pair: a probe that FORKS a
#: conversation (E37) needs them over the wire, where the in-process path reaches
#: `root.ctx` directly. Naming them here is the whole point of their being
#: privileged — a driver says yes on purpose, a wildcard never does it by accident.
ROOT_VERBS = [
    "*",
    "create_agent",
    "update_agent",
    "delete_agent",
    "rows_read",
    "rows_write",
]

#: The HTTP header carrying a caller-chosen execution id. The credential
#: already rides `X-Fantastic-Auth`; dispatch metadata belongs on the carrier
#: next to it, leaving the JSON body purely the agent's own verb payload.
EXEC_ID_HEADER = "X-Fantastic-Execution-Id"
# The binary leg — mirror of web_rest. A payload/reply field whose value is raw
# bytes rides an octet-stream body; the rest of the envelope is JSON on
# `X-Fantastic-Payload`, the field named by `X-Fantastic-Bytes-Field`. Never base64.
_OCTET = "application/octet-stream"
_PAYLOAD_HEADER = "X-Fantastic-Payload"
_BYTES_FIELD_HEADER = "X-Fantastic-Bytes-Field"


def _bytes_field(d: dict):
    for k, v in d.items():
        if isinstance(v, (bytes, bytearray)):
            return k, bytes(v)
    return None, None

#: The host runtimes this driver can spawn. Adding one is adding a ROW.
#:
#: `root_id` is the same on every runtime now: the root IS the persistence
#: loader, `kernel_state`. Rust and swift called it `core` until the python
#: parity port (`bootstrap.rs DEFAULT_ROOT_ID`, `Kernel.swift ROOT_ID`); the row
#: is what the driver seeds and grants against, so it must name what the binary
#: actually writes into `.fantastic/agent.json` or every grant misses.
RUNTIMES = {
    "python": {
        "cmd": ["src/lib/python/.venv/bin/python", "src/lib/python/main.py"],
        "root_id": "kernel_state",
        # The probe packages are importable ONLY through this entry. A
        # production python kernel never has it set, so `dispenser_probe.tools`
        # does not resolve and the role cannot be played — the same gate the
        # compiled runtimes get from a build flag, in python's own mechanism.
        "env": {"PYTHONPATH": "e2e/probes"},
    },
    "rust": {
        "cmd": ["src/lib/rust/target/debug/fantastic_kernel"],
        "root_id": "kernel_state",
        "build": ["cargo", "build", "--bin", "fantastic_kernel",
                  "--features", "scenario-probes"],
        "build_cwd": "src/lib/rust",
    },
    "swift": {
        "cmd": ["src/lib/swift/.build/debug/fantastic_kernel"],
        "root_id": "kernel_state",
        "build": ["swift", "build", "--product", "fantastic_kernel",
                  "-Xswiftc", "-DSCENARIO_PROBES"],
        "build_cwd": "src/lib/swift",
    },
}


def runtime(repo, name: str | None = None) -> tuple[str, dict]:
    """(name, spec) for the kernel under test — NAMED, never guessed.

    Paths in the row are repo-relative; they come back absolute, because the
    kernel is spawned with its cwd set to a scratch workdir.
    """
    name = name or os.environ.get("KERNEL", "")
    if name not in RUNTIMES:
        raise SystemExit(
            f"KERNEL must be named: {' | '.join(sorted(RUNTIMES))}"
            + (f" (got {name!r})" if name else "")
        )
    spec = dict(RUNTIMES[name])
    spec["cmd"] = [str(repo / c) for c in spec["cmd"]]
    spec["env"] = {k: str(repo / v) for k, v in (spec.get("env") or {}).items()}
    return name, spec


def ensure_runtime(repo, name: str) -> dict:
    """Stage the kernel as a PACKAGED ARTIFACT and return the spec that runs it.

    The build + packaging mirror `.github/workflows/release.yml` step for step —
    same command, same product name, same working directory, same tarball
    layout — with DEBUG in place of `--release`. See `artifacts.py`: running the
    packaged thing is what makes a packaging bug (a bundle the tarball forgot, an
    entry point that only resolved because the source tree was the cwd) fail
    HERE instead of on a user's first boot.

    A stale binary without the probes reports `cannot play ['dispenser']` — a
    skip that reads like a design boundary when it is really a build flag, so
    the staging is done here and the skip only ever means what it says.
    """
    _, spec = runtime(repo, name)
    # ALREADY STAGED BY THE LAUNCHER. `run.py` stages each kernel once before it
    # dispatches any cell; without this the cells rebuilt concurrently and
    # serialised on the toolchain's build lock (swift: 4-6s per cell for a
    # NO-OP build, ~11s when the tree changed), time that was then charged to
    # the test. Staging is idempotent, so this is a stat walk when it is a no-op.
    spec = artifacts.stage(repo, name, spec)
    if not Path(spec["cmd"][0]).exists():
        raise SystemExit(f"{name}: no staged artifact at {spec['cmd'][0]}")
    return spec


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class HostKernel:
    """A spawned kernel, reachable over REST."""

    def __init__(self, spec: dict, workdir: Path, reach: list[str] | None = None):
        """`reach` — every agent id the driver will need to call.

        Granted ON THE TREE, before the daemon starts, which is the only way an
        HTTP driver can hold authority: it arrives as the `rest` agent, and an
        agent may not raise its own. This used to be done by POSTing
        `update_agent` on `rest` after each create — a SELF-GRANT, which python
        refuses outright and which rust and swift were letting through. Their
        conferral check sat behind an early return for kernel verbs at the root,
        so it never ran for `update_agent`; that is fixed in both, and the
        driver no longer asks for something it should never have been given.

        The ids are all known up front — the scenario names its roster — so
        declaring them costs nothing and keeps authority flowing downward.
        """
        self.cmd = spec["cmd"]
        self.root_id = spec["root_id"]
        self.env = {**os.environ, **(spec.get("env") or {})}
        self.workdir = workdir
        self.reach = list(reach or [])
        self.port: int | None = None
        self.proc: subprocess.Popen | None = None
        #: The kernel's own stdout+stderr, on DISK — and NOT under `workdir`.
        #:
        #: It used to go to `subprocess.PIPE` with nobody reading it. A pipe holds
        #: ~64KB; past that the kernel BLOCKS on its next write, mid-request,
        #: forever — no error, no exit, just a process that stops answering. That
        #: is indistinguishable from a model that chose to stop, and it is the one
        #: failure this harness must never confuse with a model's. A file cannot
        #: fill, and it is the only record of what the kernel had to say.
        #:
        #: OUTSIDE THE TREE UNDER TEST. Probes scan the workdir for what the model
        #: wrote; a capture file living there is scanned too. Put in `workdir` on
        #: 2026-09-03, it was immediately reported as the model's stored answer
        #: (`stored='fantastic_kernel: headless host up…' key='kernel.out'`). A
        #: diagnostic that becomes evidence about itself is worse than none.
        self.klog_path = Path(
            tempfile.mkdtemp(prefix="fantastic-kernel-out-")) / "kernel.out"
        self.klog = None

    def seed(self) -> int:
        """Write the faces the driver needs, before the daemon starts.

        The root must name the driver on its INGRESS side explicitly: `rest` is
        a GRANDCHILD (root → web → rest) and `$children` covers direct children
        only — a create_agent succeeded while a reflect was refused until this
        was added.
        """
        port = free_port()
        # Python and rust seed `.fantastic` from a one-shot `reflect`; swift
        # only from the daemon. Checked, not assumed — the binaries differ.
        subprocess.run([*self.cmd, "reflect"], cwd=self.workdir, env=self.env,
                       capture_output=True, check=False)
        w = self.workdir / ".fantastic"
        if not (w / "agent.json").exists():
            seed = subprocess.Popen(self.cmd, cwd=self.workdir, env=self.env,
                                    stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL,
                                    start_new_session=True)
            for _ in range(100):
                if (w / "agent.json").exists():
                    break
                time.sleep(0.1)
            seed.terminate()
            try:
                seed.wait(timeout=10)
            except subprocess.TimeoutExpired:
                seed.kill()
        if not (w / "agent.json").exists():
            raise SystemExit(f"kernel never seeded {w}/agent.json")

        (w / "agents/web/agents/rest").mkdir(parents=True, exist_ok=True)
        root = json.loads((w / "agent.json").read_text())
        root["callable_by"] = {**BASE, "rest": "*"}
        root["can_call"] = {**BASE, "rest": "*"}
        (w / "agent.json").write_text(json.dumps(root, indent=2))
        # `web` NEEDS EGRESS OF ITS OWN. A `watch` frame is dispatched as the
        # WEB agent, not as the ws leg that carried it, so without this every
        # subscription is refused `web may not call "watch" on <id>` — and a
        # watcher that is refused looks exactly like a stream with no events,
        # which is worse than no watcher at all.
        (w / "agents/web/agent.json").write_text(json.dumps(
            {"id": "web", "handler_module": "web.tools",
             "parent_id": self.root_id, "port": port,
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          **{t: "*" for t in self.reach if t != self.root_id}}}, indent=2))
        # The WS leg — the EVENT face. REST cannot push, so a failed pass (which
        # announces itself as a `status` event on the agent's own stream) is
        # invisible without this; see `helpers/host/ws.py`.
        #
        # BOTH HALVES, ON THE LEG'S OWN RECORD. An io leg with no `ingress_rule`
        # is SEALED on every runtime now, and EVERY ws frame — `call`, `watch`,
        # `emit`, `state_subscribe` — is dispatched AS THIS LEG, not as the web
        # host that carries it. So the leg needs the rule (or the upgrade answers
        # and every frame is refused) and a `can_call` naming each id by literal
        # id or `$token`: `"*"` as a grant-map KEY never matches a target.
        (w / "agents/web/agents/ws").mkdir(parents=True, exist_ok=True)
        (w / "agents/web/agents/ws/agent.json").write_text(json.dumps(
            {"id": "ws", "handler_module": "web_ws.tools", "parent_id": "web",
             "ingress_rule": "allow_all_ingress",
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          **{t: "*" for t in self.reach if t != self.root_id}}}, indent=2))
        (w / "agents/web/agents/rest/agent.json").write_text(json.dumps(
            {"id": "rest", "handler_module": "web_rest.tools", "parent_id": "web",
             # SEALED BY DEFAULT: an io leg with no rule denies every inbound
             # call. Swift enforces this on the written tree; rust let the same
             # record through — a parity difference worth knowing.
             "ingress_rule": "allow_all_ingress",
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          **{t: "*" for t in self.reach if t != self.root_id}}}, indent=2))
        return port

    def boot(self) -> "HostKernel":
        # A SIGTERM (run.py killing the lane, a pkill) must run the exit hooks, or the
        # kernel group below outlives the probe; python's default handler skips them.
        if signal.getsignal(signal.SIGTERM) in (signal.SIG_DFL, None):
            signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
        self.port = self.seed()
        self.klog = self.klog_path.open("wb")
        # ITS OWN PROCESS GROUP, KILLED AS A GROUP. A kernel is a daemon by design —
        # it keeps serving when its parent dies — so every probe that ended without
        # reaching `close` (a lane timeout, a pkill, a harness crash) left one behind:
        # 29 orphans, 835 MB, measured 2026-09-07. `close` and the exit hook now kill
        # the group, which also takes any child the kernel spawned.
        self.proc = subprocess.Popen(
            self.cmd, cwd=self.workdir, env=self.env,
            stdout=self.klog, stderr=subprocess.STDOUT,
            start_new_session=True)
        # AT EXIT, NOT AT `close`. Probes bind `close_kernel` and never call it —
        # the kernel dies with the process — so a dump that lived only in `close`
        # never ran and the capture was worth nothing (measured 2026-09-03).
        atexit.register(self.close)
        # WAIT ON THE PORT, never on a guessed sleep — and long enough for a
        # HYDRATED host: booting an agent with a store runs `context_recall`,
        # a model call per AI agent, before the listener comes up (measured
        # 2026-09-12, kernel_restart:rust: 324 rows hydrated, then `did not
        # serve` at the old 15 s cap). 120 s, bounded.
        for _ in range(1200):
            with socket.socket() as s:
                if s.connect_ex(("127.0.0.1", self.port)) == 0:
                    return self
            time.sleep(0.1)
        raise SystemExit(f"kernel did not serve on :{self.port}")

    def close(self) -> None:
        """Stop the kernel's whole process group, then dump. Idempotent — it runs
        from the probe's `finally` and again at exit."""
        if self.proc and self.proc.poll() is None:
            self._signal_group(signal.SIGTERM)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._signal_group(signal.SIGKILL)
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
        self.dump()

    def _signal_group(self, sig: int) -> None:
        try:
            os.killpg(os.getpgid(self.proc.pid), sig)
        except ProcessLookupError:
            pass

    def dump(self) -> None:
        """The kernel's own words, INTO THE TEST LOG.

        Leaving them in a temp dir the run does not keep is the same as not
        capturing them. Idempotent — it runs from `close` and again at exit, and
        the second call has nothing left to say.
        """
        if self.klog is None:
            return
        self.klog.close()
        self.klog = None
        raw = self.klog_path.read_bytes()
        print(f"\n  ── kernel stdout+stderr · {len(raw)} bytes ──", flush=True)
        if raw:
            tail = raw[-4000:].decode("utf-8", "replace")
            print(tail if len(raw) <= 4000 else "…\n" + tail, flush=True)

    def send(self, target: str, payload: dict, timeout: float = 300,
             exec_id: str | None = None) -> dict:
        """One POST per call.

        `exec_id` NAMES the execution, so ensure-semantics are reachable from
        the wire: an unknown id dispatches, an in-flight id joins, and a settled
        id returns the recorded outcome instead of running the verb twice. A
        scenario whose subject is a safe repeat is measurable through HTTP
        because of this header.
        """
        field, blob = _bytes_field(payload)
        if field is not None:
            # Binary leg: the bytes ARE the body; the rest of the envelope is JSON
            # on a header — the same shape web_rest reconstructs.
            rest = {k: v for k, v in payload.items() if k != field}
            headers = {
                "content-type": _OCTET,
                _PAYLOAD_HEADER: json.dumps(rest),
                _BYTES_FIELD_HEADER: field,
            }
            data = blob
        else:
            headers = {"content-type": "application/json"}
            data = json.dumps(payload).encode()
        if exec_id is not None:
            headers[EXEC_ID_HEADER] = exec_id
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}/rest/{target}", data=data, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                raw = r.read()
                if r.headers.get("content-type", "").startswith(_OCTET):
                    # A bytes-carrying reply (e.g. read_stream) comes back the same way.
                    rest = json.loads(r.headers.get(_PAYLOAD_HEADER) or "{}")
                    rest[r.headers.get(_BYTES_FIELD_HEADER) or "bytes"] = raw
                    return rest
                return json.loads(raw or b"null")
        except urllib.error.HTTPError as e:
            return {"error": f"HTTP {e.code}", "body": e.read().decode()[:300]}

    def create(self, **meta) -> dict:
        """Create an agent. Egress to it was granted at SEED time.

        There is no wildcard target, so an id the driver did not declare in
        `reach` is unreachable — correctly. It fails loudly here rather than
        being silently widened after the fact.
        """
        rec = self.send(self.root_id, {"type": "create_agent", **meta})
        if isinstance(rec, dict) and rec.get("id") and rec["id"] not in self.reach:
            raise SystemExit(
                f"created {rec['id']!r} but it is not in the driver's declared "
                f"reach {self.reach} — add it, authority is not self-granted")
        return rec
