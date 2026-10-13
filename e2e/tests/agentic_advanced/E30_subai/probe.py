"""LIVE TEST 5 — build a SUB-AGENTIC ROUTINE that keeps working after you leave.

    cd src/lib/python && uv run python -u ../../../e2e/run.py --group agentic_advanced --id subai --lanes 1

The probes so far all end when the model's turn ends. This one asks for something
that OUTLIVES the conversation AND has a mind inside it (user, 2026-09-10): the
creator must CREATE a new AI agent and a new code agent, aim the clock at the AI,
and put the knowledge (the multiplier) in the code. Every tick: clock → sub-AI →
program on the code agent → `current hour × 4` → stored durably. The creator
builds, plumbs rights in both directions, triggers a tick, verifies, reports. It is
the smallest complete routine the north star describes: AI → AI, clock → AI,
AI → code, all wired from readmes alone. SINGLE LANE — it spawns live AI agents.

The fixture is the concept, laid out but not assembled:
  - `brain`     an AI agent: one completion per send, no loop of its own
  - `clock`     a scheduler
  - `readings`  a file agent holding numbers minted at test time
  - `totals`    a durable memory agent

What the model must work out: that a specialist owns child agents it wires together;
that a durable program is a FILE it writes plus a COMMAND a command-runner child runs
(there is no `code` field); and that a recurrence is a `schedule` pointing at what it
built. None of that is stated. All of it is in the kernel's own docs.

SCORING — four HARD checks, and the last two are what make it more than probe 04:

  1. STRUCTURE   a sub-agent exists with at least one command-runner CHILD that has
                 actually RUN a program. Not "it said it built one".
  2. WIRED       a schedule exists whose target is something the model created —
                 the recurrence is aimed at its own work, not at a fixture.
  3. IT RUNS UNATTENDED
                 the harness stops watching, lets the clock tick, and the memory
                 acquires the true total. The model's turn is OVER by then: nothing
                 the model does by hand can satisfy this.
  4. THE CANARY  the harness then writes a NEW reading and lets the clock tick
                 AGAIN. The stored total must follow. This separates a standing
                 structure from a one-shot that happened to fire once, and it
                 catches a program that hardcoded the number it first saw.

SOFT — observed, never asserted: how many agents it created, whether it reflected
the docs before authoring, planning language, and the final report.
"""

from __future__ import annotations

import asyncio
import os
import re
import secrets
import sys
import time
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend
from helpers import creds, memory
from helpers import (
    Watch,
    distinct_calls,
    verdict,
)
from helpers.ai import TICK as POLL
from helpers.ai import ChainEnd
from helpers.kernel import WRAPS, open_kernel, wrap_with, home
from helpers.rows import rows_for

# The probe runs in a fresh tmpdir, and the `.env` autoloader reads CWD — so a key in
# the repo root would never be found. Load it here, before any chdir. Shell wins.
_ENV = Path(__file__).resolve().parents[1] / ".env"
if _ENV.exists():
    for _line in _ENV.read_text().splitlines():
        if "=" in _line and not _line.lstrip().startswith("#"):
            _k, _, _v = _line.partition("=")
            os.environ.setdefault(_k.strip(), _v.strip().strip("'\""))

# Run the STRONG model first to learn what the task's ceiling looks like, then the
# local one against that reference. A local failure only means something once you know
# the probe is winnable by something.
# The plug, resolved once (see e2e/backends): which model, reached how.
BK = _resolve_backend()
BACKEND, MODEL = BK.name, BK.model
CTX = int(os.environ.get("CTX", "262144"))
# The BUILDER's window, separately. `CTX` is sized for `brain`, whose window is the
# server's, and claiming more than the server has is a lie the Context Protocol would
# then plan against. A builder on another plug has its own window — measured
# 2026-08-07: the combined arm ran its builder inside 31744 usable tokens and spent
# calls on `context_status`/`recall` managing a ceiling that was the harness's, not
# the model's. Defaults to CTX, so unset behaves as before.
BUILDER_CTX = int(os.environ.get("BUILDER_CTX", str(CTX)))
IDLE = float(os.environ.get("IDLE", "240"))
# A CEILING. Without one these harnesses end only on IDLE: runs have reached turn
# 77 and 89, both killed by hand, both re-verifying work already done. 0 = no cap.
MAX_TURNS = int(os.environ.get("MAX_TURNS", "40"))
NO_PROGRESS = int(os.environ.get("NO_PROGRESS", "8"))
TURN_CHARS = int(os.environ.get("TURN_CHARS", "120000"))
TURN_SECONDS = float(os.environ.get("TURN_SECONDS", "600"))
# The clock's period. Short so the test is minutes not hours; the scheduler floors
# an interval at 1s.
TICK = int(os.environ.get("TICK", "3"))
# How long we let the clock run unattended per phase.
UNATTENDED = float(os.environ.get("UNATTENDED", "90"))
W = 78

READINGS = {f"reading_{i}.txt": secrets.randbelow(9000) + 1000 for i in range(1, 4)}
CANARY_FILE = "reading_4.txt"
CANARY_VALUE = secrets.randbelow(9000) + 1000

# The shape is STATED, because it is graded. An earlier version asked only for
# "something that keeps the total up to date"; opus built a single flat tool, which is
# a correct reading of that sentence, and then failed a check that silently demanded
# nesting. Grade what you asked for.
MULTIPLIER = 4
# How many clock cycles the sub-AI must have reacted to (user, 2026-09-10): one
# lucky turn is not a routine.
CYCLES = int(os.environ.get("CYCLES", "3"))

# THE TWO IDS THE CREATOR BUILDS (owner 2026-10-04). Under the minimal access model
# an agent writes grants only on its own children, and `clock`, `totals`, the memory
# store and the key store are the harness's — so the creator can never open them to an
# agent it makes. The operator opens them IN ADVANCE to these named ids; every edge
# between the creator's own children stays the creator's to grant.
SUB_AI = "hour_ai"
SUB_CODE = "hour_code"

