"""author — the AI agent's SELF-REFLECT / SELF-CORRECT loop, as a verb.

Merged INTO `ai_core` (2026-08-07, user decision) from the rust `fantastic-author`
bundle: the verify-and-repair driver is not a separate mind that borrows a brain —
it is the AI agent driving ITSELF. `{type:'author', goal}` makes the agent author a
durable tool (a script agent on the advertised executor), OBSERVE it answering a
probe the model never sees, and refuse its own `final` until reality agrees.

The algorithm (proven live on gemma4:12b before the port — every clause is paying
for itself on a weak model):

1. TURN LOOP — each step is one stateless pass of THIS agent's own model under
   [`LEAN_SYSTEM`]: the model may reply ONLY `{"act":{target,payload}}` or
   `{"final":…}`.
2. NO REPAIR of the model's JSON. [`first_json`] takes the first complete object;
   an unparseable reply earns a nudge, never a brace patch — a driver that patches
   the envelope is measuring its own patch.
3. RELAY — an `act` is dispatched into the real kernel; the RESULT becomes the
   next turn.
4. VERIFY-REPAIR — `final` is REFUSED until the authored tool has been observed
   answering. On a false completion the loop reflects the tool's real state and
   pushes back.
5. AUTONOMOUS VERIFY — once a tool exists the loop probes it ITSELF every turn
   with a turn the model never sees, and reports ground truth: deaf vs CRASHED
   (the interpreter's own traceback, verbatim — from the ear's `job_done` or,
   when that fired before the ear existed, from the executor's job table), vs a
   REJECTED dependency call (the verb it used vs the verbs the dependency really
   has), vs accepted-but-mute.
6. `must_call` — "it answered" is weak (an echo answers perfectly); the probe
   must also drive that dependency, observed by a second ear.
7. `expect` — the canary that cannot be gamed: the probe's correct handling
   forces the model's completion through the tool, and the tool's `say` must
   carry the tracer (probe words subtracted first, so quoting the question back
   smuggles nothing).
8. The probe and the tracer are NEVER readable by the model — every turn is
   scrubbed on its way in.

Progress rides events on the agent's OWN id (`status`/`say`/`done`), so a UI just
watches the agent that is authoring.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import time

from kernel._authz import PRIVILEGED_VERBS

# Structural mutations the author routes through its OWN id, so authored tools are
# its children (baseline authority, no grants). One source of truth with the gate.
_AUTHOR_MUTATIONS = PRIVILEGED_VERBS

LEAN_SYSTEM = """You are an agent inside a LIVE agent kernel. Everything here is an agent addressed by a string id. There is ONE primitive: send(target_id, payload) — a JSON payload whose "type" field names the verb; the target replies with JSON.

You act by replying with EXACTLY ONE JSON object and NOTHING else, in one of these two forms:
  {"act": {"target": "<id>", "payload": { ...a verb payload... }}}
  {"final": "<short message>"}
After an {"act"} I send it for you and reply with the result as your next message.

YOUR REPLY MUST BE ONE COMPLETE, WELL-FORMED JSON OBJECT. It is parsed by a machine, not read by a human: nothing is repaired for you. Every "{" you open you MUST close with "}", and every "[" with "]" — an object that ends one brace short is DISCARDED and your act never happens. No prose, no markdown, no code fences, no trailing commas. When the payload carries source code, the code is ONE JSON string: escape every " as \\" and every newline as \\n, and close the string before you close the enclosing objects. Before you finish, re-read your reply from "{" to the final "}" and confirm it is balanced.

To LEARN anything, reflect it: {"act":{"target":"<id>","payload":{"type":"reflect"}}}. Reflecting "kernel" returns the system overview, your OWN capabilities, the agent tree, and a MAP of keyed docs (`readmes`) you can fetch one at a time. DISCOVER what you need by reflecting BEFORE you act — do not assume verb or API shapes.

