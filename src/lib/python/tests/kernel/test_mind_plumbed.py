"""PLUMBED MIND (python) — a python-authored agent on `exec_bridge` drives a
context-managed backend (deterministic `_FakeProvider`, no network) through a
tool-feedback cycle. The emergent-harness chain on the python substrate:

  user turn → mind (python) → send{text, system_prompt} → "CALL … list_agents"
           → mind dispatches → feeds RESULT back as the next turn → send
           → "FINAL: …" → mind emits say + done.

The backend HOLDS the conversation (keyed by client_id) + would auto-compact; the
mind just sends text turns and teaches its tool convention via `system_prompt`.
"""

from __future__ import annotations

import asyncio
import sys
import json
import secrets
import time

import ollama_backend.tools as ot


class _FakeProvider:
    """Yields a scripted token stream per `chat()` call (one per pass). Records
    the `messages` of every pass in `requests`, so a test can prove a human
    answer became the NEXT AI-agent turn (mirrors the rust `_RecordProvider`)."""

    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0
        self.requests: list = []

    async def chat(self, messages):
        self.requests.append(messages)
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


def _watch(kernel, agent_id: str):
    """Subscribe a synthetic watcher to an agent's inbox; returns its queue."""
    name = "w_" + secrets.token_hex(3)
    kernel.ctx.ensure_inbox(name)
    kernel.watch(agent_id, name)
    return kernel.ctx.inboxes[name]


# The authored mind. Callback → queue → MAIN-loop: the on_message callback runs on
# the connector's reader thread, so it must NOT block; it only enqueues turns. The
# main thread does the blocking `kernel.send`s (safe). It processes ONLY `send`
# turns — ignoring its own say/done emits (which also land on its owner inbox).
MIND_CODE = r"""
import queue, json, re
inbox = queue.Queue()
def _on(m):
    if isinstance(m, dict) and m.get("type") == "send":
        inbox.put(m)
kernel.on_message(_on)
kernel.reflect("kernel")  # FIFO round-trip: ensure on_message is registered upstream
print("READY", flush=True)
SYSTEM = ("You drive a kernel. To act, reply EXACTLY `CALL <target> <verb>`. "
          "When you know the answer, reply EXACTLY `FINAL: <answer>`.")
while True:
    msg = inbox.get()
    print("TURN " + json.dumps(msg), flush=True)
    text = msg.get("text", "")
    cid = msg.get("client_id")
    for _ in range(4):
        reply = kernel.send("the_ai", {"type": "send", "text": text,
                                       "system_prompt": SYSTEM, "client_id": cid})
        print("REPLY " + json.dumps(reply), flush=True)
        resp = (reply or {}).get("response") or (reply or {}).get("error") or ""
        m2 = re.match(r"^CALL (\S+) (\S+)", resp)
        if m2:
            r = kernel.send(m2.group(1), {"type": m2.group(2)})
            text = "RESULT: " + json.dumps(r)
        else:
            ans = resp[7:] if resp.startswith("FINAL: ") else resp
            kernel.emit({"type": "say", "text": ans, "client_id": cid})
            kernel.emit({"type": "done", "client_id": cid})
            break
"""


