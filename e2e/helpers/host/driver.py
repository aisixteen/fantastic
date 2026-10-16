"""Drive a kernel over its REST surface, FROM e2e.

The scenario is data (`e2e/tests/scenario/E*/scenario.yaml`); this is the e2e
runner's transport for EVERY host runtime — python, rust and swift alike.

ONE FACE FOR EVERY RUNTIME. An in-process runner can call `root.send(...)`
directly — name an execution, impersonate the operator, register bespoke
in-process fixtures — none of which is reachable over HTTP, so the same test
would not be the same test across runtimes. Python is spawned and driven over
REST here like everything else, and a limit found here is a limit for all
three.

THE ONE REMAINING LIMIT, stated rather than worked around:

  * NO SENDER IMPERSONATION. `web_rest` carries no "act as", so every send is
    from the surface agent (`rest`) — an AGENT, gated, needing each target
    granted explicitly. There is no wildcard target token, by decision.

Fixtures are NOT bespoke: `memory` is the shipped `file_bridge` and `compute`
the shipped `exec_bridge` on every runtime. The three stateful roles come from
a TEST-ONLY probe bundle that a default build does not carry — a rust feature,
a swift define, and a launcher that registers them (`probes/scenario_probes.py`).
A role with no implementation is REFUSED by name, never approximated.
"""

from __future__ import annotations

import atexit
import json

import yaml
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

from . import artifacts
from helpers import creds, memory, oslayer

#: The only stamped edge is `$self` — every other edge is an explicit grant.
BASE = {"$self": "*"}
#: `*` covers every verb (there is no privileged list).
ROOT_VERBS = "*"

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
#: actually writes into `.fantastic/agent.yaml` or every grant misses.
RUNTIMES = {
    "python": {
        # The probes are REGISTERED (`register_unlisted`) ONLY by this launcher,
        # which then runs `main.py`'s own entry. A production python kernel never
        # runs it, so `dispenser_probe.tools` does not resolve and the role cannot
        # be played — the same gate the compiled runtimes get from a build flag.
        "cmd": [str(oslayer.venv_python(Path("src/lib/python/.venv"))), "e2e/probes/scenario_probes.py"],
        "root_id": "kernel_state",
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

    The build + packaging are the release recipe (`scripts/build/<runtime>.sh`) —
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


#: The record store's id (`seed`).
RECORDS = "records"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])



def _dump(rec: dict) -> str:
    """A record as the kernel writes it (`agent.yaml`): block style, key order kept."""
    return yaml.safe_dump(rec, sort_keys=False, allow_unicode=True, default_flow_style=False)


