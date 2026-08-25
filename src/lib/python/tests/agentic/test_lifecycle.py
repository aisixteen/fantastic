"""`lifecycle` — work in flight, and agents at their end of life.

Three claims, all made by ATTEMPTING the operation and reading what the caller
got back (`reach`), never by reading a record:

- **C14** a `cancel` stops a live dispatch BEFORE its next step, the blocked
  caller learns, the bundle gets to tear down first, and the table forgets it.
- **C15** both terminal states ABSORB — a cancelled task and a deleted agent
  answer every later call, and neither pretends the call did something.
- **C16** unknown id / unknown verb / malformed / gate denial stay four shapes a
  caller can tell apart WITHOUT knowing which agent it was talking to.

Not asserted here: the five preconfig invariants (shape, edges, closure, reboot,
verdict). `test_manifest_contract.py` walks those for every config, this one
included — repeating them here would be two truths about one file.

The canonical a2a wording for C15 ("an operation on a terminal task returns an
error") is FALSE in this kernel and is not smuggled in: a second `cancel`
answers `{"cancelled": false, "killed": []}` — an honest no-op, measured. A test
that asserted an error would pin a behaviour the kernel does not have.
"""

from __future__ import annotations

import asyncio

import pytest
from lifecycle_probe.tools import CANCEL_HOOK, PAST_THE_AWAIT, STARTED, reset

from _engine import check, dehydrate, denial_side, denied, reach

# `slowbot`'s bundle is `lifecycle_probe/`, a real package beside this file
# rather than a module object stitched into `sys.modules` here. That is not
# tidiness: the manifest's `handler_module` has to resolve at REBOOT too, inside
# `Kernel.load`, and a registration performed by a validator makes the reboot
# depend on which test file pytest happened to import. Measured — running the
# general runner alone dropped the record (`[kernel] skipping agent slowbot:
# bundle lifecycle_probe.tools not installed in this runtime`) and PC-4 went red
# for this config only.


# ─── fixtures ───────────────────────────────────────────────────


@pytest.fixture(autouse=True)
def probe_state():
    """The probe's recorders are process-wide and every test hydrates the SAME
    ids, so a stale `asyncio.Event` from a previous test would belong to a closed
    loop and never fire again."""
    reset()


@pytest.fixture
async def in_flight():
    """Caller tasks the test put in flight, cancelled at teardown INSIDE the
    loop. A failed assertion must not leave a 30-second sleep pending."""
    tasks: list[asyncio.Task] = []
    yield tasks
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)


async def _launch_slow(h, in_flight) -> tuple[asyncio.Task, str]:
    """Put one `slow` dispatch in flight AS warden; return (caller, pid).

    The caller is a real task because the point is that it BLOCKS — `send` does
    not return until the dispatch resolves, and WHAT it resolves to is the claim.
    The pid is discovered the way any caller discovers one: `ps` at the KERNEL,
    agent-origin. `ps`/`cancel` became kernel-owned on 2026-08-24 — they read and
    act on the kernel-wide task table, so the root answers them and the agent is
    named in the payload, not by being addressed. The cost, accepted knowingly:
    the manifest can no longer grant "warden may watch and kill SLOWBOT's work"
    as an edge. Reaching the kernel at all is now the whole permission."""
    started = STARTED.setdefault("slowbot", asyncio.Event())
    caller = asyncio.create_task(reach(h.kernel, "warden", "slowbot", {"type": "slow"}))
    in_flight.append(caller)
    await asyncio.wait_for(started.wait(), timeout=2)
    reply = await reach(h.kernel, "warden", h.kernel.id, {"type": "ps"})
    assert "tasks" in reply, f"ps through the granted kernel edge: {reply!r}"
    rows = reply["tasks"]
    slow = [r for r in rows if r["agent"] == "slowbot" and r["verb"] == "slow"]
    assert len(slow) == 1, f"expected exactly one in-flight slow dispatch: {rows}"
    return caller, slow[0]["id"]


