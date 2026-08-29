"""LIVE TEST 4 — plan, BUILD a program, and have it actually run.

    cd src/lib/python && BACKEND=llamacpp uv run python -u ../../../tmp/live_build.py

Harder than `live_plan.py` in the direction that matters: `live_plan` asked for a
CHANGE (a record edit the model could make with one verb). This asks for an
ARTIFACT — a durable script agent whose persisted `code` IS a working program —
and then refuses to believe it exists until it has been watched doing its job on
input it has never seen.

Nothing about the API is given. The task names a CAPABILITY the way a person would.
Which executor this runtime has, how `code` is persisted, how a script receives a
turn, how it reaches another agent — all of it is in the kernel's own docs
(`reflect readme:'self-extension'`), and DISCOVERING that is half the task. Spelling
it out here would be dictation, and dictation is not emergence.

SCORING is three HARD checks, and the third is the one with teeth:

  1. AN ARTIFACT EXISTS — a script agent the model created, carrying non-empty
     `code`. Not "it said it built one".
  2. THE ANSWER IS RIGHT — the memory agent holds the true total. The numbers are
     minted with `secrets` at test time, so the total cannot be known, guessed, or
     recalled from training; a model that fabricates cannot land on it.
  3. THE CANARY — the harness writes a FOURTH file with a fresh random number and
     RE-RUNS the persisted program (`{"type":"start"}` falls back to the record's own
     `code`). The stored total must become the NEW true total. This separates the two
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
import sys
import time
import tempfile
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(0, str(next(d for d in Path(__file__).resolve().parents
                            if (d / ".e2e-root").exists())))
from helpers import (  # noqa: E402
    BASELINE, Limits, Watch, connect, distinct_calls, drive, verdict,
)
from helpers.kernel import open_kernel  # noqa: E402
from backends import backend as _resolve_backend  # noqa: E402


# The plug, resolved once (see e2e/backends): which model, reached how.
BK = _resolve_backend()
BACKEND, MODEL = BK.name, BK.model
NVIDIA_KEY = os.environ.get("NVIDIA_API_KEY", "")
LLAMACPP_ENDPOINT = os.environ.get(
    "FANTASTIC_LLAMACPP_ENDPOINT", "http://127.0.0.1:8080/v1"
)
CTX = int(os.environ.get("CTX", "32768"))
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
LIM = Limits(idle=IDLE, max_turns=MAX_TURNS, no_progress=NO_PROGRESS,
             turn_chars=TURN_CHARS, turn_seconds=TURN_SECONDS, ctx=CTX,
             show_system=os.environ.get("SHOW_SYSTEM") == "1")
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

# The pre-wired executor leg — the operator's half of self-extension (see
# `wire_executor_leg`). It is FIXTURE, so scoring must never mistake it for the
# artifact the model built: its handler_module is the executor's, which is exactly what
# HARD 1 looks for.
EXECUTOR_LEG = "runner"

TASK = (
    "Your goal is to build something durable in this kernel that totals a directory "
    "of sensor readings and remembers the total. This kernel has an agent holding "
    "that directory — one number per file — and a memory agent for durable state. "
    "DONE means all three: (1) what you build PERSISTS as an agent in this kernel, "
    "not as a one-off action you performed yourself; (2) the correct total is stored "
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

    try:
        with urllib.request.urlopen(f"{LLAMACPP_ENDPOINT}/models", timeout=5) as r:
            data = json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
        return f"<llama-server at {LLAMACPP_ENDPOINT} did not name its model: {e}>"
    served = data.get("data") or []
    return served[0].get("id", "<unnamed>") if served else "<no model loaded>"


async def advertised_executors(root) -> set[str]:
    """The executor module(s) this kernel ADVERTISES, read the way a MODEL reads them.

    This used to be `"runtime" in handler_module`, and that is the
    harness-asserts-the-wrong-surface scar the emergence README records: it encodes a
    NAMING CONVENTION, so renaming the bundle scores a working system FAIL. Worse here
    than elsewhere — HARD 3 (the canary, the point of this probe) is gated on HARD 1, so
    a missed match means the canary never runs and still reports NO.

    Read the DISTILLED reflect: only it interpolates a real module name (the full one says
    `'<an executor below>'`), and it is the same string the model is given, so the harness
    and the model discover the executor by the same path.
    """
    se = ((await root.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0]["result"]).get("self_extension") or ""
    found = set(re.findall(r"handler_module:'([a-z_]+\.[a-z_]+)'", se))
    if not found:
        # NOT A HARNESS FAILURE — a kernel that advertises no executor cannot be
        # self-extended by a model, which is the whole subject of this probe.
        # Measured 2026-08-29: python interpolates the real module
        # (`exec_bridge.tools`); rust ships the PLACEHOLDER `'<an executor
        # below>'` uninterpolated; swift's `self_extension` is empty. Scored as
        # a plain FAIL naming what is missing, rather than crashing the runner
        # with a traceback that reads as a broken test.
        print("\n  ** the kernel's DEFAULT reflect names no executor **")
        print(f"  self_extension={se[:300]!r}")
        verdict(False, "kernel advertises no executor — self-extension undiscoverable")
        raise SystemExit(1)
    return found


async def advertised_interpreters(root) -> list[str]:
    """The interpreter paths this kernel will spawn, read the way a MODEL reads them.

    The advertised executor is an io leg with an interpreter ALLOWLIST and NO default —
    `start` refuses when the record lists none — so "which interpreter" is a second
    unguessable fact next to the executor's name. Two hops, both of them a model's:
    `list_agents` to find who carries the advertised executor's handler_module (the
    DISTILLED tree does not carry it — only id/sentence/description, and the leg's
    sentence is what points a reading model at it), then that agent's own `reflect`,
    which is where the allowlist is advertised.

    NOT `sys.executable`. That is a fact about the harness process, and hardcoding it
    would prove the task is SOLVABLE while stepping over whether the interpreter is
    FINDABLE — the same half-measure that let this selftest pass while the probe it
    validates was unwinnable.
    """
    execs = await advertised_executors(root)
    listing = ((await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]).get("agents") or []
    out: list[str] = []
    for rec in listing:
        if rec.get("handler_module") not in execs:
            continue
        r = (await root.send([{"target": rec["id"], "payload": {"type": "reflect"}}]))[0]["result"]
        out.extend(r.get("interpreters") or [])
    assert out, (
        "no executor agent in this kernel advertises an interpreter — the kernel "
        "refuses to guess one, so a model cannot author a tool and this probe is "
        "unwinnable. Wire an executor leg with `interpreters` first."
    )
    return out


async def wire_executor_leg(
    root, agent_id: str = EXECUTOR_LEG, interpreter: str | None = None
) -> str:
    """The OPERATOR's half of self-extension: ONE open executor leg with an allowlist.

    Choosing which interpreter may run is not the model's decision and never was — the
    bundle refuses per-call overrides for exactly that reason. So the fixture makes the
    choice a fixture makes it: `sys.executable`, which is a FACT about this harness
    process rather than a guess at a path on disk. The model's job is to FIND it
    (`advertised_interpreters` reads it back the same way) and author against it.

    `interpreter` overrides that choice — used by `live_graph_shape.py` (historically, to allowlist a
    side venv). It is the ONLY fixture difference between that probe's
    two arms, which is what makes the comparison single-variable.
    """
    executor = sorted(await advertised_executors(root))[0]
    r = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": executor,
            "id": agent_id,
            "ingress_rule": "allow_all_ingress",
            # Two-sided deny-all: opening the IO seal is not enough — the CALL gate
            # still has to accept the AI, or the one step self-extension mandates
            # ("reflect an existing executor for its `interpreters` allowlist") is
            # refused. Measured 2026-08-24: every `reflect runner` came back
            # `ai may not call 'reflect' on runner` with no dispatch, and the model
            # re-read the self-extension doc four times looking for another way.
            "callable_by": {"ai": "*"},
            "interpreters": [interpreter or sys.executable],
            "shim": "python",
            "description": "Process edge: the interpreter(s) this kernel may spawn.",
        }}]))[0]["result"]
    assert not r.get("error"), f"could not wire the executor leg: {r}"
    return r["id"]


def find_node(node: dict, pred) -> dict | None:
    if pred(node):
        return node
    for c in node.get("children", []):
        if (f := find_node(c, pred)) is not None:
            return f
    return None


def _disk_total() -> int:
    """The CURRENT sum of the readings directory — truth recomputed at check time.

    Grading against the seed-time snapshot scored a WORKING system FAIL on
    2026-08-25 (qwen3.8-27b): the model's own plan said "write a new reading
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