You are ALONE and AUTONOMOUS — no human will reply between your turns. Never stop to ask, and never announce that you are "ready to proceed" or "understand now": that is NOT done. Reflecting and planning are not completing. Every reply must be a concrete {"act"} that moves the task forward. Only reply {"final":...} once the thing you were asked to build actually EXISTS and you have exercised it and seen it work. If you catch yourself about to declare readiness, instead emit the next {"act"} (a create_agent, or a send). Do NOT reflect the same target or doc twice — reading is not progress. The MOMENT a doc shows you how to build the tool (e.g. the self-extension doc's copy-pasteable example), your NEXT act MUST be a create_agent, not another reflect."""

# Turn budget — the live seed converges well inside it; a caller-supplied `steps`
# is clamped so the loop stays BOUNDED whoever asks (a runaway burns a real model).
STEPS = 26
MAX_STEPS = 64
DEFAULT_PROBE = "hello world"

# Waits are env-tunable because the unit tests drive REAL subprocesses through a
# scripted model and must not sit out live-scale grace periods.
PROBE_WAIT = float(os.environ.get("FANTASTIC_AUTHOR_PROBE_WAIT", "1.5"))
PROBE_POLL = 0.05
PROBE_THINK_WAIT = float(os.environ.get("FANTASTIC_AUTHOR_THINK_WAIT", "120"))
# A subprocess tool needs a beat to boot + register on_message before the first
# probe — the in-proc engines had no such window, and probing into it reads a
# good tool as deaf (measured on the rust port).
BOOT_BEAT = float(os.environ.get("FANTASTIC_AUTHOR_BOOT_BEAT", "0.4"))

NEXT_ACT = "Emit your next act now."


def first_json(s: str):
    """The FIRST complete JSON object in `s`, or None. No repair — a driver that
    patches the envelope is measuring its own patch. The one tolerance is `\\'`
    (a python-ism some models emit inside JSON strings), same as rust."""
    i = s.find("{")
    if i < 0:
        return None
    tail = s[i:]
    dec = json.JSONDecoder()
    for candidate in (tail, tail.replace("\\'", "'")):
        try:
            obj, _ = dec.raw_decode(candidate)
        except (json.JSONDecodeError, ValueError):
            continue
        if isinstance(obj, dict):
            return obj
    return None


def _compose(base: str, note: str | None) -> str:
    if note:
        return f"{base}\n\n{note} {NEXT_ACT}"
    return f"{base} {NEXT_ACT}"


def _redact(text: str, needle: str, mask: str) -> str:
    """Case-insensitive removal of `needle` — the scrub half of "the tracer is
    never readable". A tracer a model can read is a tracer it can hardcode."""
    if not needle:
        return text
    return re.sub(re.escape(needle), mask, text, flags=re.IGNORECASE)


def _task_prompt(goal: str, want_id: str | None) -> str:
    name_clause = f" Name it '{want_id}'." if want_id else ""
    return (
        f"TASK: Author a DURABLE tool — a new agent whose behaviour you write — "
        f"that satisfies this goal: {goal}.{name_clause} It must ANSWER a turn: "
        'when it is sent {"type":"send","text":<text>,"client_id":<id>} it emits '
        "a 'say' (tagged with that client_id) and then a 'done'. Discover HOW to "
        "author it (which executor, its script API, its receive/emit pattern) by "
        "REFLECTING first. Create it, then TEST it yourself by sending it "
        '{"type":"send","text":<any turn you like>,"client_id":"me"}. I will '
        "ALSO probe it myself, every step, with a turn of MY choosing that you "
        "will never see, and I will tell you what it did — so it must work for "
        "ANY turn, not for one you prepared for. Only reply final once it really "
        "answers."
    )


def _deaf_pushback(tid: str, state: str) -> str:
    return (
        f"AUTONOMOUS VERIFY: I sent your agent '{tid}' a probe turn of my own "
        'choosing ({"type":"send","text":<my text>,"client_id":"me"}) and it '
        "started but emitted NO say/done — it is not answering its inbox "
        f"(nothing crashed; it is deaf, not broken). Its state: {state}. Its "
        "code MUST register kernel.on_message(callback) and keep running, and "
        "on a turn where type=='send' read `text` and `client_id` and then emit "
        "{type:'say', text:<its answer>, client_id:<client_id>} then "
        "{type:'done', client_id:<client_id>}. Fix it: update_agent its `code` "
        "then stop+boot, or delete_agent then create_agent again with corrected "
        "code."
    )


def _crash_pushback(tid: str, err: str) -> str:
    return (
        f"AUTONOMOUS VERIFY: your agent '{tid}' CRASHED handling my probe turn. "
        f"The interpreter's own words:\n{err}\nThat is the REAL exception from "
        "your code — fix exactly that (update_agent its `code` then stop+boot, "
        "or delete_agent then create_agent with corrected code)."
    )


def _rejected_note(dep: str, used: str, verbs: str) -> str:
    return (
        f"THE CAUSE: I watched your agent send {used} to '{dep}'. '{dep}' does "
        f"NOT accept that — the verbs it actually has are: {verbs}. Your call "
        "was REJECTED, and your agent received an error object instead of an "
        "answer. REFLECT an agent BEFORE you call it "
        f'({{"act":{{"target":"{dep}","payload":{{"type":"reflect"}}}}}}) and '
        f"read its `verbs` — then fix your agent's code to call '{dep}' with a "
        "verb it really has (update_agent its `code` then stop+boot, or "
        "delete_agent then create_agent)."
    )


def _accepted_note(dep: str, used: str) -> str:
    return (
        f"WHAT I OBSERVED: your agent sent {used} to '{dep}', and '{dep}' DOES "
        "accept that verb — the call landed. So the call is not the defect: the "
        f"answer '{dep}' returned never came out of your agent as its `say`. "
        "Read what that call RETURNS and emit the completion text it carries."
    )


def _echo_pushback(dep: str) -> str:
    return (
        f"Your agent answered, but it NEVER called '{dep}' — it is ECHOING, not "
        f"THINKING. A mind must send the user's turn to '{dep}' and use the "
        "completion it returns. Read the kernel's `minds` doc: "
        '{"act":{"target":"kernel","payload":{"type":"reflect","readme":"minds"}}}. '
        "Then FIX its code (update_agent then stop+boot, or delete_agent + "
        "create_agent) and try again."
    )


def _canary_pushback(said: str, dep: str) -> str:
    return (
        f"Your agent answered '{said}' — but that did NOT come from the brain. "
        f"It must hand the user's turn to '{dep}' and USE the completion the "
        "brain returns. If the brain rejected your call, you used the wrong "
        "verb or shape: REFLECT it "
        f'({{"act":{{"target":"{dep}","payload":{{"type":"reflect"}}}}}}) to '
        "see the verbs it actually accepts, and read the kernel's `minds` doc "
        '({"act":{"target":"kernel","payload":{"type":"reflect","readme":"minds"}}}). '
        "Then FIX the agent's code (update_agent then stop+boot, or "
        "delete_agent + create_agent) and try again."
    )


class _Run:
    """One author run's observation state: the ears, the probe window, the crash."""

    def __init__(self, self_id: str, kernel, probe: str, must_call: str | None):
        self.self_id = self_id
        self.kernel = kernel
        self.probe = probe
        self.must_call = must_call
        self.ear = f"_author_ear_{self_id}"
        self.dep_ear = f"_author_dep_{self_id}"
        kernel.ctx.ensure_inbox(self.ear)
        if must_call:
            kernel.ctx.ensure_inbox(self.dep_ear)
            kernel.watch(must_call, self.dep_ear)
        # window state
        self.probing = False
        self.said: str = ""
        self.saw_done = False
        self.crash: str | None = None
        self.dep_verbs_used: list[str] = []

    def _drain_inbox(self, inbox_id: str) -> list[dict]:
        q = self.kernel.ctx.ensure_inbox(inbox_id)
        out = []
        while not q.empty():
            out.append(q.get_nowait())
        return out

    def observe(self) -> bool:
        """Pull the ears. True when the tool answered (a `say` inside the window)."""
        ok = False
        for ev in self._drain_inbox(self.ear):
            t = ev.get("type")
            if t == "say" and self.probing:
                self.said = str(ev.get("text", ""))
                ok = True
            elif t == "done" and self.probing:
                self.saw_done = True
            elif t == "job_done" and ev.get("ok") is False:
                # The interpreter's own words: exec_bridge carries them in stderr.
                self.crash = str(
                    ev.get("error") or ev.get("stderr") or "(no error text)"
                )
                self.saw_done = True
        if self.must_call:
            for ev in self._drain_inbox(self.dep_ear):
                v = ev.get("type")
                if isinstance(v, str) and v:
                    self.dep_verbs_used.append(v)
        return ok or bool(self.said)

    async def probe_tool(self, tool: str) -> bool:
        """Send the probe and WAIT (polling) for the tool to answer — the loop's
        ground truth. Consults the executor's JOB TABLE before calling a dead
        program merely deaf: a create-time crash fires before the ear exists."""
        self.probing = True
        self.said = ""
        self.saw_done = False
        self.dep_verbs_used = []
        # Revive a stopped durable tool for observation.
        rec = self.kernel.get(tool) or {}
        refl = (
            await self.kernel.send([{"target": tool, "payload": {"type": "reflect"}}])
        )[0]["result"]
        if isinstance(refl, dict) and refl.get("running") == 0 and rec.get("code"):
            (await self.kernel.send([{"target": tool, "payload": {"type": "start"}}]))[
                0
            ]["result"]
            await asyncio.sleep(BOOT_BEAT)
        (
            await self.kernel.send(
                [
                    {
                        "target": tool,
                        "payload": {
                            "type": "send",
                            "text": self.probe,
                            "client_id": "author",
                        },
                    }
                ]
            )
        )[0]["result"]
        deadline = time.monotonic() + PROBE_WAIT
        extended = False
        while True:
            if self.observe():
                break
            if self.saw_done:
                break
            if self.must_call and self.dep_verbs_used and not extended:
                extended = True
                deadline = time.monotonic() + PROBE_THINK_WAIT
            if time.monotonic() >= deadline:
                break
            await asyncio.sleep(PROBE_POLL)
        ok = self.observe() or bool(self.said)
        self.probing = False
        if not ok and self.crash is None:
            st = (
                await self.kernel.send(
                    [{"target": tool, "payload": {"type": "status"}}]
                )
            )[0]["result"]
            jobs = st.get("jobs") if isinstance(st, dict) else None
            failed = [j for j in jobs or [] if j.get("status") == "failed"]
            if failed:
                jid = failed[-1].get("job_id", "")
                full = (
                    await self.kernel.send(
                        [{"target": tool, "payload": {"type": "status", "job_id": jid}}]
                    )
                )[0]["result"]
                j = (full.get("jobs") or [{}])[0] if isinstance(full, dict) else {}
                self.crash = (
                    str(j.get("stderr") or failed[-1].get("last_line") or "") or None
                )
        return ok

    async def dep_verdict(self) -> str | None:
        """After a failed probe with `must_call`: was the dependency called, and
        with a verb it really has? Returns the note for the pushback, or None."""
        if not self.must_call:
            return None
        used = [v for v in self.dep_verbs_used if v not in ("say", "done")]
        if not used:
            return None
        refl = (
            await self.kernel.send(
                [{"target": self.must_call, "payload": {"type": "reflect"}}]
            )
        )[0]["result"]
        have = list((refl.get("verbs") or {}).keys()) if isinstance(refl, dict) else []
        shown = " and ".join(f"{{type:'{v}'}}" for v in have)
        used_s = " and ".join(f"{{type:'{v}'}}" for v in sorted(set(used)))
        if any(v in have for v in used):
            return _accepted_note(self.must_call, used_s)
        return _rejected_note(self.must_call, used_s, shown)

    def canary_ok(self, expect: str) -> bool:
        """The tracer must be in the `say` AFTER the probe's own words are
        subtracted — quoting the question back smuggles nothing in."""
        if not expect:
            return True
        cleaned = _redact(self.said, self.probe, "")
        return expect.lower() in cleaned.lower()

    def scrub(self, turn: str, expect: str) -> str:
        """Nothing the model reads may carry the probe or the tracer."""
        out = _redact(turn, self.probe, "<a probe turn you never see>")
        return _redact(out, expect, "<redacted>")

    def teardown(self):
        if self.must_call:
            self.kernel.unwatch(self.must_call, self.dep_ear)


async def _emit_progress(kernel, self_id: str, phase: str, text: str):
    await kernel.emit(
        self_id,
        {"type": "status", "source": self_id, "phase": phase, "detail": {"text": text}},
    )


async def author(self_id: str, payload: dict, kernel) -> dict:
    """args: goal:str (req), steps:int? (≤64), id:str? (suggested tool id),
    probe:str? (the turn the tool must answer — the model never sees it),
    must_call:str? (the probe must also drive this agent), expect:str? (the
    tool's answer must carry this tracer, probe words subtracted). The agent's
    SELF-CORRECT loop: it authors a durable tool on the advertised executor and
    its own `final` is refused until the tool is OBSERVED answering. Returns
    {id, code, message} on success, {error, last_state} when the budget runs out."""
    goal = payload.get("goal")
    if not isinstance(goal, str) or not goal.strip():
        return {"error": "author: goal (str) required"}
    steps = min(int(payload.get("steps") or STEPS), MAX_STEPS)
    probe = str(payload.get("probe") or DEFAULT_PROBE)
    expect = str(payload.get("expect") or "")
    must_call = payload.get("must_call") or None
    want_id = payload.get("id") or None

    run = _Run(self_id, kernel, probe, must_call)
    history: list[dict] = []
    turn = _task_prompt(goal, want_id)
    tool: str | None = None
    verified = False
    final_msg: str | None = None
    last_state = "no tool was ever created"
    reflect_streak = 0
    executor = None  # learned from the created tool's record

    try:
        for _step in range(steps):
            await _emit_progress(kernel, self_id, "authoring", turn)
            reply = (
                await kernel.send(
                    [
                        {
                            "target": self_id,
                            "payload": {
                                "type": "send",
                                "text": run.scrub(turn, expect),
                                "system_prompt": LEAN_SYSTEM,
                                "messages": history
                                + [
                                    {"role": "user", "content": run.scrub(turn, expect)}
                                ],
                                "client_id": "author",
                            },
                        }
                    ]
                )
            )[0]["result"]
            response = (
                (reply or {}).get("response", "") if isinstance(reply, dict) else ""
            )
            history.append({"role": "user", "content": run.scrub(turn, expect)})
            history.append({"role": "assistant", "content": response})

            obj = first_json(response)
            if obj is None:
                turn = _compose(
                    "Your reply was not ONE complete JSON object, so NOTHING "
                    'happened. Reply with exactly {"act":{...}} or '
                    '{"final":"..."} and nothing else.',
                    None,
                )
                continue

            # ── final: refused until observed ──
            if "final" in obj:
                if tool and verified:
                    final_msg = str(obj.get("final", ""))
                    break
                note = None
                if tool:
                    ok = await run.probe_tool(tool)
                    if ok and run.canary_ok(expect):
                        dep_ok = not must_call or any(
                            v not in ("say", "done") for v in run.dep_verbs_used
                        )
                        if dep_ok:
                            verified = True
                            final_msg = str(obj.get("final", ""))
                            break
                        last_state = "it answers, but never called its dependency"
                        turn = _compose(_echo_pushback(str(must_call)), None)
                        continue
                    if ok and not run.canary_ok(expect):
                        last_state = (
                            f"it answers '{run.said}', but that answer does not "
                            "come from the brain"
                        )
                        turn = _compose(
                            _canary_pushback(run.said, str(must_call or "-")), None
                        )
                        continue
                    if run.crash:
                        last_state = f"it CRASHED: {run.crash.splitlines()[-1]}"
                        turn = _compose(_crash_pushback(tool, run.crash), None)
                        run.crash = None
                        continue
                    note = await run.dep_verdict()
                    refl = (
                        await kernel.send(
                            [{"target": tool, "payload": {"type": "reflect"}}]
                        )
                    )[0]["result"]
                    state = json.dumps(refl, default=str)
                    last_state = "it started but emitted no say/done (deaf)"
                    turn = (
                        "You replied final, but I have NOT observed your tool "
                        "answer the probe turn I sent it — you have NOT "
                        "succeeded. Do not give up. "
                        + _compose(_deaf_pushback(tool, state), note)
                    )
                    continue
                last_state = "no tool was ever created"
                turn = _compose(
                    "You replied final, but you have not created any tool — "
                    "create_agent one NOW, on the exec_bridge executor, whose "
                    "`code` satisfies the goal.",
                    None,
                )
                continue

            # ── act: relay into the real kernel ──
            act = obj.get("act") or {}
            target = act.get("target")
            act_payload = act.get("payload")
            if not isinstance(target, str) or not isinstance(act_payload, dict):
                turn = _compose(
                    'An {"act"} needs {"target":"<id>","payload":{...}}. '
                    "Yours was missing one of them.",
                    None,
                )
                continue
            verb = act_payload.get("type", "")
            if verb == "reflect":
                reflect_streak += 1
            else:
                reflect_streak = 0
            # The tools an author builds are ITS OWN subtree. Route every
            # structural mutation through self_id, so `create_agent` lands the
            # tool as this agent's child and later `delete_agent`/`update_agent`
            # edit it — all baseline authority (fork + $children), zero grants,
            # and the tools are owned + cleaned up with the author. Under deny-all
            # an agent could not reach a tool parented elsewhere anyway.
            if verb in _AUTHOR_MUTATIONS:
                # Kernel-owned verbs go to `kernel`; the author's OWN id rides as
                # `parent_id` so a tool it authors is still its child. Same
                # placement as when addressing self did the work implicitly.
                if verb == "create_agent":
                    act_payload = {**act_payload, "parent_id": self_id}
                target = "kernel"
            try:
                result = (
                    await kernel.send([{"target": target, "payload": act_payload}])
                )[0]["result"]
            except Exception as exc:  # noqa: BLE001 — an exception is a result too
                result = {"error": f"{type(exc).__name__}: {exc}"}

            # A fresh script agent on the executor is the candidate tool.
            hm = act_payload.get("handler_module", "")
            if (
                verb == "create_agent"
                and hm == "exec_bridge.tools"
                and isinstance(result, dict)
                and not result.get("error")
                and result.get("id")
            ):
                tool = str(result["id"])
                executor = hm
                kernel.watch(tool, run.ear)
                await asyncio.sleep(BOOT_BEAT)
                verified = False

            result_text = json.dumps(result, default=str)
            turn = f"Result of your send to '{target}':\n{result_text}"

            # Autonomous verify on every act once a tool exists.
            if tool:
                await _emit_progress(kernel, self_id, "verifying", tool)
                ok = await run.probe_tool(tool)
                if ok and run.canary_ok(expect):
                    dep_ok = not must_call or any(
                        v not in ("say", "done") for v in run.dep_verbs_used
                    )
                    verified = bool(dep_ok)
                    if verified:
                        turn += (
                            "\n\nAUTONOMOUS VERIFY: I probed your agent with a "
                            "turn of my own and it ANSWERED. If you are done, "
                            'reply {"final":"<what you built>"}.'
                        )
                        continue
                    turn = _compose(_echo_pushback(str(must_call)), None)
                    continue
                if run.crash:
                    last_state = f"it CRASHED: {run.crash.splitlines()[-1]}"
                    turn += "\n\n" + _crash_pushback(tool, run.crash)
                    run.crash = None
                    continue
                note = await run.dep_verdict()
                refl = (
                    await kernel.send(
                        [{"target": tool, "payload": {"type": "reflect"}}]
                    )
                )[0]["result"]
                state = json.dumps(refl, default=str)
                last_state = "it started but emitted no say/done (deaf)"
                turn += "\n\n" + _compose(_deaf_pushback(tool, state), note)

        if final_msg is not None and tool:
            rec = kernel.get(tool) or {}
            await _emit_progress(kernel, self_id, "done", final_msg)
            await kernel.emit(self_id, {"type": "done", "client_id": "author"})
            return {
                "id": tool,
                "code": rec.get("code", ""),
                "message": final_msg,
                "executor": executor,
            }
        await _emit_progress(kernel, self_id, "done", "gave up")
        return {
            "error": (
                f"author: gave up after {steps} steps — tool: "
                f"{tool or 'none'}; last state: {last_state}"
            )
        }
    finally:
        run.teardown()
