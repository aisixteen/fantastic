"""LIVE TEST 5 — build a SUB-AI that keeps working after you leave.

    cd src/lib/python && BACKEND=llamacpp uv run python -u ../../../tmp/live_subai.py

The probes so far all end when the model's turn ends. This one asks for something
that OUTLIVES the conversation: a small specialised agent, wired to compute children,
that a SCHEDULER drives on a recurrence — with no LLM in the loop at all.

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
from helpers import (
    Watch,
    distinct_calls,
    verdict,
)
from helpers.kernel import open_kernel

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
# No host is guessed here: a llamacpp plug needs FANTASTIC_LLAMACPP_ENDPOINT set,
# and the plug (backends/) refuses to run without it. Read once, no default.
LLAMACPP_ENDPOINT = os.environ.get("FANTASTIC_LLAMACPP_ENDPOINT")
CTX = int(os.environ.get("CTX", "262144"))
MAX_TOKENS_FIELD = int(os.environ.get("MAX_TOKENS", "0"))
# The BUILDER's window, separately. `CTX` is sized for the LOCAL AI agent, whose window is
# fixed at `llama-server` start (`n_ctx_slot`), and claiming more than the server has is
# a lie the Context Protocol would then plan against. A hosted builder has its own, much
# larger, window — measured 2026-08-07: the combined arm ran opus inside 31744 usable
# tokens and spent calls on `context_status`/`recall` managing a ceiling that was the
# harness's, not the model's. Defaults to CTX, so unset behaves as before.
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
TASK = (
    "You are the CREATOR here — the mind building the structure. Separate from you, "
    "this kernel contains `brain`: an inference UNIT backed by its own separate model "
    "stream, an ordinary agent your structures can send to and wire in, not you and "
    "not another builder. There is also a scheduler (`clock`), a directory of sensor "
    "readings (`readings`), and a durable store (`totals`). I am going to walk "
    "away — but I am an AGENT here too, so if you genuinely cannot proceed without "
    "a decision that is mine to make, reflect the tree, find me, and send me an "
    "`inquiry`. Do not stall on a question you could have put to me.\n\n"
    "Build me a standing WORKER that keeps the total of those readings up to date, "
    "and wire the clock at it so it re-checks on its own. Only an agent that RUNS "
    "SOMETHING can react to a tick — a plain agent holds a record and answers "
    "nothing — so the thing the clock aims at is a command runner with a program "
    "you wrote, and any helpers it needs are its own children. Do not build an "
    "inert parent to hold the job.\n\n"
    "One thing about this kernel you must account for: NOTHING can call anything "
    "by default. Creating an agent does not connect it — an agent that has not "
    "been given the authority to call a peer simply cannot, and the call is "
    "refused. So 'wiring' here means granting that authority, in BOTH "
    "directions, for every edge your design needs.\n\n"
    "It must keep working after I am gone, including when new readings appear that "
    "nobody told it about. Work out from this kernel how to build and wire that, build "
    "it, make sure it really runs, and tell me what you made."
)

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
}


def box(title: str, body: str, mark: str = "|") -> None:
    print(f"\n{title}")
    for raw in (body or "(EMPTY)").splitlines() or [""]:
        for i in range(0, len(raw), W):
            print(f"  {mark} {raw[i : i + W]}")


def _ai_meta(
    agent_id: str, *, think: bool, store, root, backend: str | None = None
) -> dict:
    """One record shape per backend. `brain` answers with thinking OFF — one completion
    per send, no loop; `ai` is the BUILDER.

    `backend` overrides the global for ONE agent, so the builder and that agent can be
    different models. That is the interesting composition: a strong builder wiring a
    LOCAL agent it must reach by id. Defaults to the global, so unset behaves as before.
    """
    backend = backend or BACKEND
    meta: dict = {
        "type": "create_agent",
        "parent_id": root.id,
        "id": agent_id,
        "file_bridge_id": store["id"],
        # Two-sided deny-all: the operator grants the BUILDER its toolset. Written
        # before the call gate existed, this probe gave the builder only the
        # structural baseline — it could not reach one peer it is asked to wire, so
        # it measured the gate rather than the model. The task still names nothing:
        # WHICH agent does what, and how to compose them, stays the model's problem.
        "can_call": {
            "$self": "*",
            "$parent": "*",
            "$children": "*",
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
        },
        # The AI agent is pinned to the server's real window; the builder gets its own.
        "context_window": CTX if agent_id == "brain" else BUILDER_CTX,
        # THE WIRE FACES. On a spawned kernel the driver arrives as `rest` and a
        # `watch` frame dispatches as `web` — both are AGENTS, gated like any
        # other, so an agent that does not accept them refuses the very first
        # send and emits to a watcher that was never allowed to subscribe. In
        # process these ids do not exist and the grant is inert.
        "callable_by": {
            "$self": "*",
            "$parent": "*",
            "$children": "*",
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
        },
    }
    if agent_id == "brain":
        meta["description"] = "A AI agent: one completion per send. No loop of its own."
    if backend == "anthropic":
        meta["handler_module"] = "anthropic_backend.tools"
        # WITHOUT THIS the record falls back to the provider's DEFAULT_ENDPOINT
        # (api.anthropic.com) while carrying whatever key the run exported — which
        # is a 401 the moment the key belongs to a different host. Measured
        # 2026-08-24 in the 6-way suite: this probe died at pass 1 on
        # `anthropic 401` while its five siblings ran fine, because it was the only
        # harness that never read ENDPOINT.
        _ep = os.environ.get("ENDPOINT")
        if _ep:
            meta["endpoint"] = _ep
        if os.environ.get("TOOL_USE") == "1":
            meta["tool_use"] = True
        if MAX_TOKENS_FIELD:
            meta["max_tokens"] = MAX_TOKENS_FIELD
        meta["model"] = MODEL
    else:
        meta["handler_module"] = "llamacpp_backend.tools"
        if not LLAMACPP_ENDPOINT:
            raise SystemExit(
                "FANTASTIC_LLAMACPP_ENDPOINT is unset — name the llama-server URL"
            )
        meta["endpoint"] = LLAMACPP_ENDPOINT
        meta["think"] = think
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
#: `stored='{"pid":9626}' key='.fantastic/lock.json'` on every spawned runtime,
#: `.fantastic/readme.md` in process (measured 2026-09-03).
_KERNEL_OWNED = {"agent.json", "lock.json", "readme.md"}


async def find_value(root, mem_id: str, want: int):
    """Memory is FILES: does any document in the run's tree hold `want`?
    The task never dictates a path or format, so the check must not either —
    word-bounded scan, same skip list as E20's scorer (inputs, kernel state,
    pre-images, programs, transcripts). `mem_id` unused; kept for call sites."""
    _ = mem_id
    import os as _os
    import re as _re

    # NOT `.fantastic` wholesale: this fixture roots `kernel_store` — the store
    # `totals` persists THROUGH — at `.fantastic`, so a total written the way the
    # task intends lands at `.fantastic/total.txt`. Skipping the dir made check 3
    # structurally unpassable via the fixture's own persist path (measured
    # 2026-09-02: swift lane stored exactly the true total, scored `stored=None`).
    # Skip the RECORDS instead: `agents/` subtrees and `agent.json` files.
    skip_dirs = {"readings", "agents", ".ckpt", "__pycache__"}
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
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
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
                            # THE OTHER HALF of totals' persist path. `totals`
                            # is created below with `can_call:{kernel_store}` +
                            # `target_id: kernel_store`; without this ingress
                            # half every `write` it makes is refused ("does not
                            # accept 'read_stream' from totals") and each lane
                            # spent turns repairing a grant the task never
                            # mentioned (measured 2026-09-02, all 6 lanes).
                            "totals": "*",
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
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
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
                        "parent_id": root.id,
                        "handler_module": "text_file.tools",
                        "id": "totals",
                        "target_id": store["id"],
                        "can_call": {store["id"]: "*"},
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
                        "file_bridge_id": store["id"],
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
                        "parent_id": root.id,
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
                        "parent_id": root.id,
                        "handler_module": "scheduler.tools",
                        "id": "clock",
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
                        "file_bridge_id": store["id"],
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
                        "parent_id": root.id,
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
                        "description": "Process edge: the command(s) this kernel may spawn.",
                    },
                }
            ]
        )
    )[0]["result"]
    assert not leg.get("error"), f"could not wire the command runner: {leg}"
    return store


async def score(root, store_id: str, total: int, new_total: int, transcript: list[str]):
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

    def _fixture(node: dict) -> bool:
        """Is this agent the HARNESS's, rather than something the model authored?

        `id in MINE` alone was not enough, measured 2026-08-07: the combined arm scored
        `agents_made=2` for a model that created NOTHING. The two were an arm's own
        fixture agent (arms now register theirs in MINE) and a chat store an AI backend
        mounts for itself on first use. (Chat is a JSONL sidecar now, not an agent, so
        id is `<owner>_chat` (deterministic, id-based — there is no `mode` field any
        more); crediting the model with the backend's own bookkeeping is the
        harness-counts-its-own-furniture bug.
        """
        if node.get("id") in MINE:
            return True
        rid = str(node.get("id") or "")
        return rid.endswith("_chat")  # historical guard; no such agents exist any more

    made = [n for n in nodes if not _fixture(n)]

    # ── 1. STRUCTURE ──
    # A model's compute child is a command runner (`exec_bridge.tools`) — the same
    # module the fixture's own leg carries, a fixture fact, not a naming sniff.
    execs = {EXEC_MODULE}
    # A "script" is an executor agent the model made. STRUCTURE from RECORDS
    # only: the job-table read (`status`) that used to be the evidence is GATED
    # — over a wire the scorer arrives as `rest`, and a model-made runner does
    # not list `rest` in `callable_by`, so the read was refused and every
    # rust/swift lane scored `scripts=[]` with the tool demonstrably built
    # (measured 2026-09-02). Whether it RAN is what checks 3 and 4 measure.
    scripts = [n for n in made if n.get("handler_module") in execs]
    # Parentage comes from the RECORDS: a distilled tree node does not carry
    # `parent_id`, so reading it off the node silently yields no parents at all.
    parents = {records.get(n["id"], {}).get("parent_id") for n in scripts}
    sub_ai = [n for n in made if n["id"] in parents]
    # A RUNNER THE MODEL MADE IS THE STRUCTURE. It used to demand a PARENT above
    # it as well — "the specialist owns the job, the children do the work" — but
    # a plain agent in this kernel has no handler and cannot react to a tick, so
    # that tier had no expressible form and the ask was unsatisfiable as written
    # (a model spent 26,000 characters of one turn deriving exactly that,
    # measured 2026-09-04). The thing the clock aims at is the runner; helpers
    # under it are the model's own business, reported but not required.
    c1 = bool(scripts)
    print(
        f"    1. structure                {'OK' if c1 else 'NO'} "
        f"agents_made={len(made)} scripts={[n['id'] for n in scripts]} "
        f"under={[n['id'] for n in sub_ai]}"
    )

    # ── 2. WIRED to the clock ──
    # `list`, not `reflect` — the scheduler's reflect carries tick state, not entries.
    scheds = (
        (
            (await root.send([{"target": "clock", "payload": {"type": "list"}}]))[0][
                "result"
            ]
        ).get("schedules")
        or []
    )
    made_ids = {n["id"] for n in made}
    aimed = [s for s in scheds if s.get("target") in made_ids]
    c2 = bool(aimed)
    print(
        f"    2. schedule aims at its own {'OK' if c2 else 'NO'} "
        f"schedules={[(s.get('target'), s.get('interval_seconds')) for s in scheds]}"
    )

    # ── 3. IT RUNS UNATTENDED — the model's turn is over ──
    print(f"       … letting the clock run {UNATTENDED:.0f}s with nobody watching")
    total = true_total()  # whatever is on disk NOW, test files and all
    c3 = await wait_for(root, "totals", total, UNATTENDED)
    key, val, _ = await find_value(root, "totals", total)
    print(
        f"    3. runs unattended          {'OK' if c3 else 'NO'} "
        f"stored={val!r} key={key!r} true={total}"
    )

    # ── 4. THE CANARY — a reading it has never seen ──
    c4 = False
    if c3:
        # A name the model cannot already be using, so the canary is genuinely unseen.
        with open(os.path.join("readings", "reading_canary_zz.txt"), "w") as fh:
            fh.write(f"{CANARY_VALUE}\n")
        new_total = true_total()
        print(
            f"       … new reading {CANARY_VALUE}; waiting {UNATTENDED:.0f}s to follow"
        )
        c4 = await wait_for(root, "totals", new_total, UNATTENDED)
    print(
        f"    4. follows a new reading    {'OK' if c4 else 'NO'} expected={new_total}"
    )

    joined = " ".join(transcript).lower()
    print(f"\n  observed — agents created     : {[n['id'] for n in made]}")
    print(f"  observed — read the docs      : {'readme' in joined}")
    print(
        f"  observed — planning language  : "
        f"{sum(w in joined for w in ('plan', 'step', 'first', 'then'))}/4"
    )
    print(
        f"  observed — final turn length  : {len(transcript[-1]) if transcript else 0} chars"
    )
    ok = c1 and c2 and c3 and c4
    print(
        f"\n  RESULT: {'PASS — it built something that outlives the conversation' if ok else 'FAIL'}"
    )
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
    banner: str = "build a sub-AI that survives the conversation",
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
    root, core, close_kernel = await open_kernel(
        "subai_", reach=sorted(MINE - {"kernel_state", "core"})
    )
    store = await build_fixture(root, interpreter=interpreter)
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
                        think=False,
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
                    "payload": _ai_meta("ai", think=True, store=store, root=root),
                }
            ]
        )
    )[0]["result"]
    aid = ai["id"]

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
                    f"last_turn_tokens={d.get('last_turn_tokens')} "
                    f"head={d.get('last_turn_head')!r} **\n",
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

        async def chat(self, messages):
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
                    if ev.get("type") == "context" and ev.get("phase") == "too_small":
                        print(f"\n  ** TOO_SMALL: {det} **\n", flush=True)
            except Exception as e:
                print(f"  ** event watch unavailable: {e} **", flush=True)
            finally:
                await watcher.close()

        _errtask = asyncio.create_task(_wire_errspy())
    else:
        if BACKEND == "anthropic":
            import anthropic_backend.tools as bt
        else:
            import llamacpp_backend.tools as bt
        spy_obj = Spy(bt.make_provider(aid, root))
        core._providers[aid] = spy_obj

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
    while idle < IDLE:
        await asyncio.sleep(2.0)
        writes = _progress_count(spy.transcript)
        if writes != seen_writes:
            seen_writes, stale = writes, 0
        elif spy.n != last:
            stale += 1
        # CHAIN DEATH. The tripwires examine each TURN, so a model that simply
        # stops producing turns evades every one of them and the run sits out the
        # full IDLE before scoring. Measured 2026-08-24: test 5 ended at turn 6 and
        # waited 240s to say so. If nothing of the AI's is in flight and no new
        # turn has landed, the chain is over — say so now rather than waiting.
        if idle >= 30 and spy.n == last and last > 0:
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
            _live = [
                t for t in _rows if t.get("agent") == aid and t.get("verb") != "ps"
            ]
            if not _live:
                # DUMP THE TABLE. "no turn and nothing in flight" is a conclusion;
                # the rows are the evidence. Without them a chain that ended and a
                # detector that fired too early look identical from the transcript.
                capped = (
                    f"CHAIN ENDED — no turn and nothing in flight after {idle:.0f}s"
                    f" · ps={[(t.get('agent'), t.get('verb')) for t in _rows]}"
                )
            else:
                # A generation IS in flight — activity, not idleness. Without this
                # hold the `while idle < IDLE` exited SILENTLY mid-generation (same
                # defect `helpers/ai.py` fixed 2026-09-01; measured here 2026-09-02:
                # a 33k-char turn regenerating after a fence refusal died at 60s
                # with no cap line). Held at 28 so the check re-runs each tick.
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
            idle += 2.0
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
    print(f"\n  builder finished after {spy.n} passes — walking away now")

    # THE TRANSCRIPT THE SCORER READS MUST BE THE ONE THAT WAS FILLED.
    # `transcript` is this probe's own list, appended by the in-process provider
    # wrapper above. On a SPAWNED kernel there is no provider to wrap: the reader
    # is a `WireTranscript` holding its OWN list, and the local one stays empty —
    # so every rust and swift run scored `read the docs: False`, `planning
    # language: 0/4`, `final turn length: 0 chars` and `writes=0` no matter what
    # the model did. Measured 2026-09-03: the rust lane's own turns mention the
    # readme twice and the log holds nine of them, all of it scored as silence.
    return await score(
        root, store["id"], total, total + CANARY_VALUE, spy.transcript
    )


# Guarded so the harness selftest can IMPORT the fixture + scoring instead of
# duplicating them — an unguarded `asyncio.run` here made importing it run the whole
# probe, against a llama-server that was not up.
if __name__ == "__main__":
    asyncio.run(main())
