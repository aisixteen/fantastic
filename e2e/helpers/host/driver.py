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

import json
import os
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

BASE = {"$self": "*", "$parent": "*", "$children": "*"}
#: PRIVILEGED verbs lose their wildcard cover — named explicitly, as everywhere.
ROOT_VERBS = ["*", "create_agent", "update_agent", "delete_agent"]

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
#: `root_id` differs by runtime and is NOT cosmetic — python's root is the
#: persistence loader (`kernel_state`), the compiled kernels' is `core`. The
#: driver seeds and grants against whatever the row names.
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
        "root_id": "core",
        "build": ["cargo", "build", "--bin", "fantastic_kernel",
                  "--features", "scenario-probes"],
        "build_cwd": "src/lib/rust",
    },
    "swift": {
        "cmd": ["src/lib/swift/.build/debug/fantastic_kernel"],
        "root_id": "core",
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
    """Make the kernel runnable WITH its scenario fixtures, or fail loudly.

    A stale binary without the probes reports `cannot play ['dispenser']` — a
    skip that reads like a design boundary when it is really a build flag. Doing
    the build here means the skip only ever means what it says. An interpreted
    runtime has nothing to build; its fixtures ride the env instead.
    """
    _, spec = runtime(repo, name)
    # ALREADY BUILT BY THE LAUNCHER. `run.py` builds each kernel once before it
    # dispatches any cell; without this the cells rebuilt concurrently and
    # serialised on the toolchain's build lock (swift: 4-6s per cell for a
    # NO-OP build, ~11s when the tree changed), time that was then charged to
    # the test. The binary is still checked below, so a missing one fails loud.
    prebuilt = name in os.environ.get("E2E_PREBUILT", "").split(",")
    if spec.get("build") and not prebuilt:
        r = subprocess.run(spec["build"], cwd=repo / spec["build_cwd"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            raise SystemExit(
                f"building {name} kernel with scenario fixtures failed:\n"
                + (r.stderr or r.stdout)[-800:])
    if not Path(spec["cmd"][0]).exists():
        raise SystemExit(
            f"{name}: no binary at {spec['cmd'][0]}"
            + (" (E2E_PREBUILT names it as already built)" if prebuilt else ""))
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
                                    stderr=subprocess.DEVNULL)
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
                          **{t: "*" for t in self.reach}}}, indent=2))
        # The WS leg — the EVENT face. REST cannot push, so a failed pass (which
        # announces itself as a `status` event on the agent's own stream) is
        # invisible without this; see `helpers/host/ws.py`.
        (w / "agents/web/agents/ws").mkdir(parents=True, exist_ok=True)
        (w / "agents/web/agents/ws/agent.json").write_text(json.dumps(
            {"id": "ws", "handler_module": "web_ws.tools", "parent_id": "web",
             "ingress_rule": "allow_all_ingress",
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          **{t: "*" for t in self.reach}}}, indent=2))
        (w / "agents/web/agents/rest/agent.json").write_text(json.dumps(
            {"id": "rest", "handler_module": "web_rest.tools", "parent_id": "web",
             # SEALED BY DEFAULT: an io leg with no rule denies every inbound
             # call. Swift enforces this on the written tree; rust let the same
             # record through — a parity difference worth knowing.
             "ingress_rule": "allow_all_ingress",
             "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                          **{t: "*" for t in self.reach}}}, indent=2))
        return port

    def boot(self) -> "HostKernel":
        self.port = self.seed()
        self.proc = subprocess.Popen(
            self.cmd, cwd=self.workdir, env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        for _ in range(150):  # wait on the PORT, never on a guessed sleep
            with socket.socket() as s:
                if s.connect_ex(("127.0.0.1", self.port)) == 0:
                    return self
            time.sleep(0.1)
        raise SystemExit(f"kernel did not serve on :{self.port}")

    def close(self) -> None:
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()

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