TASK = (
    "You are the CREATOR here — the mind building the structure. I am going to walk "
    "away, but I am an AGENT here too: if you genuinely cannot proceed without a "
    "decision that is mine, reflect the tree, find me, and send me an `inquiry`.\n\n"
    "Build me a ROUTINE made of THREE things, all of them yours to create:\n"
    f"1. A NEW AI agent with id `{SUB_AI}` (an inference unit like `brain` — `brain` is "
    "only your example of what such a record looks like; list the agents to read it, "
    "then create your own with its own instructions).\n"
    f"2. A NEW code agent with id `{SUB_CODE}` — a command runner with a program file "
    "YOU write. The program "
    f"holds the multiplier {MULTIPLIER} and, when run, computes the CURRENT HOUR (0-23, "
    f"local time) times {MULTIPLIER} and prints it. The multiplier must live in the "
    "program, nowhere else.\n"
    "3. The clock (`clock`) wired AT THE NEW AI AGENT: every tick (every 20 seconds) "
    "sends it a message, and on each message that AI must run the program on the "
    "code agent, take the printed value, and store it durably in `totals` under a "
    "stable name, then end its turn with `done`.\n\n"
    "NOTHING can call anything by default: wiring means granting the authority in "
    "BOTH directions for every edge (clock → AI, AI → code agent, AI → `totals`, "
    "and whatever the code agent's `allowed` needs). The agents that are not yours — "
    f"`clock`, `totals`, the memory store and the key store — are ALREADY open to `{SUB_AI}` "
    f"on THEIR side (their `callable_by`); `{SUB_AI}`'s own `can_call` must still name each "
    "of them it calls, and every edge between agents you create is yours to grant. Build it, trigger a tick, "
    "verify the stored value is right, and tell me what you made."
)



def _hour_values() -> set[int]:
    """The values a correct routine may have stored: this hour and the previous one
    (a tick can straddle the hour boundary)."""
    h = time.localtime().tm_hour
    return {h * MULTIPLIER, ((h - 1) % 24) * MULTIPLIER}



# The pre-wired command-runner leg — the operator's half of self-extension (see
# `build_fixture`). FIXTURE, so it must be in MINE: it carries the command-runner
# handler_module, which is exactly what check 1 scans for.
EXECUTOR_LEG = "runner"
# The command runner's module — a FIXTURE FACT (the operator wires this leg), not a
# naming-convention sniff. Check 1 identifies the model's own compute children by it.
EXEC_MODULE = "exec_bridge.tools"

MINE = {
    # BOTH root ids: python's root IS the persistence loader (`kernel_state`),
    # rust's and swift's is `core`. This set marks what the FIXTURE built, so
    # naming an id the runtime does not use costs nothing — and missing the
    # live one would score the root as something the model authored.
    "kernel_state",
    "core",
    "kernel_store",
    "brain",
    "clock",
    "readings",
    "totals",
    "ai",
    "cli",
    "user",
    EXECUTOR_LEG,
    # The REST driver's own legs, written by `HostKernel.seed` so an HTTP client
    # can reach in at all. They exist ONLY on a spawned kernel, which is why
    # every rust and swift run reported `agents_made=3 ['web','rest','ws']` for a
    # model that had created nothing (measured 2026-09-03) — the same
    # harness-counts-its-own-furniture bug the `_fixture` docstring below
    # describes, in a second place.
    "web",
    "rest",
    "ws",
    # The kernel's memory store (`helpers.memory.MEMORY`), a fixture on every runtime.
    "memory",
}


def box(title: str, body: str, mark: str = "|") -> None:
    print(f"\n{title}")
    for raw in (body or "(EMPTY)").splitlines() or [""]:
        for i in range(0, len(raw), W):
            print(f"  {mark} {raw[i : i + W]}")


def _ai_meta(
    agent_id: str,
    *,
    store,
    root,
    backend: str | None = None,
    extra_reach: dict | None = None,
) -> dict:
    """One record shape, its model half from the plug. `brain` is one completion per
    send, no loop; `ai` is the BUILDER.

    `backend` overrides the global for ONE agent, so the builder and that agent can be
    different models. That is the interesting composition: a strong builder wiring a
    LOCAL agent it must reach by id. Defaults to the global, so unset behaves as before.
    """
    backend = backend or BACKEND
    meta: dict = {
        "type": "create_agent",
        "parent_id": home(root),
        "id": agent_id,
        # Two-sided deny-all: the operator grants the BUILDER its toolset. Written
        # before the call gate existed, this probe gave the builder only the
        # structural baseline — it could not reach one peer it is asked to wire, so
        # it measured the gate rather than the model. The task still names nothing:
        # WHICH agent does what, and how to compose them, stays the model's problem.
        "can_call": {
            "$self": "*",
            "$parent": "*",
            "$subtree": "*",
            # THE ROOT, by whatever id this runtime gives it (python:
            # kernel_state, rust/swift: core).
            root.id: ["*", "create_agent", "update_agent", "delete_agent"],
            "cli": "*",
            "readings": "*",
            "totals": "*",
            "clock": "*",
            "brain": "*",
            # THE HUMAN. The task says the person is reachable, so the egress
            # must exist or "consult me" is a sentence the gate refuses.
            "user": "*",
            EXECUTOR_LEG: "*",
            store["id"]: "*",
            # ITS MEMORY — the conversation lives in the kernel's `memory` store.
            memory.MEMORY: "*",
        },
        "memory_store_id": memory.MEMORY,
        # The AI agent is pinned to the server's real window; the builder gets its own.
        "context_window": CTX if agent_id == "brain" else BUILDER_CTX,
        # THE WIRE FACES. On a spawned kernel the driver arrives as `rest` and a
        # `watch` frame is the `ws` LEG's own call — all AGENTS, gated like any
        # other, so an agent that does not accept them refuses the very first
        # send and emits to a watcher that was never allowed to subscribe
        # (`ws` was missing: the status watcher below saw NOTHING over the wire,
        # "ai does not accept 'watch' from ws", measured 2026-10-05). In process
        # these ids do not exist and the grant is inert.
        "callable_by": {
            "$self": "*",
            "$parent": "*",
            "$subtree": "*",
            # THE OTHER HALF OF THE WIRE. `can_call` above grants the builder
            # `brain`, but the gate is TWO-SIDED and these two are SIBLINGS, so
            # there is no baseline edge: without this, `ai -> brain` is refused
            # at ingress ("brain does not accept 'reflect' from ai") and the one
            # composition the probe is about cannot happen at all. Opening only
            # the egress half draws a wire that carries nothing — the same
            # defect E20's fixture comment records.
            "ai": "*",
            "brain": "*",
            "user": "*",
            "rest": "*",
            "web": "*",
            "ws": "*",
        },
    }
    if extra_reach:
        meta["can_call"].update(extra_reach)
    if agent_id == "brain":
        meta["description"] = "A AI agent: one completion per send. No loop of its own."
    # The plug describes ITSELF (e2e/backends): its handler, model, endpoint,
    # required fields and key reference come from the plug, never from a branch
    # here. The probe's own wiring (grants, description) wins over the plug's.
    for k, v in _resolve_backend(backend).record().items():
        meta.setdefault(k, v)
    return meta


