"""The runtime spec, the scrubbed environment, the fixed workdirs and the one
kernel client every family sends through.

ONE SPEC. The round-1/2 capture scripts each chose their own runtime the way
that suited them — `ensure_runtime` (which REBUILDS when a source file is newer
than the artifact), a hand-written `SPECS` table, a `e2e/dist` path with no
staleness check — so a capture could silently measure a kernel built from a
different tree than the one beside it. Here there is one table, `capture` never
builds, and `run.py preflight` is the only thing allowed to say the artifact is
current.

THE ROOT ID IS MEASURED, NEVER LOOKED UP. `HostKernel` reads it out of the
driver's `RUNTIMES` row; a row is a claim about a binary, and it was wrong for
two runtimes for as long as rust and swift called their root `core`. Here the
kernel seeds `.fantastic/agent.json` first and the id is READ OFF IT, with the
branch that produced the seed recorded in `meta.json`. A capture can then never
grant against an id the binary does not use.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "e2e"))

from helpers.host.driver import (  # noqa: E402
    BASE,
    EXEC_ID_HEADER,
    ROOT_VERBS,
    HostKernel,
    _BYTES_FIELD_HEADER,
    _OCTET,
    _PAYLOAD_HEADER,
    _bytes_field,
    free_port,
)

#: Run output. `e2e/.gitignore` already covers `runs/`.
RUNS = REPO / "e2e/runs/parity"
#: The three fixed sub-trees of a run, identical on every runtime (§5.2).
#: They live under a SLOT — `w/<2 chars>/` — so captures run SIDE BY SIDE without
#: wiping each other's kernel dirs. The slot is always two characters, so every
#: path has the same LENGTH on every capture (path-embedding counts still compare
#: exactly) and `diff.py` folds the two characters to `__`.
SLOTS = RUNS / "w"
WORK = LINK = HOME = TMP = None  # set_slot() — a capture with no slot has no roots


def set_slot(slot: str) -> None:
    global WORK, LINK, HOME, TMP
    if len(slot) != 2 or not slot.isalnum():
        raise SystemExit(f"--slot must be exactly two alphanumerics, got {slot!r}")
    root = SLOTS / slot
    WORK, LINK, HOME, TMP = root / "work", root / "link", root / "home", root / "tmp"

#: Staged artifacts. `capture` NEVER builds — `run.py stage` is the only path.
DIST = REPO / "e2e/dist"

RUNTIMES = ("python", "rust", "swift")


def kernel_cmd(rt: str) -> list[str]:
    """The argv that runs this runtime's kernel.

    python from the SOURCE tree (its venv holds the bundle entry points the
    capture reflects over); rust and swift from the staged release-shaped
    artifact, never from a build tree.
    """
    if rt == "python":
        return [
            str(REPO / "src/lib/python/.venv/bin/python"),
            str(REPO / "src/lib/python/main.py"),
        ]
    if rt in ("rust", "swift"):
        return [str(DIST / rt / "fantastic_kernel")]
    raise SystemExit(f"unknown runtime {rt!r} — one of {', '.join(RUNTIMES)}")


#: Context constants, recorded in meta rather than left to a default that could
#: differ between runtimes. `ctx` uses them; nothing else may.
CTX_WINDOW = 11000
CTX_RESERVE = 1000
CTX_WINDOW_TINY = 2048

#: `ai_paths`' OWN window, deliberately NOT `CTX_WINDOW`. Owner ruling 2026-09-22.
#:
#: `ai_paths` runs 24 scripted cases through ONE ai agent, so its history grows
#: for 74 model turns and it used to cross a 10000-token budget BY ACCIDENT.
#: Measured on the 2026-09-21b captures: the crossing turn landed 1 CHARACTER
#: over on python (`runtime=python` is one byte longer than `runtime=swift`), so
#: python compacted at request 34 and the ports at 35 — and because the recorded
#: event stream is compared positionally, that ONE turn of skew cascaded into
#: ~1100 diff lines on both ports (`ai_paths/events.json $.ai[n]`, and every
#: request body after it).
#:
#: Compaction is NOT this family's subject: `ctx` drives all four strategies
#: deliberately, at `CTX_WINDOW` and at `CTX_WINDOW_TINY`. Here the budget is
#: raised so nothing crosses it by accident, and the ONE crossing the family
#: does record is FORCED by `context_fill` (a scripted filler turn), landing
#: thousands of tokens past the line instead of one character past it.
#:
#: THE NUMBERS, MEASURED on a python capture of this family with these values
#: (2026-09-22). The token series is `ai_core.context.estimate_tokens` recomputed
#: from each recorded request body — system block plus messages:
#:   * budget = 30000 - 1000 = 29000 tokens.
#:   * the scenario's own growth peaks at 12548 tokens (request 61 of 76), i.e.
#:     16452 tokens short of the budget. Nothing crosses it by accident.
#:   * the turn BEFORE the filler lands is 11810 tokens — 17190 short.
#:   * `AI_FILL_CHARS` is the filler ONE scripted reply carries. At 4 chars per
#:     token (`ai_core.context.CHARS_PER_TOKEN`) 80000 chars is ~20000 tokens,
#:     so the turn it lands on is ~31800 — PAST the budget by ~2800 tokens.
#:   * the filler turn is the NEWEST turn, so compaction keeps it and reclaims
#:     the history behind it (124 turns dropped, 7 kept): the projected prompt
#:     lands at 26072 tokens, 2928 UNDER the budget, and the four turns that
#:     follow reach 26033. So the family compacts EXACTLY ONCE — measured, one
#:     `context`/`compacted` event in `events.json` where there were seven.
#: The smallest of those margins is 2810 tokens, ~28x the ~100-token spread
#: between the three runtimes' prompts for the same turn (the `runtime=` word
#: plus the id digit width) — the spread that used to decide this.
AI_WINDOW = 30000
AI_RESERVE = 1000
AI_FILL_CHARS = 80000

#: The stub key every backend reads. Never a real credential: `e2e/backends/
#: connection.local.env` is not sourced anywhere in this harness, and no capture
#: reaches a real endpoint.
STUB_KEY = "parity-stub-key"


def kernel_env(rt: str, extra: dict | None = None, drop: tuple[str, ...] = ()) -> dict:
    """The kernel's environment, BUILT FROM NOTHING.

    Not `os.environ` plus a patch: an acceptance capture that inherits the
    operator's shell measures the operator, and the round-2 captures did exactly
    that (`{**os.environ, **CONN, **SPEC["env"]}`, with `CONN` parsed out of
    `e2e/backends/connection.local.env` — a real key on the kernel's env for a
    run that is supposed to spend nothing).

    Every name here is one a bundle READS: `SHELL` is terminal_backend's
    `_detect_shell`, `TMPDIR` is every mkdtemp, `HOME`/`LANG`/`TERM` are what a
    shell child inherits. `drop` removes a name for the one case whose subject
    is its absence (providers, no key).
    """
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(HOME),
        "SHELL": "/bin/zsh",
        "LANG": "en_US.UTF-8",
        "TERM": "xterm",
        "TMPDIR": str(TMP),
        "ANTHROPIC_KEY": STUB_KEY,
        "NVIDIA_API_KEY": STUB_KEY,
    }
    if rt == "python":
        # The probe bundles are importable ONLY through this entry, exactly as in
        # the driver — the compiled runtimes get the same gate from a build flag.
        env["PYTHONPATH"] = str(REPO / "e2e/probes")
        # Unbuffered, or a SIGKILLed teardown loses the tail of the kernel log,
        # which is where several items are read (A9.5–A9.7, A11.1).
        env["PYTHONUNBUFFERED"] = "1"
    for name in drop:
        env.pop(name, None)
    env.update(extra or {})
    return env


def family_workdir(family: str) -> Path:
    """`(real, link)` for one family — wiped, then a symlink beside it.

    THE KERNEL'S CWD IS THE SYMLINK. Raw vs resolved paths are a measured
    difference (A2.17/A2.20: python answered `/private/var/...` where a port
    answered `/var/...`), so the harness must not resolve the path for them —
    it hands every runtime the same unresolved cwd and lets each answer show
    what it does with it.

    The real directory is the same string on every runtime, so path-embedding
    counts (`bytes`, `size`) compare exactly. The round-1/2 scripts used
    `mkdtemp(prefix=f"pc_{rt}_")`, whose length differs per runtime — every count
    that embedded the path differed for that reason alone.
    """
    real = WORK / family
    link = LINK / family
    if real.exists():
        shutil.rmtree(real)
    real.mkdir(parents=True)
    link.parent.mkdir(parents=True, exist_ok=True)
    if link.is_symlink() or link.exists():
        link.unlink()
    link.symlink_to(real)
    return real, link


def prepare_run_root() -> None:
    for d in (WORK, LINK, HOME, TMP):
        d.mkdir(parents=True, exist_ok=True)


class Ports:
    """The port→role map. Every port a capture binds gets a ROLE, and `diff.py`
    replaces the number by the role (N8) — a port is chosen by the OS and can
    never be compared literally, but *which* role answered is the measurement.
    """

    ROLES = ("main", "web2", "web3", "stub", "stub2", "peer", "relay")

    def __init__(self) -> None:
        #: role → every port taken under it, in order. A LIST, not one value:
        #: `providers` boots a kernel per case and each boot takes a fresh `main`.
        #: A port the map does not know normalises to `<PORT:?>`, which is a diff
        #: line — so every port a family binds has to land here.
        self.by_role: dict[str, list[int]] = {}

    def take(self, role: str) -> int:
        if role not in self.ROLES:
            raise SystemExit(f"port role {role!r} is not in the map {self.ROLES}")
        p = free_port()
        self.by_role.setdefault(role, []).append(p)
        return p

    def note(self, role: str, port: int) -> int:
        self.by_role.setdefault(role, []).append(port)
        return port

    def as_map(self) -> dict:
        return {r: list(v) for r, v in self.by_role.items()}


class CaptureKernel(HostKernel):
    """A spawned kernel with the WHOLE transport recorded.

    Moved from `SCR/parity_capture_store.py:246-279` and given the three things
    every family needs and no round-1/2 script had all of:

      * the root id MEASURED off the seeded tree (see the module docstring);
      * a scrubbed env (`kernel_env`), never the operator's;
      * the FULL reply — status line, every header in order, the raw body — so a
        refusal is data instead of `driver.py:376`'s 300-character head.
    """

    def __init__(
        self,
        rt: str,
        workdir: Path,
        reach: list[str],
        *,
        env_extra: dict | None = None,
        env_drop: tuple[str, ...] = (),
        port: int | None = None,
        seed_extra=None,
    ) -> None:
        spec = {"cmd": kernel_cmd(rt), "root_id": "", "env": {}}
        super().__init__(spec, workdir, reach=reach)
        self.rt = rt
        self.env = kernel_env(rt, env_extra, env_drop)
        self.preset_port = port
        self.seed_extra = seed_extra
        self.seed_branch: str | None = None
        self.http: list[dict] = []
        self.last_http: dict | None = None
        self.record_http = True

    # ── seeding ──────────────────────────────────────────────────────────────
    def seed(self) -> int:
        """Seed the tree, MEASURE the root id, then write the legs against it.

        `HostKernel.seed` writes `web`/`rest`/`ws` using the root id its spec row
        claims. That order cannot work when the id is unknown: the grants have to
        name the id the binary actually wrote.
        """
        port = self.preset_port or free_port()
        w = self.workdir / ".fantastic"
        subprocess.run(
            [*self.cmd, "reflect"],
            cwd=self.workdir,
            env=self.env,
            capture_output=True,
            check=False,
        )
        self.seed_branch = "one_shot_reflect"
        if not (w / "agent.json").exists():
            self.seed_branch = "brief_daemon"
            seed = subprocess.Popen(
                self.cmd,
                cwd=self.workdir,
                env=self.env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
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
            raise SystemExit(f"{self.rt}: kernel never seeded {w}/agent.json")

        root = json.loads((w / "agent.json").read_text())
        measured = root.get("id")
        if not isinstance(measured, str) or not measured:
            raise SystemExit(f"{self.rt}: seeded agent.json carries no id: {root!r}")
        self.root_id = measured

        root["callable_by"] = {**BASE, "rest": "*", "ws": "*"}
        root["can_call"] = {**BASE, "rest": "*"}
        (w / "agent.json").write_text(json.dumps(root, indent=2))

        reach = {t: "*" for t in self.reach if t != self.root_id}
        legs = {
            "agents/web": {
                "id": "web",
                "handler_module": "web.tools",
                "parent_id": self.root_id,
                "port": port,
                "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*", **reach},
            },
            # BOTH HALVES ON THE LEG. An io leg with no `ingress_rule` is SEALED,
            # and every ws frame is dispatched as THIS agent — so the rule and a
            # `can_call` naming each id (literal id or `$token`; `"*"` as a KEY
            # never matches) are what make a watcher anything but a silent stream.
            "agents/web/agents/ws": {
                "id": "ws",
                "handler_module": "web_ws.tools",
                "parent_id": "web",
                "ingress_rule": "allow_all_ingress",
                "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*", **reach},
            },
            "agents/web/agents/rest": {
                "id": "rest",
                "handler_module": "web_rest.tools",
                "parent_id": "web",
                "ingress_rule": "allow_all_ingress",
                "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*", **reach},
            },
        }
        for rel, rec in legs.items():
            d = w / rel
            d.mkdir(parents=True, exist_ok=True)
            (d / "agent.json").write_text(json.dumps(rec, indent=2))
        if self.seed_extra is not None:
            self.seed_extra(self, w, port)
        return port

    # ── transport ────────────────────────────────────────────────────────────
    def send(self, target: str, payload: dict, timeout: float = 120, exec_id=None):
        """One POST, recorded WHOLE.

        The reply this returns is the parsed one families assert on; the raw
        exchange (status, reason, ordered headers, body) lands in `self.http` and
        is written out by `record.py`. Nothing is truncated — `driver.py` cuts an
        error body at 300 characters, which is exactly where a refusal's text
        lives.
        """
        field, blob = _bytes_field(payload)
        if field is not None:
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
        url = f"http://127.0.0.1:{self.port}/rest/{target}"
        rec = {
            "target": target,
            "payload": None if field is not None else payload,
            "payload_header": headers.get(_PAYLOAD_HEADER),
            "bytes_field": field,
            "request_body_len": len(data or b""),
        }
        req = urllib.request.Request(url, data=data, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                raw = r.read()
                rec["status"] = r.status
                rec["reason"] = r.reason
                rec["headers"] = [[k, v] for k, v in r.headers.items()]
                rec["body"] = body_repr(raw)
                if (r.headers.get("content-type") or "").startswith(_OCTET):
                    out = json.loads(r.headers.get(_PAYLOAD_HEADER) or "{}")
                    out[r.headers.get(_BYTES_FIELD_HEADER) or "bytes"] = raw
                    rec["reply_kind"] = "octet"
                else:
                    try:
                        out = json.loads(raw or b"null")
                        rec["reply_kind"] = "json"
                    except ValueError:
                        out = {"__non_json_body__": raw.decode("utf-8", "replace"),
                               "__status__": r.status}
                        rec["reply_kind"] = "non_json"
        except urllib.error.HTTPError as e:
            raw = e.read()
            rec["status"] = e.code
            rec["reason"] = e.reason
            rec["headers"] = [[k, v] for k, v in e.headers.items()]
            rec["body"] = body_repr(raw)
            rec["reply_kind"] = "http_error"
            body = raw.decode("utf-8", "replace")
            try:
                parsed = json.loads(body)
            except ValueError:
                parsed = None
            out = {"__http_status__": e.code, "__body__": parsed if parsed is not None else body}
        self.last_http = rec
        if self.record_http:
            self.http.append(rec)
        return out

    def create(self, **meta) -> dict:
        """Create an agent WITHOUT the driver's reach assertion.

        `HostKernel.create` raises `SystemExit` when the created id is outside
        the declared reach. A capture whose subject IS an id nobody granted
        (`outsider`, `ghost`) must record the reply, not die on it.
        """
        return self.send(self.root_id, {"type": "create_agent", **meta})


def body_repr(raw: bytes) -> dict:
    """A body as DATA: utf-8 text when it decodes, hex when it does not, never a
    lossy `decode(errors='replace')` that makes two different bodies equal."""
    if raw is None:
        return None
    try:
        return {"len": len(raw), "text": raw.decode("utf-8")}
    except UnicodeDecodeError:
        return {"len": len(raw), "hex": raw.hex()}


def wait_port(port: int, secs: float = 25) -> bool:
    end = time.monotonic() + secs
    while time.monotonic() < end:
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.1)
    return False


def leftover_kernels(families: list[str] | None = None) -> list[str]:
    """Any `fantastic_kernel`/`main.py` process whose cwd is under the run work
    tree — every slot (preflight, `families=None`), or only THIS capture's own
    family dirs (other families of the same slot run beside it). Preflight check (c): a leftover kernel holds the lock on the very
    directory the next capture wipes, and the failure it produces reads like a
    seeding bug in the runtime under test."""
    found = []
    try:
        ps = subprocess.run(
            ["/bin/ps", "-Ao", "pid=,command="], capture_output=True, text=True, check=False
        ).stdout
    except OSError:
        return found
    for line in ps.splitlines():
        line = line.strip()
        if not line:
            continue
        pid, _, cmd = line.partition(" ")
        if "fantastic_kernel" not in cmd and "src/lib/python/main.py" not in cmd:
            continue
        try:
            cwd = subprocess.run(
                ["/usr/sbin/lsof", "-a", "-p", pid, "-d", "cwd", "-Fn"],
                capture_output=True, text=True, check=False,
            ).stdout
        except OSError:
            continue
        if families is None:
            roots = [str(SLOTS)]
        else:
            roots = [f"{d}/{f}\n" for d in (WORK, LINK) for f in families]
        if any(r in cwd + "\n" for r in roots):
            found.append(f"pid {pid}: {cmd[:120]}")
    return found
