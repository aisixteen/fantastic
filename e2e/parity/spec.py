"""The runtime spec, the scrubbed environment, the fixed workdirs and the one
kernel client every family sends through.

ONE SPEC. A capture that picks its own runtime (rebuilding on a newer source
file, a hand-written table, a `e2e/dist` path with no staleness check) can
silently measure a kernel built from a different tree than the one beside it.
Here there is one table, `capture` never builds, and `run.py preflight` is the only thing allowed to say the artifact is
current.

THE ROOT ID IS MEASURED, NEVER LOOKED UP. `HostKernel` reads it out of the
driver's `RUNTIMES` row; a row is a claim about a binary, and a claim can go
stale. Here the
kernel seeds `.fantastic/agent.yaml` first and the id is READ OFF IT, with the
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
    _dump,
    free_port,
)
from helpers import creds, memory  # noqa: E402

import yaml  # noqa: E402


def read_rec(path: Path) -> dict:
    """A persisted record (`agent.yaml`), as the kernel wrote it."""
    return yaml.safe_load(path.read_text())


def write_rec(path: Path, rec: dict) -> None:
    """A record as the kernel writes it — block style, key order kept."""
    path.write_text(_dump(rec))

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
    capture reflects over), through the probe launcher — the probes registered,
    then `main.py`'s own entry; rust and swift from the staged release-shaped
    artifact, never from a build tree.
    """
    if rt == "python":
        return [
            str(REPO / "src/lib/python/.venv/bin/python"),
            str(REPO / "e2e/probes/scenario_probes.py"),
        ]
    if rt in ("rust", "swift"):
        return [str(DIST / rt / "fantastic_kernel")]
    raise SystemExit(f"unknown runtime {rt!r} — one of {', '.join(RUNTIMES)}")


#: Context constants, recorded in meta rather than left to a default that could
#: differ between runtimes. `ctx` uses them; nothing else may.
#:
#: 16000: the system block plus the chain's largest live turn (the recall after
#: the context notice) must fit the budget, or every chain dies `too_small`. At
#: 16000 (budget 15000, trigger 12000) all four chains run their turns and
#: compact repeatedly, and `too_small` fires only where it is the case (the
#: 60000-char send, and `ai_tiny`).
CTX_WINDOW = 16000
CTX_RESERVE = 1000
CTX_WINDOW_TINY = 2048

#: `ai_paths`' OWN window, deliberately NOT `CTX_WINDOW`.
#:
#: `ai_paths` runs many scripted cases through ONE ai agent, so its history
#: grows over many model turns. A budget the history crosses BY ACCIDENT can be
#: crossed one character apart on two runtimes (`runtime=python` is one byte
#: longer than `runtime=swift`), and because the recorded event stream is
#: compared positionally, one turn of skew cascades into every later line.
#:
#: Compaction is NOT this family's subject: `ctx` drives all four strategies
#: deliberately, at `CTX_WINDOW` and at `CTX_WINDOW_TINY`. Here the budget is
#: raised so nothing crosses it by accident, and the ONE crossing the family
#: does record is FORCED by `context_fill` (a scripted filler turn), landing
#: thousands of tokens past the line instead of one character past it.
#:
#: THE NUMBERS, on a python capture of this family with these values. The token
#: series is
#: `ai_core.context.estimate_tokens` recomputed from each recorded request body
#: — system block plus messages. The trigger is 0.8 of the budget
#: (`ai_core.constants.TRIGGER`), and nothing reports a prompt size, so the
#: calibration is 1.0:
#:   * budget = 38000 - 1000 = 37000 tokens; trigger = 29600.
#:   * the scenario's own growth peaks at 13468 tokens (request 61 of 77), i.e.
#:     16132 tokens short of the trigger. Nothing crosses it by accident.
#:   * the turn the filler is answered on is 12255 tokens.
#:   * `AI_FILL_CHARS` is the filler ONE scripted reply carries. At 4 chars per
#:     token (`ai_core.context.CHARS_PER_TOKEN`) 80000 chars is ~20000 tokens,
#:     so the turn it lands on is ~32300 — PAST the trigger by ~2700 tokens.
#:   * the filler turn is the NEWEST turn, so compaction keeps it and reclaims
#:     the history behind it: the next three prompts are 27569, 27467 and 27466
#:     tokens, 2031 UNDER the trigger. So the family compacts EXACTLY ONCE — one
#:     `context`/`compacted` event in `events.json`. A smaller window would let
#:     that post-fill prompt re-fire compaction on every turn after the filler.
#: The smallest of those margins is ~2000 tokens, ~20x the ~100-token spread
#: between the three runtimes' prompts for the same turn (the `runtime=` word
#: plus the id digit width).
AI_WINDOW = 38000
AI_RESERVE = 1000
AI_FILL_CHARS = 80000