async def _wait_ready(kernel, pid, job_id, timeout=8.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        r = await kernel.send(pid, {"type": "status", "job_id": job_id})
        j = r["jobs"][0]
        if "READY" in j.get("stdout", "") or j.get("last_line") == "READY":
            return
        if j["status"] in ("done", "failed", "stopped"):
            raise AssertionError(f"mind exited before READY: {j!r}")
        await asyncio.sleep(0.05)
    raise AssertionError("mind never printed READY")


async def test_python_mind_drives_backend_through_feedback_cycle(
    seeded_kernel, store_agent
):
    k = seeded_kernel
    # A bare, context-managed backend with a deterministic provider (no network).
    ai = (
        await k.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "id": "the_ai",
                "file_bridge_id": store_agent,
                # Two-sided deny-all: accept the mind that drives it (ingress).
                "callable_by": {"the_mind": "*"},
            },
        )
    )["id"]
    fp = _FakeProvider(
        [["CALL kernel_state list_agents"], ["FINAL: the kernel is populated"]]
    )
    ot._providers[ai] = fp
    try:
        mind = (
            await k.send(
                "kernel_state",
                {
                    "type": "create_agent",
                    "handler_module": "exec_bridge.tools",
                    "id": "the_mind",
                    "interpreters": [sys.executable],
                    "shim": "python",
                    "ingress_rule": "allow_all_ingress",
                    # Deny-all: the mind drives the AI — an explicit lateral grant.
                    "can_call": {"the_ai": "*"},
                },
            )
        )["id"]
        r = await k.send(mind, {"type": "start", "code": MIND_CODE})
        jid = r["job_id"]
        await _wait_ready(k, mind, jid)

        q = _watch(k, mind)
        # Address the mind like any agent: an unknown verb is queued to its running
        # job's inbox with a `{queued:true}` ack (the addressable-inbox parity path),
        # delivered to the mind via `on_message`.
        ack = await k.send(
            mind, {"type": "send", "text": "what lives here?", "client_id": "probe"}
        )
        assert ack.get("queued") is True, ack

        say = None
        done = False
        end = time.monotonic() + 8
        while time.monotonic() < end and not done:
            try:
                ev = await asyncio.wait_for(q.get(), timeout=0.2)
            except asyncio.TimeoutError:
                continue
            t = ev.get("type")
            if t == "say":
                say = ev
            elif t == "done":
                done = True

        if not done:
            st = await k.send(mind, {"type": "status", "job_id": jid, "full": True})
            raise AssertionError(
                "the mind must finish the turn with a done event; job="
                + repr(st.get("jobs", [{}])[0])
            )
        assert say is not None and say.get("client_id") == "probe"
        assert "populated" in say.get("text", ""), f"final answer surfaced: {say!r}"
        # The feedback cycle really ran: TWO backend passes (act → result → answer).
        assert fp.calls == 2, f"expected 2 provider passes, got {fp.calls}"
    finally:
        ot._providers.pop(ai, None)


# ─── plan / execute + ask-the-human (the human-in-the-loop protocol) ──────────
#
# A richer mind: it lays out a PLAN, executes steps (a `CALL` tool dispatch OR an
# `ASK` to the human), and on an ASK it emits a {type:"question"} event, SUSPENDS
# (emits NO `done`), and resumes when the matching {type:"answer"} arrives as the
# next turn. This is the emergent side of the human-loop protocol — nothing in the
# backend or kernel changes; the mind owns the ask. The question/answer SHAPES
# (kind: single|multi|text|confirm · action: accept|decline|cancel) are the wire
# the live TUI question-card renders. See src/cli/design/27_ask_plan_execute.md.
PLAN_MIND_CODE = r"""
# plan/execute + ask harness (pull loop): PLAN lays out steps; a step CALLs a tool
# or ASKs the human; an ASK emits {type:question}, suspends on the inbox, resumes on
# the matching {type:answer} — NEVER fabricating a value on decline/cancel.
import queue, json, re
inbox = queue.Queue()
pending = []
def _on(m):
    if isinstance(m, dict) and m.get("type") in ("send", "answer"):
        inbox.put(m)
kernel.on_message(_on)
kernel.reflect("kernel")  # FIFO round-trip: ensure on_message is registered upstream
print("READY", flush=True)
SYSTEM = ("You drive a kernel with a plan/execute loop. Reply EXACTLY one line: "
          "`PLAN: a | b` to lay out steps; `CALL <target> <verb>` to run a tool; "
          "`ASK <json>` to ask the human "
          "(json={\"kind\":\"single|multi|text|confirm\",\"question\":\"...\",\"options\":[...]}); "
          "`FINAL: <answer>` when done.")
_n = [0]
def await_answer(rid):
    while True:
        m = inbox.get()
        if m.get("type") == "answer" and m.get("request_id") == rid:
            return m
        pending.append(m)  # a user turn arrived mid-question — handle it after
def run_turn(text, cid):
    for _ in range(16):
        reply = kernel.send("the_ai", {"type": "send", "text": text,
                                       "system_prompt": SYSTEM, "client_id": cid})
        resp = ((reply or {}).get("response") or (reply or {}).get("error") or "").strip()
        body = re.sub(r"^STEP \d+:\s*", "", resp)
        if body.startswith("PLAN:"):
            text = "Execute the plan step by step."
            continue
        mc = re.match(r"^CALL (\S+) (\S+)", body)
        if mc:
            r = kernel.send(mc.group(1), {"type": mc.group(2)})
            text = "RESULT: " + json.dumps(r)
            continue
        if body.startswith("ASK "):
            spec = json.loads(body[4:])
            _n[0] += 1
            rid = "q_" + str(_n[0])
            q = {"type": "question", "request_id": rid,
                 "kind": spec.get("kind", "text"), "question": spec.get("question", ""),
                 "source": kernel.self_id, "client_id": cid}
            if spec.get("options"): q["options"] = spec["options"]
            if spec.get("header"): q["header"] = spec["header"]
            if "default" in spec: q["default"] = spec["default"]
            kernel.emit(q)
            ans = await_answer(rid)
            if ans.get("action") == "accept":
                text = "ANSWER " + rid + ": " + json.dumps(ans.get("value"))
                continue
            # decline/cancel: NEVER fabricate a value — branch to an abort.
            kernel.emit({"type": "say",
                         "text": "(question " + str(ans.get("action")) + " — aborting)",
                         "client_id": cid})
            kernel.emit({"type": "done", "client_id": cid})
            return
        ans_text = body[6:].strip() if body.startswith("FINAL:") else body
        kernel.emit({"type": "say", "text": ans_text, "client_id": cid})
        kernel.emit({"type": "done", "client_id": cid})
        return
while True:
    msg = pending.pop(0) if pending else inbox.get()
    if msg.get("type") == "send":
        run_turn(msg.get("text", ""), msg.get("client_id"))
"""