async def _cancel(h, task_id) -> dict:
    """`cancel`, agent-origin, at the KERNEL. The task_id already names its agent
    (`<agent>:<token>`), which is why the verb never needed the target to say so.
    The gate still applies — warden reaches the kernel only because the manifest
    granted that edge — but it is now ONE edge covering every task, not a
    per-victim grant."""
    payload = {"type": "cancel"}
    if task_id is not None:
        payload["task_id"] = task_id
    return await reach(h.kernel, "warden", h.kernel.id, payload)


async def _kill(h, task_id) -> dict:
    """`_cancel` where the kill is the test's PRECONDITION rather than its claim.
    A plain assert, so a revoked grant reports as "the kill was refused" instead
    of surfacing two seconds later as a bare timeout on the blocked caller."""
    reply = await _cancel(h, task_id)
    assert reply.get("cancelled") is True, f"the kill was refused: {reply!r}"
    return reply


# ─── C14 · cancel stops the work, observably ────────────────────


async def test_a_cancelled_dispatch_never_runs_its_next_step_and_its_caller_learns(
    hydrate_scenario, in_flight
):
    """The two things "cancelled" has to mean, measured together.

    STOPPED: the handler's step after the await never runs. Asserting only that
    `cancel` returned `{cancelled: true}` would pin the bookkeeping and not the
    work — a task can be dropped from the table and still be running.

    THE CALLER LEARNS: cancellation never raises out of `send`
    (`_run_dispatch`); the blocked caller is RESOLVED with `{"cancelled": True}`,
    which is a different value from the handler's own `{"finished": True}`. A
    caller that could not tell those apart would report success for work that
    never happened."""
    h = await hydrate_scenario("lifecycle")
    caller, pid = await _launch_slow(h, in_flight)

    killed = await _cancel(h, pid)
    check(
        "C14",
        killed == {"cancelled": True, "killed": [pid]},
        f"cancel must name the pid it killed: {killed!r}",
    )

    reply = await asyncio.wait_for(caller, timeout=2)
    check(
        "C14",
        reply == {"cancelled": True},
        f"the blocked caller must be resolved, not raised at, and must NOT get"
        f" the handler's own reply: {reply!r}",
    )
    # Settle a beat: had the sleep merely been abandoned rather than aborted,
    # this is where the next step would show up.
    await asyncio.sleep(0.05)
    check(
        "C14",
        PAST_THE_AWAIT == [],
        f"the handler ran its step AFTER the await: {PAST_THE_AWAIT!r}",
    )

    reply = await reach(h.kernel, "warden", h.kernel.id, {"type": "ps"})
    assert "tasks" in reply, f"ps through the granted kernel edge: {reply!r}"
    rows = reply["tasks"]
    check(
        "C14",
        [r for r in rows if r["verb"] == "slow"] == [],
        f"the killed dispatch is still in the live table: {rows!r}",
    )


async def test_the_cancel_hook_runs_while_the_handler_it_tears_down_is_still_alive(
    hydrate_scenario, in_flight
):
    """SIGTERM before SIGKILL. Cancelling an `asyncio.Task` frees NO real
    resource — a subprocess, a PTY, a socket outlives it — so the substrate
    awaits the bundle's `on_cancel` for every task in the doomed subtree BEFORE
    aborting any of them (`Kernel.cancel_task`, two phases, no `await` in the
    abort loop).

    "Before" is the entire value, so it is what is asserted: the hook records
    whether its own task was still alive when it ran. A hook that ran after the
    abort could not release anything it was holding for that task."""
    h = await hydrate_scenario("lifecycle")
    caller, pid = await _launch_slow(h, in_flight)

    await _kill(h, pid)
    await asyncio.wait_for(caller, timeout=2)

    check(
        "C14",
        CANCEL_HOOK == [("slowbot", pid, True)],
        f"expected exactly one hook call, for {pid}, with its task still alive:"
        f" {CANCEL_HOOK!r}",
    )