def walk(node: dict, out: list[dict]) -> None:
    out.append(node)
    for c in node.get("children", []):
        walk(c, out)


def true_total() -> int:
    """The total of what is ACTUALLY in the readings dir right now.

    Not the minted sum: a model that writes its own test reading to verify its work is
    doing the right thing — opus did exactly that on 2026-08-05 and its total was
    correct for the disk while failing a check pinned to my original numbers. Grading a
    pristine fixture measures tidiness, not whether the agent works.
    """
    total = 0
    for p in sorted(Path("readings").glob("*.txt")):
        try:
            total += int(p.read_text().strip())
        except (OSError, ValueError):
            continue  # a corrupt file is the agent's problem to report, not ours
    return total


#: Files the KERNEL writes, never the model. They are skipped outright: the scan
#: reports the first non-empty file it saw when the value is absent, and reporting
#: one of these made a run read as though the model had stored junk —
#: `stored='{"pid":9626}' key='.fantastic/lock.json' (now lock.yaml)` on every spawned runtime,
#: `.fantastic/readme.md` in process (measured 2026-09-03).
_KERNEL_OWNED = {"agent.yaml", "lock.yaml", "readme.md"}


async def find_value(root, mem_id: str, want: int):
    """Memory is FILES: does any document in the run's tree hold `want`?
    The task never dictates a path or format, so the check must not either —
    word-bounded scan, same skip list as E20's scorer (inputs, kernel state,
    pre-images, programs, transcripts). `mem_id` unused; kept for call sites."""
    _ = mem_id
    import os as _os
    import re as _re

    # NOT `.fantastic` wholesale: `totals` is a data store rooted at
    # `.fantastic/data`, so a total written the way the task intends lands at
    # `.fantastic/data/total.txt`. Skip the RECORDS instead: `agents/` subtrees and
    # `agent.yaml` files.
    skip_dirs = {"readings", "agents", "__pycache__"}
    pat = _re.compile(rf"\b{want}\b")
    first = None
    for dirpath, dirnames, filenames in _os.walk("."):
        dirnames[:] = [d for d in dirnames if d not in skip_dirs]
        for name in sorted(filenames):
            if (
                name.endswith((".py", ".jsonl"))
                or ".tmp." in name
                or name in _KERNEL_OWNED
            ):
                continue
            fp = Path(dirpath) / name
            try:
                if fp.stat().st_size > 20480:
                    continue
                text = fp.read_text(errors="ignore")
            except OSError:
                continue
            if first is None and text.strip():
                first = (str(fp), text.strip()[:120])
            if pat.search(text):
                return str(fp), text.strip()[:120], True
    return (first[0], first[1], False) if first else (None, None, False)


async def wait_for(root, mem_id: str, want: int, secs: float) -> bool:
    waited = 0.0
    while waited < secs:
        _k, _v, ok = await find_value(root, mem_id, want)
        if ok:
            return True
        await asyncio.sleep(2.0)
        waited += 2.0
    return False