async def find_value(
    root, mem_id: str, want: int
) -> tuple[str | None, str | None, bool]:
    """Scan EVERY key for `want`. The task never dictates a key name — doing so would
    be dictation — so the check is "does the memory hold this number anywhere",
    not "is it under the name I picked". Returns (key, value, matched); on no match,
    reports the first key it saw so a wrong answer is still visible."""
    # `keys` is a table of contents — [{key, size}] — NOT a list of strings.
    entries = ((await root.send([{"target": mem_id, "payload": {"type": "keys"}}]))[0]["result"]).get("keys") or []
    keys = [e["key"] if isinstance(e, dict) else e for e in entries]
    first: tuple[str | None, str | None] = (None, None)
    for k in keys:
        v = ((await root.send([{"target": mem_id, "payload": {"type": "read", "key": k}}]))[0]["result"]).get("value")
        if v is None:
            continue
        sv = str(v).strip()
        if first == (None, None):
            first = (k, sv)
        # A RICH result counts. Measured 2026-08-05 (opus-5) and again 2026-08-25
        # (qwen3.8-27b, batch kernel): the model stored
        # {'details': {...}, 'files': 3, 'total': 14567} — the right number,
        # nested. `live_subai_selftest` recorded this scar; this scorer never got
        # the fix, so a correct answer scored NO. The number anywhere in the
        # value matches — word-bounded, so 14567 never matches 114567.
        if sv == str(want) or re.search(rf"\b{want}\b", sv):
            return k, sv, True
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
    root, core, close_kernel = await open_kernel("build_")

    store = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "kernel_store",
            "root": ".fantastic",
            "ingress_rule": "allow_all_ingress",
            # Two-sided deny-all: the AI (and its authored tools, which it opens via
            # its own update_agent grant) reach this store — accept the AI directly.
            "callable_by": {"ai": "*"},
        }}]))[0]["result"]
    # The readings live behind an OPEN file agent — this probe is about building a
    # program, not about guessing an auth posture (that is `live_plan.py`'s job).
    os.makedirs("readings", exist_ok=True)
    for name, val in READINGS.items():
        with open(os.path.join("readings", name), "w") as fh:
            fh.write(f"{val}\n")
    (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "readings",
            "root": "readings",
            "ingress_rule": "allow_all_ingress",
            "description": "Sensor readings — one integer per file.",
            "callable_by": {"ai": "*"},  # two-sided deny-all: accept the AI
        }}]))[0]["result"]
    mem = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "yaml_state.tools",
            "id": "totals",
            "file_bridge_id": store["id"],
            "description": "Durable store for computed results.",
            "callable_by": {"ai": "*"},  # two-sided deny-all: accept the AI
        }}]))[0]["result"]
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
        "kernel_state": ["*", "create_agent", "update_agent", "delete_agent"],
        "cli": "*",
        "readings": "*",
        "totals": "*",
        EXECUTOR_LEG: "*",  # to READ its interpreters allowlist, per self-extension
        store["id"]: "*",
    }
    # The sidecar is asked for as a CAPABILITY, never by backend name — a probe
    # that branches on `BACKEND ==` is a probe that has stopped being agnostic.
    EXTRA = {"file_bridge_id": store["id"]} if BK.needs_file_bridge else {}

    # ONE connector (helpers/ai.py). It creates the agent, opens `cli`'s reciprocal
    # ingress, hands the plug its key where one is needed, subscribes to the AI's own
    # status events so a dead chain announces itself instead of looking like a model
    # that chose to stop, and installs the shared reader. Seven probes each carried
    # their own copy of all that, and every copy was behind the others.
    watch = Watch()
    conn = await connect(root, core, BK, can_call=CAN_CALL, limits=LIM,
                         watch=watch, record=EXTRA)
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
    _first = (await root.send([{"target": aid, "payload": {"type": "send", "text": task, "client_id": "probe"}}]))[0]["result"]
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
            _rows = ((await root.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]["result"]).get("tasks") or []
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
            _t = ((await root.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]["result"]).get("tasks") or []
            for t in [x for x in _t if x.get("agent") == aid and x.get("verb") != "ps"]:
                _c = (await root.send([{"target": "kernel", "payload": {"type": "cancel", "task_id": t["id"]}}]))[0]["result"]
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
            _rows = ((await root.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]["result"]).get("tasks") or []
            if any(t.get("agent") == aid and t.get("verb") != "ps" for t in _rows):
                print(f"  … idle {idle:.0f}s but a turn is STILL GENERATING — waiting on", flush=True)
                idle = 0.0
    if capped:
        print(f"\n  ** CAPPED: {capped} · {spy.n} passes **")

    print("\n" + "=" * (W + 4))
    print(f"  passes: {spy.n}\n")

    # --- HARD 1: an ARTIFACT exists ---
    rec = (await root.send([{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]))[0]["result"]
    mine = {
        "kernel_state",
        "kernel_store",
        "readings",
        "totals",
        "ai",
        "cli",
        EXECUTOR_LEG,  # fixture, and it carries the executor's handler_module
    }
    execs = await advertised_executors(root)
    built = find_node(
        rec["tree"],
        lambda n: (n.get("id") not in mine and n.get("handler_module") in execs),
    )
    listing = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]
    records = {a["id"]: a for a in (listing.get("agents") or [])}
    code = (records.get(built["id"], {}).get("code") or "") if built else ""
    # An artifact is a script agent WITH a program in it. An empty shell is not a tool.
    has_artifact = built is not None and bool(code.strip())
    print(
        f"    1. artifact exists          {'OK' if has_artifact else 'NO'} "
        f"id={(built or {}).get('id')!r} code={len(code)} chars"
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
        # RE-RUN the persisted program. `start` with no `code` falls back to the
        # record's own `code` (the advertised executor's `_start`), so this proves THE
        # CODE works. It deliberately does NOT require the tool to be a live service:
        # the task asked for a program, not a daemon, and demanding one would be the
        # harness inventing a requirement and then grading against it.
        _cs = (await root.send([{"target": built["id"], "payload": {"type": "start"}}]))[0][
            "result"
        ]
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
                                "target": built["id"],
                                "payload": {"type": "status", "job_id": _cs["job_id"]},
                            }
                        ]
                    )
                )[0]["result"]
                for _j in (_js or {}).get("jobs") or []:
                    print(
                        f"       canary job: status={_j.get('status')!r} "
                        f"exit={_j.get('exit_code')!r} "
                        f"stderr={( _j.get('stderr') or '')[:400]!r} "
                        # stdout carries the guest's OWN prints — and nothing
                        # else: a refused send returns an error DICT the code may
                        # silently ignore, which is exactly the invisible case.
                        f"stdout={( _j.get('stdout') or '')[:400]!r}"
                    )
            # DUMP THE STORE. Twice now (2026-08-25) a canary NO left no way to
            # tell "the set never arrived" from "it stored the wrong value" —
            # the kernel is gone by the time anyone asks.
            _keys = (
                await root.send([{"target": mem["id"], "payload": {"type": "keys"}}])
            )[0]["result"]
            for _e in (_keys or {}).get("keys") or []:
                _kk = _e["key"] if isinstance(_e, dict) else _e
                _vv = (
                    await root.send(
                        [
                            {
                                "target": mem["id"],
                                "payload": {"type": "read", "key": _kk},
                            }
                        ]
                    )
                )[0]["result"]
                print(f"       store[{_kk!r}] = {str((_vv or {}).get('value'))[:200]}")
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