# ─── C15 · terminal states absorb ───────────────────────────────


async def test_a_cancelled_task_is_terminal_and_a_second_cancel_refuses_to_pretend(
    hydrate_scenario, in_flight
):
    """Idempotent, and honest about it.

    The canonical conformance wording ("an operation on a terminal task returns
    an error") does not transfer: measured, a second `cancel` of the same pid
    answers `{"cancelled": False, "killed": []}` — no error key at all. That is
    the kernel's actual contract, stated in `_verb_cancel`'s own docstring: it
    does not pretend. Asserting an error here would pin a behaviour that does
    not exist and would go red the first time someone read the code.

    A pid that is terminal and a pid that never existed are the SAME reply,
    which is the honest answer to the only question the caller asked — is this
    task in the table? — and is why `cancel` needs no separate "already
    finished" state."""
    h = await hydrate_scenario("lifecycle")
    caller, pid = await _launch_slow(h, in_flight)
    await _kill(h, pid)
    await asyncio.wait_for(caller, timeout=2)

    second = await _cancel(h, pid)
    check(
        "C15",
        second == {"cancelled": False, "killed": []},
        f"a terminal task must absorb a second cancel: {second!r}",
    )
    check(
        "C15",
        "error" not in second,
        f"the no-op is not an error — cancel fails honestly, it does not raise a"
        f" class of failure that does not exist: {second!r}",
    )
    never = await _cancel(h, "slowbot:ffffffff")
    check(
        "C15",
        never == second,
        f"a pid that never existed and a pid that is done are one answer:"
        f" {never!r} vs {second!r}",
    )


async def test_a_deleted_agent_absorbs_the_calls_a_standing_grant_still_names(
    hydrate_scenario,
):
    """The agent-terminal half, and it is a different animal from the task one.

    `doomed` is reachable from warden while it exists. It is then deleted — an
    OPERATOR act (`_current_sender` is None), which is the only authority that
    composes the graph after boot. Warden's `can_call` still names it: nothing
    revoked the grant. So the reply to the next call proves which layer answered
    — target resolution fails BEFORE the gate (`Agent.send`), so the caller gets
    a non-existence report, NOT `{"reason": "unauthorized"}`. A caller that saw a
    denial there would retry with credentials for an agent that is gone.

    The second delete absorbs the same way `cancel` does: the same body plus
    `deleted: False` — it reports what did not happen instead of claiming a
    successful teardown of nothing."""
    h = await hydrate_scenario("lifecycle")
    before = await reach(h.kernel, "warden", "doomed", {"type": "list", "path": ""})
    check(
        "C15",
        not denied(before) and "error" not in before,
        f"doomed must be reachable BEFORE it is deleted, or the after-state"
        f" proves nothing: {before!r}",
    )

    gone = (
        await h.kernel.send(
            [
                {
                    "target": h.kernel.id,
                    "payload": {"type": "delete_agent", "id": "doomed"},
                }
            ]
        )
    )[0]["result"]
    check("C15", gone == {"deleted": True, "id": "doomed"}, f"{gone!r}")
    check(
        "C15",
        "doomed" not in (await dehydrate(h.kernel))["ids"],
        "the deleted agent is still in the tree the kernel reports",
    )

    after = await reach(h.kernel, "warden", "doomed", {"type": "list", "path": ""})
    check(
        "C15",
        after.get("error") == "no agent 'doomed'",
        f"a deleted id must absorb the call: {after!r}",
    )
    check(
        "C15",
        not denied(after),
        f"the grant outlived the agent — the reply must be about non-existence,"
        f" never about authority: {after!r}",
    )

    again = (
        await h.kernel.send(
            [
                {
                    "target": h.kernel.id,
                    "payload": {"type": "delete_agent", "id": "doomed"},
                }
            ]
        )
    )[0]["result"]
    check(
        "C15",
        again.get("deleted") is False and again.get("error") == "no agent 'doomed'",
        f"a second delete must report what it did NOT do: {again!r}",
    )