async def _collect(q, until_type: str, timeout: float = 8.0):
    """Drain a watcher queue until an event of `until_type`; return (event, all_seen)."""
    seen: list = []
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            ev = await asyncio.wait_for(
                q.get(), timeout=max(0.05, end - time.monotonic())
            )
        except asyncio.TimeoutError:
            break
        seen.append(ev)
        if ev.get("type") == until_type:
            return ev, seen
    return None, seen


async def _spawn_plan_mind(k, store_agent, scripts):
    """Create the `the_ai` AI agent (deterministic `_FakeProvider`) + the plan/execute
    mind; return (mind_id, fake_provider). Caller pops `ot._providers` in a finally."""
    ai = (
        await k.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "id": "the_ai",
                "file_bridge_id": store_agent,
                # Two-sided deny-all: accept the mind that drives it (ingress).
                "callable_by": {"the_mind": "*"},
            },
        )
    )["id"]
    fp = _FakeProvider(scripts)
    ot._providers[ai] = fp
    mind = (
        await k.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "exec_bridge.tools",
                "id": "the_mind",
                "interpreters": [sys.executable],
                "shim": "python",
                "ingress_rule": "allow_all_ingress",
                # Deny-all: the mind drives the AI — an explicit lateral grant.
                "can_call": {"the_ai": "*"},
            },
        )
    )["id"]
    r = await k.send(mind, {"type": "start", "code": PLAN_MIND_CODE})
    await _wait_ready(k, mind, r["job_id"])
    return mind, fp


async def test_mind_plans_executes_and_asks_single(seeded_kernel, store_agent):
    """The mind plans, executes a CALL step, then ASKs a SINGLE-choice question — it
    emits {type:question} and SUSPENDS (no done). Answering `accept` feeds the choice
    back as the next AI-agent turn, and only then does the turn seal."""
    k = seeded_kernel
    mind, fp = await _spawn_plan_mind(
        k,
        store_agent,
        [
            ["PLAN: list the agents, then confirm"],
            ["STEP 1: CALL kernel_state list_agents"],
            [
                'STEP 2: ASK {"kind":"single","question":"Proceed?","header":"Go",'
                '"options":[{"value":"yes"},{"value":"no"}]}'
            ],
            ["FINAL: proceeded with yes"],
        ],
    )
    try:
        q = _watch(k, mind)
        ack = await k.send(
            mind, {"type": "send", "text": "set it up", "client_id": "c"}
        )
        assert ack.get("queued") is True, ack

        qev, before = await _collect(q, "question")
        assert qev is not None, f"no question emitted; saw {before}"
        assert qev["kind"] == "single" and qev.get("options"), qev
        assert qev.get("client_id") == "c" and qev.get("source"), qev
        # The turn genuinely SUSPENDED — no done leaked before the human was asked.
        assert all(e.get("type") != "done" for e in before), (
            f"done before question: {before}"
        )

        rid = qev["request_id"]
        a = await k.send(
            mind,
            {"type": "answer", "request_id": rid, "action": "accept", "value": "yes"},
        )
        assert a.get("queued") is True, a

        done_ev, rest = await _collect(q, "done")
        assert done_ev is not None, f"turn never sealed; saw {rest}"
        say = next((e for e in rest if e.get("type") == "say"), None)
        assert say and "yes" in say.get("text", ""), f"final say: {say}"
        # The full cycle ran: plan + call + ask + post-answer final = 4 AI passes,
        assert fp.calls == 4, f"expected 4 AI passes, got {fp.calls}"
        # and the human answer really became the NEXT AI-agent turn.
        assert "yes" in json.dumps(fp.requests[3], default=str), fp.requests[3]
    finally:
        ot._providers.pop("the_ai", None)


