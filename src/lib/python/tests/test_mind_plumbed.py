"""PLUMBED MIND (python) — a python-authored agent on `python_runtime` drives a
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
import secrets
import time

import ollama_backend.tools as ot


class _FakeProvider:
    """Yields a scripted token stream per `chat()` call (one per pass)."""

    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0

    async def chat(self, messages):
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
                    "handler_module": "python_runtime.tools",
                    "id": "the_mind",
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