# ─── C16 · four failure classes, four shapes ────────────────────


def _failure_class(reply: dict, verb: str | None) -> str:
    """Label a failure by SHAPE ALONE — no knowledge of which agent answered.

    That constraint is the point. A caller (an LLM, a relay, a UI) sees one
    dict and has to decide what to do next: fix the id, fix the verb, fix the
    payload, or ask for authority. If two of those classes were one shape, the
    caller would have to guess."""
    if reply.get("reason") == "unauthorized":
        return "denial"
    if "agent_count" in reply:
        return "unknown_id"
    err = str(reply.get("error", ""))
    if not err:
        return "not-a-failure"
    if "unknown type" in err:
        return "unknown_verb"
    if verb is not None and err.startswith(f"{verb}: "):
        return "malformed"
    return "unclassified"


async def test_the_four_failure_classes_stay_four_distinguishable_shapes(
    hydrate_scenario,
):
    """One sender, four mistakes, four shapes — each carrying the fact that
    repairs it.

    - unknown id → the live id index (`agent_count` + `agents`), so the caller
      can correct itself. Measured 2026-08-04, quoted in `Kernel.unknown_agent`:
      a model copied a literal `<id>` placeholder and got the same bare error 34
      times without ever repairing it.
    - unknown verb → BUNDLE-scoped (`file_bridge: unknown type 'frobnicate'`),
      which names who did not understand as well as what.
    - malformed → VERB-scoped (`cancel: task_id (str) required`) and names the
      missing field. Only SYSTEM verbs produce this class; see the next test.
    - denial → `reason`, plus the `side` and `hint` that repair it. Those two
      are declared as an edge in the config, so PC-2 already pins them for this
      exact probe; what is left for the taxonomy is that `reason` makes the
      class separable from the other three, which the comparison below asserts.

    The unknown-verb probe rides a grant of `"*"` so the gate is NOT what
    answers: an ungranted fake verb would come back as a denial and the class
    would be untestable."""
    h = await hydrate_scenario("lifecycle")

    unknown_id = await reach(h.kernel, "warden", "nope", {"type": "list"})
    unknown_verb = await reach(h.kernel, "warden", "open_fs", {"type": "frobnicate"})
    malformed = await _cancel(h, None)
    denial = await reach(h.kernel, "warden", "stranger", {"type": "list", "path": ""})

    check(
        "C16",
        unknown_id.get("error") == "no agent 'nope'"
        and unknown_id.get("agent_count") == len(unknown_id.get("agents", [])),
        f"an unknown id must carry the live index that repairs it: {unknown_id!r}",
    )
    check(
        "C16",
        unknown_verb.get("error") == "file_bridge: unknown type 'frobnicate'",
        f"an unknown verb must name the BUNDLE that did not understand it:"
        f" {unknown_verb!r}",
    )
    check(
        "C16",
        malformed.get("reason") == "missing_arg"
        and malformed.get("arg") == "task_id"
        and "cancel" in malformed.get("error", ""),
        # The verb and the missing field are now STRUCTURAL (`reason` + `arg`),
        # not something a caller has to parse out of English. Pinning the exact
        # prose was pinning the weakest part of the reply.
        f"a malformed system verb must name the verb AND the missing field:"
        f" {malformed!r}",
    )
    got = {
        "unknown_id": _failure_class(unknown_id, "list"),
        "unknown_verb": _failure_class(unknown_verb, "frobnicate"),
        "malformed": _failure_class(malformed, "cancel"),
        "denial": _failure_class(denial, "list"),
    }
    check(
        "C16",
        got == {k: k for k in got},
        f"the four classes must be separable by shape alone: {got!r}",
    )