async def test_mind_plans_executes_and_asks_multi(seeded_kernel, store_agent):
    """MULTI-answer: the accepted `value` is an ARRAY; every chosen value must reach
    the next AI-agent turn."""
    k = seeded_kernel
    mind, fp = await _spawn_plan_mind(
        k,
        store_agent,
        [
            ["PLAN: choose features"],
            [
                'STEP 1: ASK {"kind":"multi","question":"Enable which?",'
                '"options":[{"value":"cache"},{"value":"audit"},{"value":"sso"}]}'
            ],
            ["FINAL: enabled cache and sso"],
        ],
    )
    try:
        q = _watch(k, mind)
        ack = await k.send(
            mind, {"type": "send", "text": "configure", "client_id": "c"}
        )
        assert ack.get("queued") is True, ack

        qev, before = await _collect(q, "question")
        assert qev is not None and qev["kind"] == "multi", (qev, before)
        assert all(e.get("type") != "done" for e in before), (
            f"done before question: {before}"
        )

        rid = qev["request_id"]
        a = await k.send(
            mind,
            {
                "type": "answer",
                "request_id": rid,
                "action": "accept",
                "value": ["cache", "sso"],
            },
        )
        assert a.get("queued") is True, a

        done_ev, rest = await _collect(q, "done")
        assert done_ev is not None, f"turn never sealed; saw {rest}"
        assert fp.calls == 3, f"expected 3 AI passes, got {fp.calls}"
        # BOTH selected values became the next AI-agent turn (the array survived).
        blob = json.dumps(fp.requests[2], default=str)
        assert "cache" in blob and "sso" in blob, fp.requests[2]
    finally:
        ot._providers.pop("the_ai", None)


async def test_mind_ask_declined(seeded_kernel, store_agent):
    """NO-FALLBACKS at the mind boundary: a `decline` (no value) makes the mind take a
    distinct branch (here: abort) and it NEVER invents a value or runs another AI
    pass as if one were chosen."""
    k = seeded_kernel
    mind, fp = await _spawn_plan_mind(
        k,
        store_agent,
        [
            ["PLAN: confirm the destructive step"],
            ['STEP 1: ASK {"kind":"confirm","question":"Delete everything?"}'],
            # No further script: a decline MUST NOT trigger another AI pass.
        ],
    )
    try:
        q = _watch(k, mind)
        ack = await k.send(mind, {"type": "send", "text": "clean up", "client_id": "c"})
        assert ack.get("queued") is True, ack

        qev, before = await _collect(q, "question")
        assert qev is not None and qev["kind"] == "confirm", (qev, before)

        rid = qev["request_id"]
        a = await k.send(
            mind, {"type": "answer", "request_id": rid, "action": "decline"}
        )
        assert a.get("queued") is True, a

        done_ev, rest = await _collect(q, "done")
        assert done_ev is not None, f"turn never sealed; saw {rest}"
        say = next((e for e in rest if e.get("type") == "say"), None)
        txt = (say or {}).get("text", "").lower()
        assert "abort" in txt or "declin" in txt, (
            f"expected an abort/decline branch: {say}"
        )
        # It did NOT run a post-answer AI pass (plan + ask only)…
        assert fp.calls == 2, (
            f"a declined ask must not run another pass, got {fp.calls}"
        )
        # …and never injected a fabricated answer into any AI turn.
        assert all(
            "ANSWER" not in json.dumps(req, default=str) for req in fp.requests
        ), fp.requests
    finally:
        ot._providers.pop("the_ai", None)