class HostKernel:
    """A spawned kernel, reachable over REST."""

    def __init__(self, spec: dict, workdir: Path, reach: list[str] | None = None):
        """`reach` — every agent id the driver will need to call.

        Granted ON THE TREE, before the daemon starts, which is the only way an
        HTTP driver can hold authority: it arrives as the `rest` agent, and an
        agent may not raise its own (POSTing `update_agent` on `rest` would be a
        SELF-GRANT, which every runtime refuses).

        The ids are all known up front — the scenario names its roster — so
        declaring them costs nothing and keeps authority flowing downward.
        """
        self.cmd = spec["cmd"]
        self.root_id = spec["root_id"]
        # The tree persists through a RECORD STORE the seed wires (`records`, a
        # file_bridge rooted at `.fantastic`), as a product tree does — so
        # `reopen_kernel` boots the tree the probe stopped.
        self.env = {**os.environ, **(spec.get("env") or {})}
        self.workdir = workdir
        self.reach = list(reach or [])
        self.port: int | None = None
        self.proc: subprocess.Popen | None = None
        #: The kernel's own stdout+stderr, on DISK — and NOT under `workdir`.
        #:
        #: Not `subprocess.PIPE` with nobody reading it: a pipe holds ~64KB; past
        #: that the kernel BLOCKS on its next write, mid-request,
        #: forever — no error, no exit, just a process that stops answering. That
        #: is indistinguishable from a model that chose to stop, and it is the one
        #: failure this harness must never confuse with a model's. A file cannot
        #: fill, and it is the only record of what the kernel had to say.
        #:
        #: OUTSIDE THE TREE UNDER TEST. Probes scan the workdir for what the model
        #: wrote; a capture file living there would be scanned too, and reported as
        #: the model's stored answer. A diagnostic that becomes evidence about itself is worse than none.
        self.klog_path = Path(
            tempfile.mkdtemp(prefix="fantastic-kernel-out-")) / "kernel.out"
        self.klog = None

    def seed(self) -> int:
        """Write the faces the driver needs, before the daemon starts.

        The root must name the driver on its INGRESS side explicitly: `rest` is
        a GRANDCHILD (root → web → rest), and the root grants nothing beyond
        `$self` — a create_agent succeeded while a reflect was refused until this
        was added.
        """
        port = free_port()
        # THE ROOT RECORD is written here: a lockless invocation (`reflect`)
        # writes nothing, and every runtime's root is the same `kernel_state`
        # loader — the kernel mints its guid at boot.
        w = self.workdir / ".fantastic"
        if not (w / "agent.yaml").exists():
            w.mkdir(parents=True, exist_ok=True)
            (w / "agent.yaml").write_text(_dump(
                {"id": self.root_id, "handler_module": "kernel_state.tools"}))

        rest_dir = w / "agents/web/agents/rest"
        rest_dir.mkdir(parents=True, exist_ok=True)
        # A REOPEN (`reopen_kernel`) boots on a tree a kernel already ran: the root
        # and the stores carry the grants the probe wrote at runtime (an AI's
        # ingress on `memory`), so only the FIRST seed writes them.
        if not (rest_dir / "agents" / memory.MEMORY / "agent.yaml").exists():
            root = yaml.safe_load((w / "agent.yaml").read_text())
            # THE `rest` LEG IS THE TEST'S OPERATOR.
            # Every REST call is the leg, and a mutation must come from the
            # operand's direct parent — so every agent a probe creates is the leg's
            # child, and the leg writes their records (grants included). The root
            # accepts the legs' kernel verbs and the web host's reads; nothing is
            # stamped. The root is nobody's child, so only this seed can open it:
            # the legs, the web host, and every declared id (an AI's kernel verbs
            # need it too).
            root["callable_by"] = {
                **BASE, "rest": "*", "ws": "*", "web": "*",
                **{t: "*" for t in self.reach if t != self.root_id},
            }
            root["memory_store_id"] = memory.MEMORY
            (w / "agent.yaml").write_text(_dump(root))
            # THE KERNEL'S MEMORY STORE and THE CREDENTIAL STORE live UNDER THE
            # LEG: an AI agent the leg creates is wired to them by the leg, which
            # may write its own children's `callable_by`. The kernel writes its own
            # rows as bookkeeping.
            (rest_dir / "agents" / memory.MEMORY).mkdir(parents=True, exist_ok=True)
            (rest_dir / "agents" / memory.MEMORY / "agent.yaml").write_text(_dump(
                memory.store_record("rest", callable_by={**BASE, "$parent": "*", "ws": "*", "web": "*"})))
            # THE RECORD STORE — the root loader discovers it (a file_bridge
            # rooted at its own `.fantastic`) and persists every record through it.
            (w / "agents" / RECORDS).mkdir(parents=True, exist_ok=True)
            (w / "agents" / RECORDS / "agent.yaml").write_text(_dump(
                {"id": RECORDS, "handler_module": "file_bridge.tools",
                 "parent_id": self.root_id, "root": ".fantastic", "allow": ["."],
                 "ingress_rule": "allow_all_ingress",
                 "callable_by": {**BASE, "$parent": "*"}}))
            (rest_dir / "agents" / creds.CREDS).mkdir(parents=True, exist_ok=True)
            (rest_dir / "agents" / creds.CREDS / "agent.yaml").write_text(_dump(
                {**creds.store_record("rest", creds.env_refs()), "callable_by": {**BASE, "$parent": "*"}}))
        reach = {t: "*" for t in self.reach if t != self.root_id}
        # `web` reads its legs' routes and serves /file/ AS ITSELF: it reaches the
        # root, its legs and the declared ids; its legs accept their parent.
        (w / "agents/web/agent.yaml").write_text(_dump(
            {"id": "web", "handler_module": "web.tools",
             "parent_id": self.root_id, "port": port,
             # the ws leg watches through its host (forensics, `watch`)
             "callable_by": {**BASE, "ws": "*"},
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          "rest": "*", "ws": "*", memory.MEMORY: "*", **reach}}))
        # The WS leg — the EVENT face. Every ws frame is the LEG: it names each
        # target by literal id (the probe's declared reach, created under `rest`).
        (w / "agents/web/agents/ws").mkdir(parents=True, exist_ok=True)
        (w / "agents/web/agents/ws/agent.yaml").write_text(_dump(
            {"id": "ws", "handler_module": "web_ws.tools", "parent_id": "web",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {**BASE, "$parent": "*"},
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*", "web": "*",
                          memory.MEMORY: "*", **reach}}))
        # The REST leg — the test's operator: the root's kernel verbs, every one of
        # its own children (`$subtree`), the declared ids.
        (rest_dir / "agent.yaml").write_text(_dump(
            {"id": "rest", "handler_module": "web_rest.tools", "parent_id": "web",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {**BASE, "$parent": "*"},
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          "$subtree": "*", **reach}}))
        return port

    def boot(self) -> "HostKernel":
        # A SIGTERM (run.py killing the lane, a pkill) must run the exit hooks, or the
        # kernel group below outlives the probe; python's default handler skips them.
        oslayer.install_stop_handler(lambda *_: sys.exit(143))
        self.port = self.seed()
        self.klog = self.klog_path.open("wb")
        # ITS OWN PROCESS GROUP, KILLED AS A GROUP. A kernel is a daemon by design —
        # it keeps serving when its parent dies — so every probe that ended without
        # reaching `close` (a lane timeout, a pkill, a harness crash) would leave one
        # behind. `close` and the exit hook kill the group, which also takes any child the kernel spawned.
        self.proc = subprocess.Popen(
            self.cmd, cwd=self.workdir, env=self.env,
            stdout=self.klog, stderr=subprocess.STDOUT,
            **oslayer.group_kwargs())
        self._pid = None  # this boot's kernel (see `pid`)
        # AT EXIT, NOT AT `close`. Probes bind `close_kernel` and never call it —
        # the kernel dies with the process — so a dump that lived only in `close`
        # would never run.
        atexit.register(self.close)
        # WAIT ON THE PORT, never on a guessed sleep — and long enough for a
        # HYDRATED host: booting an agent with a store runs `context_recall`,
        # a model call per AI agent, before the listener comes up. 120 s, bounded.
        for _ in range(1200):
            with socket.socket() as s:
                if s.connect_ex(("127.0.0.1", self.port)) == 0:
                    return self
            time.sleep(0.1)
        raise SystemExit(f"kernel did not serve on :{self.port}")

    @property
    def pid(self) -> int:
        """The KERNEL's pid — the program the spawned process runs (on Windows a
        venv's `python.exe` launcher is its parent; `oslayer.program_pid`)."""
        if getattr(self, "_pid", None) is None:
            self._pid = oslayer.program_pid(self.proc)
        return self._pid

    def close(self) -> None:
        """Stop the kernel's whole process group, then dump. Idempotent — it runs
        from the probe's `finally` and again at exit."""
        if self.proc and self.proc.poll() is None:
            oslayer.stop_group(self.proc.pid)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                oslayer.kill_group(self.proc.pid)
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
        self.dump()

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