async def test_a_payload_with_no_type_at_all_collapses_into_the_unknown_verb_class(
    hydrate_scenario,
):
    """Why the taxonomy has FOUR classes and not three, stated as the measurement
    that forces it.

    A bundle dispatches on `payload["type"]` and a missing key arrives as `None`,
    so the most malformed payload there is — no verb at all — comes back
    `{"error": "file_bridge: unknown type None"}`: the unknown-VERB shape. A
    genuinely distinct "malformed" class therefore exists only where the
    substrate validates arguments itself, i.e. for SYSTEM verbs, which is why
    the class's probe above is `cancel` and not a bundle call.

    Pinned rather than xfailed: it is not a defect, it is where the boundary
    actually falls, and a taxonomy that claimed a bundle can tell "no verb" from
    "wrong verb" would be teaching callers something untrue."""
    h = await hydrate_scenario("lifecycle")
    typeless = await reach(h.kernel, "warden", "open_fs", {})
    check(
        "C16",
        typeless.get("error") == "file_bridge: unknown type None",
        f"a typeless payload is answered by the bundle's verb table: {typeless!r}",
    )
    check(
        "C16",
        _failure_class(typeless, None) == "unknown_verb",
        f"it lands in the unknown-verb class, not a fifth one: {typeless!r}",
    )


async def test_a_cancel_naming_no_live_task_is_a_no_op_not_a_member_of_the_taxonomy(
    hydrate_scenario,
):
    """The line between "you asked wrong" and "there was nothing to do".

    `cancel` with a missing `task_id` is MALFORMED — the caller cannot be
    answered. `cancel` with a well-formed `task_id` that names no live task is a
    NO-OP — the caller was answered, and the answer is no. They share a verb and
    nothing else: one carries `error`, the other carries `cancelled: False`.

    Collapsing them would cost real information. A supervisor polling `cancel`
    on a pid it no longer sees would read a typo in its own payload as "the task
    is already gone", or the reverse."""
    h = await hydrate_scenario("lifecycle")
    malformed = await _cancel(h, None)
    no_op = await _cancel(h, "slowbot:ffffffff")

    check(
        "C16",
        _failure_class(malformed, "cancel") == "malformed",
        f"a cancel with no task_id is a payload error: {malformed!r}",
    )
    check(
        "C16",
        no_op == {"cancelled": False, "killed": []},
        f"a cancel naming nothing live is an answer, not a failure: {no_op!r}",
    )
    check(
        "C16",
        _failure_class(no_op, "cancel") == "not-a-failure",
        f"the no-op must stay OUT of the failure taxonomy: {no_op!r}",
    )


async def test_an_unknown_id_names_the_whole_roster_to_a_sender_that_may_call_none_of_it(
    hydrate_scenario,
):
    """Pinning a deliberate disclosure, with the reason it is safe MEASURED
    rather than argued.

    Target resolution runs BEFORE the call gate (`Agent.send`), so the id index
    in an unknown-id reply reaches ANY sender — `stranger` here holds nothing
    but the structural baseline and still reads the full roster off a send to a
    bogus id. That is in tension with "gating `reflect` closes lateral
    discovery": the ids leak through a channel `reflect` does not control.

    It is pinned as-is, not xfailed, because the leak is of NAMES and names are
    not reach: the same sender, immediately, is refused on every id it just
    learned — `list` and `reflect` alike, at egress, before the target is
    touched. The disclosure is the teaching-error design (`Kernel.unknown_agent`)
    and it hands out nothing the gate then honours. If that ever stops being
    true — if a disclosed id becomes callable — this test goes red on the second
    half, which is the half that matters."""
    h = await hydrate_scenario("lifecycle")
    leak = await reach(h.kernel, "stranger", "nope", {"type": "list"})
    roster = leak.get("agents") or []
    check(
        "C16",
        "open_fs" in roster and "slowbot" in roster,
        f"a deny-all sender reads the roster off an unknown id: {leak!r}",
    )

    for verb in ("list", "reflect"):
        r = await reach(h.kernel, "stranger", "open_fs", {"type": verb, "path": ""})
        check(
            "C16",
            denied(r) and denial_side(r) == "egress",
            f"a disclosed id must stay uncallable — {verb!r} got {r!r}",
        )
