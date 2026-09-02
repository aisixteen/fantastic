"""LIVE TEST 4 — plan, BUILD a program, and have it actually run.

    cd src/lib/python && BACKEND=llamacpp uv run python -u ../../../tmp/live_build.py

Harder than `live_plan.py` in the direction that matters: `live_plan` asked for a
CHANGE (a record edit the model could make with one verb). This asks for an
ARTIFACT — a durable program (a FILE the model wrote, plus a command that ran it)
that totals the readings and stores the total — and then refuses to believe it
exists until it has been watched doing its job on input it has never seen.

Nothing about the API is given. The task names a CAPABILITY the way a person would.
Which command executor this runtime has, how a program's bytes reach a file, how a
running program reaches another agent — all of it is in the kernel's own docs
(`reflect readme:'self-extension'`), and DISCOVERING that is half the task. Spelling
it out here would be dictation, and dictation is not emergence.

SCORING is three HARD checks, and the third is the one with teeth:

  1. AN ARTIFACT EXISTS — a program file the model wrote into the store, and a
     command that actually ran it. Not "it said it built one".
  2. THE ANSWER IS RIGHT — the memory agent holds the true total. The numbers are
     minted with `secrets` at test time, so the total cannot be known, guessed, or
     recalled from training; a model that fabricates cannot land on it.
  3. THE CANARY — the harness writes a FOURTH file with a fresh random number and
     RE-RUNS the program (the same `start` command, against the file on disk). The
     stored total must become the NEW true total. This separates the two
     ways to satisfy (2): a model that read the three files in its own turns and wrote
     the sum itself passes (1) and (2) and FAILS HERE, and so does a program that
     hardcoded the number it happened to see.
     It asks for a PROGRAM, not a service. Nothing has to stay running between the
     model's turn and this one — an earlier revision demanded that, which was the
     harness inventing a requirement the task never stated and then grading against it.

SOFT — observed, never asserted: did a plan artifact appear anywhere, how much
planning language showed up, and whether the last turn reported back in prose.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import secrets
import shlex
import sys
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
    Limits,
    Watch,
    connect,
    distinct_calls,
    verdict,
)
from helpers.kernel import open_kernel

# The plug, resolved once (see e2e/backends): which model, reached how.
BK = _resolve_backend()
BACKEND, MODEL = BK.name, BK.model
NVIDIA_KEY = os.environ.get("NVIDIA_API_KEY", "")
LLAMACPP_ENDPOINT = os.environ.get("FANTASTIC_LLAMACPP_ENDPOINT")  # no guessed host
CTX = int(os.environ.get("CTX", "262144"))
MAX_TOKENS_FIELD = int(os.environ.get("MAX_TOKENS", "0"))
IDLE = float(os.environ.get("IDLE", "60" if BACKEND == "anthropic" else "240"))
# A CEILING. Without one these harnesses end only on IDLE: runs have reached turn
# 77 and 89, both killed by hand, both re-verifying work already done. 0 = no cap.
MAX_TURNS = int(os.environ.get("MAX_TURNS", "40"))
NO_PROGRESS = int(os.environ.get("NO_PROGRESS", "8"))
TURN_CHARS = int(os.environ.get("TURN_CHARS", "120000"))
TURN_SECONDS = float(os.environ.get("TURN_SECONDS", "600"))
# How long the CANARY waits for the authored program to react to the new file.
# Every bound in ONE object, carrying this probe's own tuned values.
LIM = Limits(
    idle=IDLE,
    max_turns=MAX_TURNS,
    no_progress=NO_PROGRESS,
    turn_chars=TURN_CHARS,
    turn_seconds=TURN_SECONDS,
    ctx=CTX,
    show_system=os.environ.get("SHOW_SYSTEM") == "1",
)
CANARY_WAIT = float(os.environ.get("CANARY_WAIT", "180"))
W = 78

# Minted HERE, at test time. Nothing in any training set contains these, so the
# total is unreachable except by actually reading the files.
READINGS = {f"reading_{i}.txt": secrets.randbelow(9000) + 1000 for i in range(1, 4)}
# NOT "reading_4.txt". The model's own plan writes a self-test reading, and the
# natural next name IS reading_4 — measured 2026-08-25: the canary OVERWROTE the
# model's 1000-value file with its own 4695/9786/1109, the artifact recomputed the
# disk sum EXACTLY right (store == disk to the digit, three runs), and the harness
# failed it against a total that double-counted the clobbered file. A canary must
# ADD a reading nobody could have written.
CANARY_FILE = "canary_reading.txt"
CANARY_VALUE = secrets.randbelow(9000) + 1000

# The pre-wired command-runner leg — the operator's half of self-extension (see
# `wire_executor_leg`). The model does not build this; it DISCOVERS it and runs its
# program on it. HARD 1 looks for the program FILE + a job on this leg, so the leg
# itself is never mistaken for the artifact.
EXECUTOR_LEG = "runner"

TASK = (
    "Your goal is to build something durable in this kernel that totals a directory "
    "of sensor readings and remembers the total. This kernel has an agent holding "
    "that directory — one number per file — and a memory agent for durable state. "
    "DONE means all three: (1) what you build PERSISTS in this kernel — a program "
    "that stays and can be run again, not a one-off action you performed yourself by "
    "hand; (2) the correct total is stored "
    "in the memory agent; (3) when a new reading file appears later and the thing you "
    "built is run again, the stored total updates to include it. Work out how to "
    "build that here, build it, make sure it really runs, and tell me what you made. "
    # COMPLETION PRESSURE — same clause 03 needed. Measured 2026-08-20/21: without an
    # explicit "keep going until it works", models analyse, plan, report, and stop.
    # The "or certain it is impossible" half supplies the EXIT so this cannot become
    # a verify-forever loop.
    "Do not stop until it actually works — built and run, not merely planned — or "
    "you are certain it is impossible, in which case say so and say why."
)


def box(title: str, body: str, mark: str = "|") -> None:
    print(f"\n{title}")
    for raw in (body or "(EMPTY)").splitlines() or [""]:
        for i in range(0, len(raw), W):
            print(f"  {mark} {raw[i : i + W]}")


def _served_model() -> str:
    """What the llama-server actually has loaded — for the banner and the transcript.

    The probe does not name the model on this backend, so it asks the server's OpenAI
    `/models` surface. Discovery, not a guess. The transcripts are kept as evidence and
    one that misnames its own model is worse than one with no name at all, so a failure
    to reach the server is reported inline rather than silently filled in.
    """
    import urllib.error
    import urllib.request

    if not LLAMACPP_ENDPOINT:
        return "<FANTASTIC_LLAMACPP_ENDPOINT unset>"
    try:
        with urllib.request.urlopen(f"{LLAMACPP_ENDPOINT}/models", timeout=5) as r:
            data = json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
        return f"<llama-server at {LLAMACPP_ENDPOINT} did not name its model: {e}>"
    served = data.get("data") or []
    return served[0].get("id", "<unnamed>") if served else "<no model loaded>"


async def discover_command_runner(root) -> tuple[str, list[str]]:
    """The command-runner leg and the commands it permits, found the way a MODEL finds them.

    A durable program here is a FILE plus a command that runs it. The command runner
    is a general-purpose executor gated by `allowed` regexes over the full command
    line (no default — `start` refuses when the record lists none). So "who runs
    commands" and "what may run" are two unguessable facts a model discovers by the
    same two hops it always has: `list_agents` for the tree, then each agent's own
    `reflect`, which advertises `allowed`. The ONE leg advertising a non-empty
    `allowed` IS the runner — no naming convention, no handler_module sniff (that was
    the harness-asserts-the-wrong-surface scar the emergence README records).
    """
    listing = (
        (
            (
                await root.send(
                    [{"target": "kernel", "payload": {"type": "list_agents"}}]
                )
            )[0]["result"]
        ).get("agents")
        or []
    )
    for rec in listing:
        r = (await root.send([{"target": rec["id"], "payload": {"type": "reflect"}}]))[
            0
        ]["result"]
        allowed = r.get("allowed") or []
        if allowed:
            return rec["id"], list(allowed)
    raise AssertionError(
        "no agent in this kernel advertises an `allowed` command — nothing can run a "
        "program, so this probe is unwinnable. Wire a command runner with "
        "`allowed=[<regex>, ...]` first."
    )


async def advertised_commands(root) -> list[str]:
    """The command regexes this kernel will run — the patterns alone (see discover_command_runner)."""
    _leg, pats = await discover_command_runner(root)
    return pats


async def wire_executor_leg(
    root, agent_id: str = EXECUTOR_LEG, interpreter: str | None = None
) -> str:
    """The OPERATOR's half: ONE command-runner leg with an interpreter allowlist.

    Which interpreter may run is the operator's call, never the model's (the doc says
    so; the bundle refuses a per-call interpreter choice). The fixture makes the
    choice a fixture makes — `sys.executable`, a FACT about this harness process, not
    a guess at a path — and allowlists running it against a program FILE. The model's
    job is to FIND the leg (`discover_command_runner` reads it the same way), WRITE a
    program file into the store, and run `<interp> <file>` past the gate.

    `cwd` is pinned to the run dir so the file the model writes into the store and the
    program's own `readings/` both resolve by relative name — store and readings agent
    are rooted in this same dir. `interpreter` overrides the choice (the side-venv arm
    of `live_graph_shape`).
    """
    interp = interpreter or sys.executable
    r = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "exec_bridge.tools",
                        "id": agent_id,
                        "cwd": os.getcwd(),
                        "ingress_rule": "allow_all_ingress",
                        # Two-sided deny-all: the AI reflects + starts on this leg. The wire
                        # surfaces too — the harness DISCOVERS the leg the same way the model
                        # does (reflect), and over REST that reflect arrives as `rest`.
                        "callable_by": {"ai": "*", "rest": "*", "web": "*"},
                        # THE GATE: run THIS interpreter against a file argument. A model reflects
                        # the leg, reads the exact interpreter path here, and runs `<interp> <file>`.
                        "allowed": [rf"{re.escape(interp)} \S+"],
                        # The running program reaches the memory agent over its control wire; that
                        # send lands FROM this leg (its owner), so the leg needs egress to totals.
                        "can_call": {"totals": "*"},
                        "description": "Process edge: the command(s) this kernel may spawn.",
                    },
                }
            ]
        )
    )[0]["result"]
    assert not r.get("error"), f"could not wire the command runner: {r}"
    return r["id"]


def _disk_total() -> int:
    """The CURRENT sum of the readings directory — truth recomputed at check time.

    Grading against the seed-time snapshot scored a WORKING system FAIL on
    2026-08-25 (the model): the model's own plan said "write a new reading
    file to test requirement 3", it added one worth 500, and its stored total
    honestly included it — 18268 vs the frozen 17768. The task's words are
    "keep the total up to date"; the correct total is whatever the directory
    holds when the check runs, so the scorer reads the same files the model's
    program does."""
    out = 0
    for name in os.listdir("readings"):
        try:
            with open(os.path.join("readings", name)) as fh:
                out += int(str(fh.read()).strip())
        except (ValueError, OSError):
            continue
    return out


def _last_successful_job(jobs: list[dict]) -> dict | None:
    """The runner's most recent job that exited 0, carrying its own `command`/`args`.

    The durable artifact is "the command the model ran and proved works", not "a
    file on disk" — a model may write several programs (an exploratory probe, then
    the real one), so guessing by filename picks the wrong one. The runner records
    each job's exact invocation (`status` reports `command`/`args`); the LAST one
    that succeeded is what the model most recently demonstrated, and it is what the
    canary re-issues verbatim — no filename heuristic, no interpreter assumption."""
    done = [j for j in jobs if j.get("exit_code") == 0 and j.get("command")]
    return max(done, key=lambda j: j.get("started_at", 0), default=None)


async def find_value(
    root, mem_id: str, want: int
) -> tuple[str | None, str | None, bool]:
    """Memory is FILES: scan the run's tree for `want` — the task never dictates
    a path or a format (doing so would be dictation), so the check is "does any
    document hold this number", word-bounded so 14567 never matches 114567.

    The SKIP LIST is what keeps the scan honest — places where the number
    appears without having been STORED by the model:
      dirs: `readings/` (the inputs the number is derived from), `.fantastic/`
      (kernel records + chat sidecars — the number appears in conversation),
      `.ckpt/` (pre-images), `__pycache__`;
      files: `*.py` (the model's program may embed constants), `*.jsonl`
      (transcripts), `*.tmp.*` (in-flight commit debris).
    `.json` is deliberately NOT skipped — models legitimately store results as
    JSON documents (measured 2026-09-01: `sensor_total.json` held the right
    total and a scorer draft that skipped .json scored it None).

    Returns (path, snippet, matched); on no match, the first candidate seen so
    a wrong answer is still visible. `mem_id` is unused — kept for call-site
    stability."""
    _ = mem_id
    skip_dirs = {"readings", ".fantastic", ".ckpt", "__pycache__", "repo"}
    first: tuple[str | None, str | None] = (None, None)
    pat = re.compile(rf"\b{want}\b")
    for dirpath, dirnames, filenames in os.walk("."):
        dirnames[:] = [d for d in dirnames if d not in skip_dirs]
        for name in sorted(filenames):
            if name.endswith((".py", ".jsonl")) or ".tmp." in name:
                continue
            fp = Path(dirpath) / name
            try:
                if fp.stat().st_size > 20480:
                    continue
                text = fp.read_text(errors="ignore")
            except OSError:
                continue
            rel = str(fp).removeprefix("./")
            snippet = text.strip()[:120]
            if first == (None, None) and snippet:
                first = (rel, snippet)
            if pat.search(text):
                return rel, snippet, True
    return first[0], first[1], False


# A WRITE is a call that changes the world: a record mutation, or a store/disk/exec
# verb. Reads and reflects are not progress — a model can emit them forever.
# Progress = a call not made before. The shared definition; the per-probe
# copies of it drifted (one counted writes only and cut a probe mid-work).
_progress_count = distinct_calls


async def main():
    """Arm A of the A/B: the default fixture, the default task."""
    return await run()


async def run(
    *,
    task: str = TASK,
    interpreter: str | None = None,
    extra_agents=None,
    banner: str = "plan / BUILD a program / prove it runs",
) -> bool:
    """The whole probe, parameterized so a variant reuses this fixture AND this scoring.

    `live_graph_shape.py` runs it twice with one field different. Sharing the function is
    the point: two copies of the scoring would let the arms diverge silently, which is
    the failure mode the emergence README already records twice.
    """
    # WHICH KERNEL is a plug (`RUNTIME=python|rust|swift`), exactly as the
    # model is a plug in `backends/`. In process for python; spawned and driven
    # over REST otherwise. `core` is None on the wire and the connector reads
    # the transcript from the agent's history instead of wrapping a provider.
    # The ids this probe's FIXTURE builds, plus the peers it grants the AI.
    # Declared here because a spawned kernel's REST leg is granted from ABOVE,
    # before boot — it cannot raise its own authority later. In process the
    # probe IS the operator and the list is ignored.
    root, core, close_kernel = await open_kernel(
        "build_",
        reach=["kernel_store", "readings", "totals", EXECUTOR_LEG, "ai", "cli"],
    )

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
                        # The RUN DIR itself: a program written here lands beside readings/ and in
                        # the command runner's cwd, so `<interp> <file>` and open('readings/..') both
                        # resolve by relative name. (The store also holds the AI's chat sidecars.)
                        "root": ".",
                        "ingress_rule": "allow_all_ingress",
                        # Two-sided deny-all: the AI (and its authored tools, which it opens via
                        # its own update_agent grant) reach this store — accept the AI directly.
                        # The wire surfaces too: over REST/WS the harness's own reads arrive AS
                        # `rest`/`web`, and ingress is two-sided (inert in-process — no such ids).
                        "callable_by": {
                            "ai": "*",
                            "totals": "*",
                            "rest": "*",
                            "web": "*",
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    # The readings live behind an OPEN file agent — this probe is about building a
    # program, not about guessing an auth posture (that is `live_plan.py`'s job).
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
                        "root": "readings",
                        "ingress_rule": "allow_all_ingress",
                        "description": "Sensor readings — one integer per file.",
                        # Two-sided deny-all: accept the AI + the wire surfaces (harness reads
                        # arrive as `rest`/`web` on a spawned kernel; inert in-process).
                        "callable_by": {"ai": "*", "rest": "*", "web": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    mem = (
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
                        "description": "Durable store for computed results.",
                        # Two-sided deny-all: accept the AI directly, AND the command runner — the
                        # program's `set` lands from the runner leg (its owner) over the control wire.
                        "callable_by": {
                            "ai": "*",
                            EXECUTOR_LEG: "*",
                            "rest": "*",
                            "web": "*",
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    # The executor leg, open, with an interpreter allowlist. Without it there is no
    # allowlisted interpreter anywhere in the kernel and NOTHING can be authored — the
    # spawner has no default and refuses rather than guessing a path.
    await wire_executor_leg(root, interpreter=interpreter)
    if extra_agents is not None:
        await extra_agents(root)

    # Two-sided deny-all: the operator grants the AI its egress toolset (full operator
    # power over the root incl. privileged verbs, + the pre-created peers + cli). The
    # peers reciprocally ACCEPT the AI (their `callable_by:{ai:*}` above; cli opened
    # below). NOTE: a tool the AI AUTHORS is a SEPARATE agent carrying only the baseline
    # grant — to reach readings/totals it must (a) grant its OWN can_call in its
    # create_agent AND (b) open those peers' `callable_by` for it (it holds the
    # kernel_state update_agent grant, so it can). If an authored tool can't reach a
    # peer, that is the two-sided doc lesson surfacing, not a harness bug.
    CAN_CALL = {
        "$self": "*",
        "$parent": "*",
        "$children": "*",
        # THE ROOT, by whatever id this runtime gives it — python's root IS the
        # persistence loader (`kernel_state`), rust's and swift's is `core`.
        root.id: ["*", "create_agent", "update_agent", "delete_agent"],
        "cli": "*",
        "readings": "*",
        "totals": "*",
        EXECUTOR_LEG: "*",  # to reflect its `allowed` gate + start the program
        store["id"]: "*",
    }
    # The sidecar is asked for as a CAPABILITY, never by backend name — a probe
    # that branches on `BACKEND ==` is a probe that has stopped being agnostic.
    # ...and the RUNTIME, which is a separate question the plug cannot answer:
    # python's anthropic backend persists its history through the loader and
    # needs no leg, while rust's and swift's refuse `send` outright with
    # "file_bridge_id required". One bundle, two contracts — recorded here
    # rather than papered over, and the store exists either way so binding it
    # costs nothing.
    # ALWAYS. The AI's chat store persists through ITS OWNER'S leg and nothing
    # else — the loader path died with yaml_state — so an AI without a
    # `file_bridge_id` runs AMNESIAC: every turn starts with empty history.
    # Measured 2026-08-30 on plan:python — turn 6 said "I don't see any prior
    # conversation context" and walked away from a task it had been doing.
    EXTRA = {"file_bridge_id": store["id"]}

    # ONE connector (helpers/ai.py). It creates the agent, opens `cli`'s reciprocal
    # ingress, hands the plug its key where one is needed, subscribes to the AI's own
    # status events so a dead chain announces itself instead of looking like a model
    # that chose to stop, and installs the shared reader. Seven probes each carried
    # their own copy of all that, and every copy was behind the others.
    watch = Watch()
    conn = await connect(
        root, core, BK, can_call=CAN_CALL, limits=LIM, watch=watch, record=EXTRA
    )
    aid, spy = conn.id, conn.proxy
    runaway, transcript = conn.trips, spy.transcript

    total = sum(READINGS.values())
    print("=" * (W + 4))
    print(f"  {_served_model() if BACKEND == 'llamacpp' else MODEL}  ·  {banner}")
    print(f"  {len(READINGS)} readings, minted at test time.  true total = {total}")
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
    # The reader `connect()` installed — the provider table exists only in
    # process, and `conn.proxy` is the same object on either transport.
    spy = conn.proxy
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
        writes = _progress_count(transcript)
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
            # AN IN-FLIGHT GENERATION IS NOT IDLE — see live_subai.py (measured
            # 2026-08-25: the loop walked away mid-generation and scored FAIL).
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

    print("\n" + "=" * (W + 4))
    print(f"  passes: {spy.n}\n")

    # --- HARD 1: an ARTIFACT exists ---
    # A durable program is a FILE the model wrote plus a COMMAND that ran it — not a
    # new agent (nothing carries `code` any more). The evidence is a job the runner
    # actually completed successfully, WHOSE COMMAND it records: that command IS the
    # artifact, and the canary re-issues it verbatim.
    runner_id, _pats = await discover_command_runner(root)
    st = (await root.send([{"target": runner_id, "payload": {"type": "status"}}]))[0][
        "result"
    ]
    jobs = (st or {}).get("jobs") or []
    job = _last_successful_job(jobs)
    has_artifact = job is not None
    program = (
        None if job is None else shlex.join([job["command"], *(job.get("args") or [])])
    )
    print(
        f"    1. artifact exists          {'OK' if has_artifact else 'NO'} "
        f"ran={program!r} runner={runner_id!r} jobs={len(jobs)}"
    )

    # --- HARD 2: the answer is right ---
    # Truth is recomputed from DISK, not the seed snapshot — the model may have
    # added readings of its own (it is allowed to test its work), and the task
    # asks for the total to be UP TO DATE. See `_disk_total`.
    true_now = _disk_total()
    key, val, answer_ok = await find_value(root, mem["id"], true_now)
    print(
        f"    2. total is correct         {'OK' if answer_ok else 'NO'} "
        f"stored={val!r} key={key!r} true={true_now}"
    )

    # --- HARD 3: THE CANARY — does the thing the model built actually RUN? ---
    canary_ok = False
    if has_artifact:
        with open(os.path.join("readings", CANARY_FILE), "w") as fh:
            fh.write(f"{CANARY_VALUE}\n")
    # Expected sum computed AFTER the write, from the same disk the program reads —
    # arithmetic on top of a pre-write snapshot broke every time the directory had
    # changed in between (see CANARY_FILE).
    new_total = _disk_total()
    if has_artifact:
        # RE-RUN exactly what the model last ran — the runner recorded the job's
        # `command`/`args`, so this re-issues that invocation verbatim (no filename
        # guess, no interpreter assumption). It proves THE PROGRAM works against the
        # new file and does NOT require a live service: the task asked for a program,
        # not a daemon, and demanding one would be the harness inventing a requirement
        # and grading against it.
        _cs = (
            await root.send(
                [
                    {
                        "target": runner_id,
                        "payload": {
                            "type": "start",
                            "command": job["command"],
                            "args": job.get("args") or [],
                        },
                    }
                ]
            )
        )[0]["result"]
        waited = 0.0
        while waited < CANARY_WAIT and not canary_ok:
            await asyncio.sleep(3.0)
            waited += 3.0
            _k, _v, canary_ok = await find_value(root, mem["id"], new_total)
        if not canary_ok:
            # SAY WHY. The re-run's reply and its job row were discarded before
            # 2026-08-25, so a canary NO named nothing — the exact artifact later
            # replayed CLEAN offline and the live cause was unrecoverable.
            print(f"       canary start reply: {_cs!r}")
            if isinstance(_cs, dict) and _cs.get("job_id"):
                _js = (
                    await root.send(
                        [
                            {
                                "target": runner_id,
                                "payload": {"type": "status", "job_id": _cs["job_id"]},
                            }
                        ]
                    )
                )[0]["result"]
                for _j in (_js or {}).get("jobs") or []:
                    print(
                        f"       canary job: status={_j.get('status')!r} "
                        f"exit={_j.get('exit_code')!r} "
                        f"stderr={(_j.get('stderr') or '')[:400]!r} "
                        # stdout carries the guest's OWN prints — and nothing
                        # else: a refused send returns an error DICT the code may
                        # silently ignore, which is exactly the invisible case.
                        f"stdout={(_j.get('stdout') or '')[:400]!r}"
                    )
            # DUMP THE MEMORY — files now: list every candidate document the
            # scorer's scan can see, so a canary NO says what was actually there.
            for _dp, _dn, _fn in os.walk("."):
                _dn[:] = [
                    d
                    for d in _dn
                    if d not in ("readings", ".fantastic", ".ckpt", "__pycache__")
                ]
                for _n in sorted(_fn):
                    if _n.endswith((".py", ".jsonl")) or ".tmp." in _n:
                        continue
                    _fp = Path(_dp) / _n
                    try:
                        print(
                            f"       file[{_fp}] = {_fp.read_text(errors='ignore').strip()[:200]!r}"
                        )
                    except OSError:
                        pass
    print(
        f"    3. it RUNS (canary)         {'OK' if canary_ok else 'NO'} "
        f"new file={CANARY_VALUE} expected={new_total}"
    )

    # --- SOFT: observed, never asserted ---
    joined = " ".join(transcript).lower()
    plan_words = sum(w in joined for w in ("plan", "step", "first", "then"))
    last_turn = transcript[-1] if transcript else ""
    print(f"\n  observed — planning language        : {plan_words}/4 markers")
    print(f"  observed — memory keys after run    : {json.dumps(key)}")
    print(f"  observed — final turn length        : {len(last_turn)} chars")

    ok = has_artifact and answer_ok and canary_ok
    print(f"\n  RESULT: {'PASS — it built a program that runs' if ok else 'FAIL'}")
    verdict(ok)
    return ok


if __name__ == "__main__":
    # GUARDED so the selftest can import this module for its fixture + helpers without
    # launching a live run. `live_subai.py` had the identical defect and it cost a
    # confusing debugging round: importing the probe silently started it.
    asyncio.run(main())