#: The stub key the seeded `creds` store reads for a keyed case. Never a real
#: credential: `e2e/backends/connection.local.env` is not sourced anywhere in this
#: harness, and no capture reaches a real endpoint.
STUB_KEY = "parity-stub-key"


def kernel_env(rt: str, extra: dict | None = None, drop: tuple[str, ...] = ()) -> dict:
    """The kernel's environment, BUILT FROM NOTHING.

    Not `os.environ` plus a patch: an acceptance capture that inherits the
    operator's shell measures the operator (and can put a real key on the
    kernel's env for a run that is supposed to spend nothing).

    Every name here is one the runtime READS: `TMPDIR` is every mkdtemp,
    `HOME`/`LANG` are the runtime's own home and locale. A child gets none of
    them unless its record names them in `inherit_env`. `drop` removes a name for the one case whose subject
    is its absence (providers, no key).
    """
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(HOME),
        "LANG": "en_US.UTF-8",
        "TMPDIR": str(TMP),
        # Read only by the seeded `creds` store, and only for a ref a boot names
        # (`providers`' keyed vllm cases): no backend reads the environment.
        "VLLM_API_KEY": STUB_KEY,
    }
    if rt == "python":
        # The probe bundles are registered by the launcher `kernel_cmd` runs,
        # exactly as in the driver — the compiled runtimes get the same gate from
        # a build flag. Unbuffered, or a SIGKILLed teardown loses the tail of the
        # kernel log, which is where several items are read (A9.5–A9.7, A11.1).
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
    counts (`bytes`, `size`) compare exactly. A per-runtime temp dir name differs
    in length, and every count that embedded the path would differ for that
    reason alone.
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

    It carries the three things every family needs:

      * the root id MEASURED off the seeded tree (see the module docstring);
      * a scrubbed env (`kernel_env`), never the operator's;
      * the FULL reply — status line, every header in order, the raw body — so a
        refusal is data instead of `driver.py:376`'s 300-character head.
    """

    #: WHERE A FAMILY CREATES ITS AGENTS (minimal model): every
    #: call is the `rest` leg, and a mutation must come from the operand's direct
    #: parent — so the leg's own subtree is the test's world.
    home = "rest"

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
        creds: dict[str, str] | None = None,
    ) -> None:
        spec = {"cmd": kernel_cmd(rt), "root_id": "", "env": {}}
        super().__init__(spec, workdir, reach=reach)
        self.rt = rt
        self.env = kernel_env(rt, env_extra, env_drop)
        self.preset_port = port
        self.seed_extra = seed_extra
        # `{ref: endpoint}` — the keyed stubs this boot's AI agents read a key for.
        self.creds = dict(creds or {})
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
        if not (w / "agent.yaml").exists():
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
                if (w / "agent.yaml").exists():
                    break
                time.sleep(0.1)
            seed.terminate()
            try:
                seed.wait(timeout=10)
            except subprocess.TimeoutExpired:
                seed.kill()
        if not (w / "agent.yaml").exists():
            raise SystemExit(f"{self.rt}: kernel never seeded {w}/agent.yaml")

        root = read_rec(w / "agent.yaml")
        measured = root.get("id")
        if not isinstance(measured, str) or not measured:
            raise SystemExit(f"{self.rt}: seeded agent.yaml carries no id: {root!r}")
        self.root_id = measured

        # THE `rest` LEG IS THE TEST'S OPERATOR — the same seed
        # as `helpers/host/driver.py`. The root is nobody's child, so only this seed
        # opens it: the legs, the web host and every declared id.
        root["callable_by"] = {
            **BASE, "rest": "*", "ws": "*", "web": "*",
            **{t: "*" for t in self.reach if t != self.root_id},
        }
        # THE KERNEL'S MEMORY STORE — `kernel_start` refuses to run without one.
        # One ram_store for the kernel's own rows AND every AI agent's conversation;
        # it and the credential store live UNDER THE LEG, which wires the agents it
        # creates to them.
        root["memory_store_id"] = memory.MEMORY
        write_rec(w / "agent.yaml", root)
        rest_dir = w / "agents/web/agents/rest"
        # Every declared id may reach both stores — an AI agent a family creates
        # reads its conversation from `memory` and its key from `creds` as itself.
        declared = {t: "*" for t in self.reach
                    if t not in (self.root_id, memory.MEMORY, creds.CREDS)}
        (rest_dir / "agents" / memory.MEMORY).mkdir(parents=True, exist_ok=True)
        write_rec(rest_dir / "agents" / memory.MEMORY / "agent.yaml", memory.store_record(
            "rest", callable_by={**BASE, "$parent": "*", "ws": "*", "web": "*", **declared}))
        # THE CREDENTIAL STORE — env-backed (the scrubbed env carries the stub's
        # dummy key), one ref per keyed stub this boot talks to, bound to its URL.
        (rest_dir / "agents" / creds.CREDS).mkdir(parents=True, exist_ok=True)
        write_rec(rest_dir / "agents" / creds.CREDS / "agent.yaml", {
            **creds.store_record(
                "rest", {ref: (creds.KEYED[ref][0], url) for ref, url in self.creds.items()}),
            "callable_by": {**BASE, "$parent": "*", **declared},
        })

        reach = {t: "*" for t in self.reach if t != self.root_id}
        legs = {
            # `web` reads its legs' routes and serves /file/ AS ITSELF; the ws leg
            # watches through it.
            "agents/web": {
                "id": "web",
                "handler_module": "web.tools",
                "parent_id": self.root_id,
                "port": port,
                "callable_by": {**BASE, "ws": "*"},
                "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                             "rest": "*", "ws": "*", memory.MEMORY: "*", **reach},
            },
            # The WS leg — the EVENT face: every ws frame is the leg, naming each
            # target by literal id.
            "agents/web/agents/ws": {
                "id": "ws",
                "handler_module": "web_ws.tools",
                "parent_id": "web",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {**BASE, "$parent": "*"},
                "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*", "web": "*",
                             memory.MEMORY: "*", **reach},
            },
            # The REST leg — the test's operator: the root's kernel verbs, its own
            # children, the declared ids.
            "agents/web/agents/rest": {
                "id": "rest",
                "handler_module": "web_rest.tools",
                "parent_id": "web",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {**BASE, "$parent": "*"},
                "can_call": {**BASE, self.root_id: ROOT_VERBS, "kernel": "*",
                             "$subtree": "*", **reach},
            },
        }
        for rel, rec in legs.items():
            d = w / rel
            d.mkdir(parents=True, exist_ok=True)
            write_rec(d / "agent.yaml", rec)
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
    """Any `fantastic_kernel`/`scenario_probes.py` process whose cwd is under the run work
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
        if "fantastic_kernel" not in cmd and "e2e/probes/scenario_probes.py" not in cmd:
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