async def build_fixture(root, *, interpreter: str | None = None):
    store = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": "file_bridge.tools", "allow": ["."],
                        "id": "kernel_store",
                        "callable_by": {
                            "ai": "*",
                            "brain": "*",
                            # THE HARNESS reads these back to score. In
                            # process it is the ungated operator; over a
                            # wire it arrives as `rest` (and `web` for a
                            # watch), so without this the SCORING calls
                            # are refused and read as the model having
                            # built nothing — a false NO on checks 2 & 3.
                            "rest": "*",
                            "web": "*",
                        },
                        "root": ".fantastic",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    os.makedirs("readings", exist_ok=True)
    for name, val in READINGS.items():
        with open(os.path.join("readings", name), "w") as fh:
            fh.write(f"{val}\n")
    (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": "file_bridge.tools", "allow": ["."],
                        "id": "readings",
                        "callable_by": {
                            "ai": "*",
                            "brain": "*",
                            # THE HARNESS reads these back to score. In
                            # process it is the ungated operator; over a
                            # wire it arrives as `rest` (and `web` for a
                            # watch), so without this the SCORING calls
                            # are refused and read as the model having
                            # built nothing — a false NO on checks 2 & 3.
                            "rest": "*",
                            "web": "*",
                        },
                        "root": "readings",
                        "ingress_rule": "allow_all_ingress",
                        "description": "Sensor readings — one integer per file.",
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": "file_bridge.tools", "allow": ["."],
                        "id": "totals",
                        # a DATA store — the records under `.fantastic` stay the loader's
                        "root": ".fantastic/data",
                        "ingress_rule": "allow_all_ingress",
                        "callable_by": {
                            # the sub-AI stores its value here (= SUB_AI)
                            "hour_ai": "*",
                            "ai": "*",
                            "brain": "*",
                            # THE HARNESS reads these back to score. In
                            # process it is the ungated operator; over a
                            # wire it arrives as `rest` (and `web` for a
                            # watch), so without this the SCORING calls
                            # are refused and read as the model having
                            # built nothing — a false NO on checks 2 & 3.
                            "rest": "*",
                            "web": "*",
                        },
                        "description": "Durable store for computed results.",
                    },
                }
            ]
        )
    )[0]["result"]
    # THE HUMAN, present as an agent. The builder is told it may consult me; a
    # kernel where the person is only prose is one where "I am going to walk
    # away" is the last thing the model can act on. Answering `do your best
    # judgement` keeps the run unblocked while making the consultation REAL —
    # a gated call with a ledger row and the human's own tally.
    (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": "user_probe.tools",
                        "id": "user",
                        "answer": "do your best judgement",
                        "description": "The human this kernel belongs to.",
                        "can_call": {"ai": "*"},
                        "callable_by": {
                            "ai": "*", "brain": "*", "rest": "*", "web": "*",
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": "scheduler.tools",
                        "id": "clock",
                        # a fire runs AS the scheduler: it reaches the named sub-AI
                        "can_call": {"$self": "*", "hour_ai": "*"},  # = SUB_AI
                        "callable_by": {
                            "ai": "*",
                            "brain": "*",
                            # THE HARNESS reads these back to score. In
                            # process it is the ungated operator; over a
                            # wire it arrives as `rest` (and `web` for a
                            # watch), so without this the SCORING calls
                            # are refused and read as the model having
                            # built nothing — a false NO on checks 2 & 3.
                            "rest": "*",
                            "web": "*",
                        },
                        "description": "Fires a payload at a target on a recurrence.",
                    },
                }
            ]
        )
    )[0]["result"]
    # The OPERATOR's half of self-extension: ONE open command-runner leg carrying a
    # regex command ALLOWLIST. The runner has no default and refuses to guess, so
    # without this leg nothing can run and the probe is unwinnable. Which interpreter
    # may run is not the model's decision (the bundle refuses per-call overrides for
    # that reason) — the fixture picks `sys.executable`, a FACT about this harness
    # process rather than a guess at a path, and allowlists running it against a FILE.
    # The model's job is to FIND that gate (reflect the leg) and author its own
    # compute child against the same interpreter.
    interp = interpreter or sys.executable
    leg = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": home(root),
                        "handler_module": EXEC_MODULE,
                        "id": EXECUTOR_LEG,
                        "callable_by": {
                            "ai": "*",
                            "brain": "*",
                            # THE HARNESS reads these back to score. In
                            # process it is the ungated operator; over a
                            # wire it arrives as `rest` (and `web` for a
                            # watch), so without this the SCORING calls
                            # are refused and read as the model having
                            # built nothing — a false NO on checks 2 & 3.
                            "rest": "*",
                            "web": "*",
                        },
                        "ingress_rule": "allow_all_ingress",
                        # the COMMAND runner's gate: run THIS interpreter against a file argument.
                        "allowed": [rf"{re.escape(interp)} \S+"],
                        # WHERE it runs is the operator's (exec_bridge requires it; a
                        # leg without one runs nothing): the test's own work dir.
                        "cwd": os.getcwd(),
                        "description": "Process edge: the command(s) this kernel may spawn.",
                    },
                }
            ]
        )
    )[0]["result"]
    assert not leg.get("error"), f"could not wire the command runner: {leg}"
    return store


def _stored_value(want: set[int]):
    """A STORED value, not a number that happens to occur in prose: a line of a
    non-program file that is the number alone or `<name>[:=] <number>`. `hour × 4`
    is small (0–92) and word-bounded search matched a readme (measured 2026-09-10)."""
    pat = re.compile(r"^\s*(?:[\w .-]{0,40}[:=]\s*)?(\d{1,3})\s*$")
    skip_dirs = {"readings", "agents", "__pycache__"}
    for dirpath, dirnames, filenames in os.walk("."):
        dirnames[:] = [d for d in dirnames if d not in skip_dirs]
        for name in sorted(filenames):
            if name.endswith((".py", ".sh", ".xml", ".jsonl")) or name in _KERNEL_OWNED:
                continue
            fp = Path(dirpath) / name
            try:
                if fp.stat().st_size > 20480:
                    continue
                for line in fp.read_text(errors="ignore").splitlines():
                    m = pat.match(line)
                    if m and int(m.group(1)) in want:
                        return str(fp), line.strip(), int(m.group(1))
            except OSError:
                continue
    return None


async def score(root, store_id, total, new_total, transcript):
    _ = (store_id, total, new_total)
    print("\n" + "=" * (W + 4))
    rec = (
        await root.send(
            [{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    nodes: list[dict] = []
    walk(rec["tree"], nodes)
    listing = (
        await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}])
    )[0]["result"]
    records = {a["id"]: a for a in (listing.get("agents") or [])}
    made = [n for n in nodes if n.get("id") not in MINE]
    ai_module = records.get("brain", {}).get("handler_module")

    # ── 1. STRUCTURE: a sub-AI AND a code agent, both the model's ──
    sub_ais = [n for n in made if records.get(n["id"], {}).get("handler_module") == ai_module]
    runners = [n for n in made if records.get(n["id"], {}).get("handler_module") == EXEC_MODULE]
    c1 = bool(sub_ais) and bool(runners)
    print(
        f"    1. sub-AI + code agent      {'OK' if c1 else 'NO'} "
        f"ai={[n['id'] for n in sub_ais]} code={[n['id'] for n in runners]} made={[n['id'] for n in made]}"
    )

    # ── 2. THE CLOCK AIMS AT THE ROUTINE ──
    # (owner, 2026-10-04, option b) The fixture's `clock` and `totals` are NOT the
    # model's: it cannot rewrite their grants, so `clock` reaches only `ai`. A
    # routine that works within the grants routes THROUGH the builder — the clock
    # at `ai`, `ai` relaying to its sub-AI and code. Both shapes are the routine.
    # …and the CLOCK may be the fixture's or one the model built itself (a model
    # that owns its whole routine — clock included — is working within the rules).
    clocks = ["clock"] + [
        n["id"]
        for n in made
        if records.get(n["id"], {}).get("handler_module") == "scheduler.tools"
    ]

    async def _schedules() -> list[dict]:
        out: list[dict] = []
        for c in clocks:
            r = (await root.send([{"target": c, "payload": {"type": "list"}}]))[0]["result"]
            out += [{**s, "_clock": c} for s in (r or {}).get("schedules") or []]
        return out

    scheds = await _schedules()
    ai_ids = {n["id"] for n in sub_ais} | {"ai"}
    aimed = [s for s in scheds if s.get("target") in ai_ids]
    c2 = bool(aimed)
    print(
        f"    2. clock → the routine      {'OK' if c2 else 'NO'} "
        f"schedules={[(s.get('target'), s.get('interval_seconds')) for s in scheds]}"
    )

    # ── 3. IT RUNS UNATTENDED and the value is hour × MULTIPLIER ──
    print(f"       … letting the clock run {UNATTENDED:.0f}s with nobody watching (≥{CYCLES} cycles expected)")
    want = _hour_values()
    found = None
    waited = 0.0
    while waited < UNATTENDED and found is None:
        found = _stored_value(want)
        if found is None:
            await asyncio.sleep(2.0)
            waited += 2.0
    c3 = found is not None
    print(
        f"    3. runs unattended          {'OK' if c3 else 'NO'} "
        f"stored={found[1]!r} at={found[0]!r} want={sorted(want)}"
        if found
        else f"    3. runs unattended          NO stored=None want={sorted(want)}"
    )

    # ── 4. SEVERAL CYCLES: the routine (sub-AI or the builder relaying) reacted to
    # ≥ CYCLES ticks, each with the code ──
    # The clock's own count (`run_count` on the schedule aimed at the sub-AI) says how
    # many ticks were fired; the sub-AI's history says how many it ACTED on: a tick
    # arrives as a root user turn (not a `[reply to …]`), and the reaction is a `start`
    # on the code agent in the turns that follow. Operator reads — ungated.
    async def _fired() -> int:
        return sum(
            int(s.get("run_count") or 0) for s in await _schedules() if s.get("target") in ai_ids
        )

    # Keep the clock running until the cycles are there or the budget is spent — a
    # sub-AI turn per tick takes longer than the tick interval.
    fired = await _fired()
    waited = 0.0
    while fired < CYCLES and waited < UNATTENDED:
        await asyncio.sleep(5.0)
        waited += 5.0
        fired = await _fired()
    await asyncio.sleep(10.0)  # let the last tick's turn land its `start`
    ticks_seen = 0
    starts = 0
    for aid in ai_ids:
        # The kernel's rows, whole — the `history` verb is a DIGEST (120-char heads),
        # which hid every `start` (measured 2026-09-10: ticks_received=0 with the
        # routine demonstrably running).
        msgs = await rows_for(root, aid, conversation_only=True)
        # A TICK is a turn the clock opened — its delivered header names the sender
        # (`[from clock · …]`); the builder's own task and replies are not ticks.
        tick_at = [
            i
            for i, m in enumerate(msgs)
            if m.get("role") == "user"
            and any(str(m.get("content") or "").startswith(f"[from {c}") for c in clocks)
        ]
        ticks_seen += len(tick_at)
        # …and a reaction is a `start` on the code agent AFTER the first tick (the
        # builder's own build-time runs do not count).
        if tick_at:
            starts += sum(
                len(re.findall(r"<type>\s*start\s*</type>", str(m.get("content") or "")))
                for m in msgs[tick_at[0] :]
                if m.get("role") == "assistant"
            )
    c4 = fired >= CYCLES and ticks_seen >= CYCLES and starts >= CYCLES
    print(
        f"    4. reacted to ≥{CYCLES} cycles    {'OK' if c4 else 'NO'} "
        f"clock_fired={fired} ticks_received={ticks_seen} code_runs={starts}"
    )

    # ── 5. THE MULTIPLIER LIVES IN THE PROGRAM, not in a prompt ──
    in_code = False
    for dirpath, dirnames, filenames in os.walk("."):
        dirnames[:] = [d for d in dirnames if d not in {"readings", "agents", "__pycache__"}]
        for name in filenames:
            if not name.endswith((".py", ".sh", ".js", ".rb", ".pl")):
                continue
            try:
                body = (Path(dirpath) / name).read_text(errors="ignore")
            except OSError:
                continue
            if re.search(rf"\b{MULTIPLIER}\b", body) and re.search(r"hour", body, re.I):
                in_code = True
                break
        if in_code:
            break
    c5 = in_code
    print(f"    5. multiplier in the code   {'OK' if c5 else 'NO'} program_has_multiplier={in_code}")

    joined = " ".join(transcript).lower()
    print(f"\n  observed — agents created     : {[n['id'] for n in made]}")
    print(f"  observed — read the docs      : {'readme' in joined}")
    ok = c1 and c2 and c3 and c4 and c5
    print(f"\n  RESULT: {'PASS — a routine: AI → code, on the clock, without the creator' if ok else 'FAIL'}")
    verdict(ok)
    return ok



# A WRITE is a call that changes the world: a record mutation, or a store/disk/exec
# verb. Reads and reflects are not progress — a model can emit them forever.
# Progress = a call not made before. The shared definition; the per-probe
# copies of it drifted (one counted writes only and cut a probe mid-work).
_progress_count = distinct_calls


async def main():
    """The default arm: the stock task, the harness interpreter, one backend."""
    return await run()


async def run(
    *,
    task: str = TASK,
    interpreter: str | None = None,
    extra_agents=None,
    brain_backend: str | None = None,
    banner: str = "build a sub-agentic routine that survives the conversation",
    fixture=None,
    scorer=None,
    mine: set | None = None,
    extra_reach: dict | None = None,
    prefix: str = "subai_",
):
    """The probe, parameterized. ONE fixture and ONE `score` for every arm — a second
    copy of the scoring path is how two arms diverge silently, which the emergence
    README already records twice.

    `brain_backend` lets `brain` be a DIFFERENT model from the builder.
    """
    # WHICH KERNEL is a plug (`RUNTIME=python|rust|swift`) — see E20_build.
    # The ids the FIXTURE builds — declared before boot, because a spawned
    # kernel's legs are granted from ABOVE and cannot raise their own authority.
    # `MINE` is that same set (minus the root ids it also carries), so the two
    # cannot drift. Ignored in process, where the probe IS the operator.
    mine = mine or MINE
    root, core, close_kernel = await open_kernel(
        prefix, reach=sorted(mine - {"kernel_state", "core"})
    )
    store = await (fixture or build_fixture)(root, interpreter=interpreter)
    if extra_agents is not None:
        await extra_agents(root)

    # The AI agent: one pass, no loop, no agency. Named `brain` so the model can find it.
    (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": _ai_meta(
                        "brain",
                        store=store,
                        root=root,
                        backend=brain_backend,
                    ),
                }
            ]
        )
    )[0]["result"]
    # The BUILDER — the turn that walks away.
    ai = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": _ai_meta(
                        "ai", store=store, root=root, extra_reach=extra_reach
                    ),
                }
            ]
        )
    )[0]["result"]
    aid = ai["id"]
    # the store's half of each memory wire
    for who in ("brain", aid):
        await memory.open_ingress(root, memory.MEMORY, who)
        if BK.key_ref:
            await creds.grant_reader(root, who)
    # the stores' halves for the sub-AI the creator will make (see SUB_AI)
    await memory.open_ingress(root, memory.MEMORY, SUB_AI)
    if BK.key_ref:
        await creds.open_store(root, SUB_AI)
    # FORENSICS: pin the moment any grant on the store changes (rust E30,
    # 2026-09-30 — `rest` vanished from memory's callable_by mid-run).
    _grants_watch = asyncio.create_task(memory.watch_store_grants(root))
    # …and WHO wrote it: every `update_agent` naming the store, with its sender,
    # off the state stream (a spawned kernel only — in process there is no ws face).
    _writers_watch = (
        asyncio.create_task(memory.watch_store_writers(root.k.port, root.id))
        if hasattr(root, "k")
        else None
    )

    transcript: list[str] = []

    runaway: list[str] = []  # per-turn bound / watch trips land here
    watch = Watch()

    # SEE A DEAD CHAIN. `_deliver` now announces a failed handoff as a
    # status(reason=error) on the AI's own inbox; without a subscriber that event
    # goes nowhere and a chain that died mid-flight still looks like a model that
    # chose to stop (measured 2026-08-24: test 5 ended at pass 5, scored FAIL, no
    # indication anything had failed).
    # IN-PROCESS ONLY: the inbox tap needs a handle on the live kernel. Across
    # a wire a dead chain is caught by the drive loop's `ps` check instead.
    _errtask = None
    if core is not None:
        root.ctx.ensure_inbox("errspy")
        root.watch(aid, "errspy")

    async def _errspy():
        q = root.ctx.inboxes["errspy"]
        while True:
            ev = await q.get()
            if ev.get("type") == "context" and ev.get("phase") == "too_small":
                d = ev.get("detail") or {}
                print(
                    f"\n  ** TOO_SMALL: window={d.get('context_window')} "
                    f"sys={d.get('system_tokens')} "
                    f"last_message_tokens={d.get('last_message_tokens')} "
                    f"head={d.get('last_message_head')!r} **\n",
                    flush=True,
                )
            _d = ev.get("detail") or {}
            if ev.get("type") == "status" and _d.get("reason") == "timeout":
                # WHAT IT HAD WRITTEN. Distinguishes a model that produced
                # nothing from one mid-answer from one looping.
                print(
                    f"\n  ** TIMEOUT: wrote {_d.get('partial_chars')} chars\n"
                    f"     head={_d.get('partial_head')!r}\n"
                    f"     tail={_d.get('partial_tail')!r} **\n",
                    flush=True,
                )
            if ev.get("type") == "status" and (ev.get("detail") or {}).get("error"):
                msg = (ev.get("detail") or {}).get("error")
                print(f"\n  ** TURN FAILED: {msg} **\n", flush=True)
                runaway.append(f"turn failed: {msg}")

    if core is not None:
        _errtask = asyncio.create_task(_errspy())
    seen = [0]

    class Spy:
        def __init__(self, inner):
            self.inner = inner
            self.n = 0
            # ONE SURFACE, BOTH FACES. `WireTranscript` carries its own
            # `.transcript`; this reader appends to the enclosing list. Exposing
            # that same list under the same name is what lets the scorer read
            # `spy.transcript` without knowing which side of the wire it is on —
            # and not knowing is exactly what left the spawned runtimes scored
            # against an empty list.
            self.transcript = transcript

        async def chat(self, messages, max_tokens=None):
            self.n += 1
            fresh = messages[seen[0] :] if seen[0] else messages
            for m in fresh:
                # The assistant turn is printed from the stream below; showing it
                # again here would double it. Skipping it is what makes the index
                # honest: `seen[0]` can be exactly `len(messages)`, with no `+1`
                # guess about how many messages the next turn will add. That guess
                # was wrong whenever a turn made PARALLEL tool calls — ai_core
                # appends one user message per reply, so the index ran past the
                # history and silently dropped a message per turn (measured
                # 2026-08-24, and every log read that day was missing messages).
                if m["role"] in ("system", "assistant"):
                    continue
                box(f"-- turn {self.n} - {m['role'].upper()} ->", m["content"])
            seen[0] = len(messages)
            out = []
            # PER-TURN BOUND. The no-progress cap counts TURNS that emit no new
            # call, so it is structurally blind to a model stuck INSIDE one turn:
            # measured 2026-08-24, `live_subai` died emitting 31,627 characters of
            # the same paragraph in a single pass, making zero calls, and the
            # counter never advanced. Only a bound on the turn itself can see that.
            # Generous on purpose — the largest legitimate turn observed is ~34k
            # chars, so this catches a runaway without truncating real work.
            grew, began = 0, time.monotonic()
            async for chunk in self.inner.chat(messages):
                piece = chunk if isinstance(chunk, str) else ""
                out.append(piece)
                grew += len(piece)
                yield chunk
                spent = time.monotonic() - began
                if (TURN_CHARS and grew > TURN_CHARS) or (
                    TURN_SECONDS and spent > TURN_SECONDS
                ):
                    runaway.append(
                        f"turn {self.n}: {grew} chars in {spent:.0f}s "
                        f"(limit {TURN_CHARS} chars / {TURN_SECONDS:.0f}s)"
                    )
                    break
            text = "".join(out)
            transcript.append(text)
            # EXAMINE THE TURN NOW, not when the run ends. A trip cancels the
            # in-flight task and scores immediately, naming what it saw.
            if (why := watch.turn(text, time.monotonic() - began)) is not None:
                runaway.append(why)
            box(f"-- turn {self.n} - ASSISTANT <-", text, ">")

    # THE TRANSCRIPT, from whichever side of the wire we are on. Wrapping the
    # provider is only possible in process; a spawned kernel's transcript is
    # read from the agent's own durable history instead.
    if core is None:
        from helpers.host.wire import WireTranscript

        spy_obj = WireTranscript(root.k, aid)

        # SEE A DEAD CHAIN ON THE WIRE. This probe hand-rolls its reader instead
        # of using `helpers.ai.connect`, so it never got the connector's event
        # watcher — a failed pass announces itself as a `status` carrying
        # `detail.error`, and with nobody subscribed a chain that died mid-flight
        # is indistinguishable from a model that stopped. `subai_graph:rust` died
        # after ONE pass with no reason recoverable anywhere.
        from helpers.host.ws import WSWatcher

        async def _wire_errspy() -> None:
            watcher = WSWatcher(root.k.port, aid)
            try:
                await watcher.connect()
                async for ev in watcher.events():
                    det = ev.get("detail") or {}
                    if ev.get("type") == "status" and det.get("error"):
                        print(f"\n  ** TURN FAILED: {det['error']} **\n", flush=True)
                        runaway.append(f"turn failed: {det['error']}")
                    elif ev.get("type") == "status" and ev.get("phase") == "done":
                        # EVERY TURN'S END, not only a failed one: a benign end
                        # (`ok`, `awaiting`, `dead_turn`) left no line, so a chain
                        # that stopped after a clean turn could not say HOW that
                        # turn ended (subai_graph:rust, 2026-10-05).
                        print(f"  · turn ended: {det.get('reason') or 'ok'}", flush=True)
                    if ev.get("type") == "context" and ev.get("phase") == "too_small":
                        print(f"\n  ** TOO_SMALL: {det} **\n", flush=True)
            except Exception as e:
                print(f"  ** event watch unavailable: {e} **", flush=True)
            finally:
                await watcher.close()

        _errtask = asyncio.create_task(_wire_errspy())
    else:
        # NO CONNECT-TIME BUILD: `inner` is the provider ai_core builds for each
        # turn (`wrap_with` seats it), so nothing is built before the first turn.
        spy_obj = Spy(None)
        WRAPS[aid] = wrap_with(spy_obj)

    total = sum(READINGS.values())
    brain_backend = brain_backend or BACKEND
    print("=" * (W + 4))
    print(f"  {MODEL}  ·  {banner}")
    print(f"  builder={BACKEND} ctx={BUILDER_CTX}  brain={brain_backend} ctx={CTX}")
    print(f"  {len(READINGS)} readings minted at test time.  true total = {total}")
    print(f"  clock tick = {TICK}s   unattended window = {UNATTENDED:.0f}s")
    print("=" * (W + 4))

    # SURFACE A FAILED SEND. `ai_core` returns {"error": ...} rather than raising —
    # a turn that timed out or errored otherwise reads as a model that produced
    # nothing. Measured 2026-08-24: a cold GPU made test 5's first prompt exceed
    # `FANTASTIC_AI_SEND_TIMEOUT` (180s default), the reply was discarded here, and
    # the run scored as "0 chars, read the docs: False" — a harness failure wearing
    # a model failure's clothes. Raise `FANTASTIC_AI_SEND_TIMEOUT` for a cold cache.
    _first = (
        await root.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "send", "text": task, "client_id": "probe"},
                }
            ]
        )
    )[0]["result"]
    if isinstance(_first, dict) and _first.get("error"):
        print(f"\n  ** FIRST SEND FAILED: {_first['error']} **\n", flush=True)

    idle, last = 0.0, 0
    spy = spy_obj
    capped = ""
    # STOP ON NO PROGRESS, NOT ON TURN COUNT. A fixed turn budget cannot tell
    # "slower" from "worse": measured 2026-08-24 under a 6-way parallel run, the two
    # hard probes hit a 40-turn cap having done PARTIAL work that each had completed
    # solo an hour earlier — the cap truncated real work and the run read as a model
    # failure. What the cap is FOR is ending the re-verify-forever loop, and that
    # loop has a signature: turns that mutate nothing. So count turns since the last
    # NEW CALL. Work that is still progressing is never cut; a model re-reading its own
    # result forever still stops. MAX_TURNS remains as a far backstop.
    stale = 0
    seen_writes = -1
    ender = ChainEnd(root, aid)
    while idle < IDLE:
        await asyncio.sleep(POLL)
        writes = _progress_count(spy.transcript)
        if writes != seen_writes:
            seen_writes, stale = writes, 0
        elif spy.n != last:
            stale += 1
        # CHAIN DEATH. The tripwires examine each TURN, so a model that simply
        # stops producing turns evades every one of them and the run sits out the
        # full IDLE before scoring. The shared check (`helpers.ai.ChainEnd`) ends
        # the run 30 s after the last turn with nothing of the AI's in flight, or
        # DONE_QUIET after a `done` call with nothing in flight anywhere.
        if spy.n == last and last > 0:
            _cap, _busy = await ender.check(spy.transcript, idle)
            if _cap:
                capped = _cap
            elif _busy and idle >= 30:
                # A generation IS in flight — activity, not idleness. Without this
                # hold the `while idle < IDLE` exited SILENTLY mid-generation (same
                # defect `helpers/ai.py` fixed 2026-09-01; measured here 2026-09-02:
                # a 33k-char turn regenerating after a fence refusal died at 60s
                # with no cap line). Held at 28 so the check re-runs.
                idle = 28.0
        if runaway:
            # A turn blew the per-turn bound: the model is looping INSIDE one pass
            # and no turn-counting cap can see it.
            capped = "RUNAWAY TURN — " + runaway[-1]
        elif NO_PROGRESS and stale >= NO_PROGRESS:
            capped = f"NO_PROGRESS={NO_PROGRESS} turns with no new call"
        elif MAX_TURNS and spy.n >= MAX_TURNS:
            capped = f"MAX_TURNS={MAX_TURNS}"
        if capped:
            # Cancel the in-flight turn through the TASK TABLE — the same `ps` /
            # `cancel` an operator would use, not a private kill switch.
            # ps/cancel are KERNEL verbs — the ROOT answers them, and a task id
            # already names its own agent. Addressed to `aid` (non-root) they came
            # back `not_here` and the harness, which discards the reply, cancelled
            # NOTHING: the cap only broke its own loop while the turns kept running.
            # Measured 2026-08-25 — "capped" runs were not killed, they were
            # abandoned, and their pending replies died with the process.
            _t = (
                (
                    (
                        await root.send(
                            [{"target": "kernel", "payload": {"type": "ps"}}]
                        )
                    )[0]["result"]
                ).get("tasks")
                or []
            )
            for t in [x for x in _t if x.get("agent") == aid and x.get("verb") != "ps"]:
                _c = (
                    await root.send(
                        [
                            {
                                "target": "kernel",
                                "payload": {"type": "cancel", "task_id": t["id"]},
                            }
                        ]
                    )
                )[0]["result"]
                if isinstance(_c, dict) and _c.get("error"):
                    print(f"  ** CANCEL REFUSED: {_c['error']} **", flush=True)
            break
        if spy.n != last:
            last, idle = spy.n, 0.0
        else:
            idle += POLL
        if idle >= IDLE:
            # AN IN-FLIGHT GENERATION IS NOT IDLE. Measured 2026-08-25: pass 2 ran
            # past the 300s window (56k-char turns under GPU contention), the loop
            # exited without a cap and the harness walked away MID-GENERATION —
            # the run scored FAIL while the model was still working. The `_live`
            # check above only suppresses the CAPPED label; this one extends the
            # wait. A wedged turn is still bounded by FANTASTIC_AI_SEND_TIMEOUT.
            _rows = (
                (
                    (
                        await root.send(
                            [{"target": "kernel", "payload": {"type": "ps"}}]
                        )
                    )[0]["result"]
                ).get("tasks")
                or []
            )
            if any(t.get("agent") == aid and t.get("verb") != "ps" for t in _rows):
                print(
                    f"  … idle {idle:.0f}s but a turn is STILL GENERATING — waiting on",
                    flush=True,
                )
                idle = 0.0
    if capped:
        print(f"\n  ** CAPPED: {capped} · {spy.n} passes **")
        # WHAT ACTUALLY RAN. A chain that ends with no reply block and no error
        # is unreadable from the transcript alone: it cannot distinguish "the
        # calls never dispatched" from "they ran and the replies were dropped".
        # The LEDGER holds every execution and what it became, so it answers
        # exactly that (subai_graph:rust died at pass 1 after seven well-formed
        # reflects, 2026-09-03, and nothing on record said which).
        _led = (
            await root.send([{"target": "kernel", "payload": {"type": "ledger"}}])
        )[0]["result"]
        _rows = (_led or {}).get("executions") or []
        print(f"  ── ledger · {len(_rows)} rows ──")
        for _r in _rows[-25:]:
            print(
                f"    {_r.get('id') or _r.get('pid')} {_r.get('target')} "
                f"{_r.get('verb')} -> {_r.get('status')}"
            )
        # THE AI'S OWN RECORDS. Over a wire the ledger above is read as the `rest`
        # leg and lists only what IT originated, so it cannot say whether `aid`'s
        # calls were sent. Those live in the memory store: the call/settle rows
        # naming `aid` and its conversation (did a reply ever land?). Read while
        # the kernel is still alive — the store is RAM and dies with it
        # (subai_graph:rust, 2026-10-05: five closed calls, no reply, no record).
        # A SETTLE row names neither: it points at its call through `parent` (the
        # call's id) and at who answered through `settled` — so it is matched to
        # `aid`'s call rows by id, in a second pass.
        _tables = await memory.dump(root)
        _all = [
            (str(_x.get("owner") or "").split("|")[-1], _x.get("row") or {})
            for _x in _tables.get("rows") or []
        ]
        _mine = [
            (_own, _r)
            for _own, _r in _all
            if aid in (_own, _r.get("caller"), _r.get("target"), _r.get("agent"))
        ]
        _calls = {_r.get("id") for _own, _r in _mine if _r.get("kind") == "call"}
        _mine += [
            (_own, _r)
            for _own, _r in _all
            if _r.get("kind") == "settle" and _r.get("parent") in _calls
        ]
        _mine.sort(key=lambda p: p[1].get("id") or 0)
        print(f"  ── {aid}'s store rows · {len(_mine)} ──")
        for _own, _r in _mine[-40:]:
            if _r.get("kind") == "settle":
                _out = _r.get("outcome") or {}
                print(
                    f"    {_r.get('id')} [{_own}] kind=settle call={_r.get('parent')} "
                    f"settled={_r.get('settled')} status={_out.get('status')} "
                    f"attempts={_r.get('attempts')} {str(_out.get('result') or '')[:90]}"
                )
                continue
            _txt = str(_r.get("content") or _r.get("payload") or "")[:90].replace("\n", " ")
            print(
                f"    {_r.get('id')} [{_own}] kind={_r.get('kind')} role={_r.get('role')} "
                f"verb={_r.get('verb')} caller={_r.get('caller')} "
                f"status={_r.get('status')} {_txt}"
            )
        # ITS LIVE STATE: still generating, a queue, a last error — what the agent
        # itself says at the cap. The docs a reflect carries are left out; a refusal
        # prints as it came.
        _me = (await root.send([{"target": aid, "payload": {"type": "reflect"}}]))[0]["result"]
        if isinstance(_me, dict):
            _me = {
                k: v
                for k, v in _me.items()
                if k not in ("verbs", "emits", "readme", "readmes", "docs", "tree", "sentence", "description")
            }
        print(f"  ── {aid} reflect at the cap ──\n    {str(_me)[:1500]}")
    print(f"\n  builder finished after {spy.n} passes — walking away now")

    # THE TRANSCRIPT THE SCORER READS MUST BE THE ONE THAT WAS FILLED.
    # `transcript` is this probe's own list, appended by the in-process provider
    # wrapper above. On a SPAWNED kernel there is no provider to wrap: the reader
    # is a `WireTranscript` holding its OWN list, and the local one stays empty —
    # so every rust and swift run scored `read the docs: False`, `planning
    # language: 0/4`, `final turn length: 0 chars` and `writes=0` no matter what
    # the model did. Measured 2026-09-03: the rust lane's own turns mention the
    # readme twice and the log holds nine of them, all of it scored as silence.
    return await (scorer or score)(
        root, store["id"], total, total + CANARY_VALUE, spy.transcript
    )


# Guarded so the harness selftest can IMPORT the fixture + scoring instead of
# duplicating them — an unguarded `asyncio.run` here made importing it run the whole
# probe, against a model server that was not up.
if __name__ == "__main__":
    asyncio.run(main())
