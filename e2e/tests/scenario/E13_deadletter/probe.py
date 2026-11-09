"""scenario/E13_deadletter, the deadline case — a message past its deadline goes to
the dead letter and is withdrawn from the busy target. NO MODEL.

Two local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model of a LEAD and
a WORKER; the kernel's own `vllm_backend` agents (ai_core) run on the PACKAGED host of
`RUNTIME` (python | rust), every call from the test through `web_rest`. The root
record carries `send_timeout: 3`.

  1. the WORKER is busy: its scripted answer is slow (`SLOW` s);
  2. `status` on the busy worker answers at once (never queued behind the turn);
  3. the LEAD sends the worker `PING-<token>` (no timeout of its own): the worker's
     chain is alive, so it is HELD; `status` counts it and names the lead in
     `pending_from`;
  4. at 3 s the message is withdrawn into the dead letter: the lead is told
     `hit its 3.0s deadline → dead letter <pid>; resend or drop`, the worker's
     history holds a `withdrawn` row naming the pid;
  5. the lead resends (`<timeout>30</timeout>`) and drops the dead letter
     (`ack_deadletter`); the worker answers only the resend, in ONE wake pass.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import shutil
import sys
import tempfile
import threading
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
LEAD, WORKER = "lead", "worker"
TOKEN = secrets.token_hex(4)
SLOW = 8
BOUND = 90.0
SLOW_MARK = "<slow>"


class SlowVllm(VllmMock):
    """The vllm mock, plus a slow answer: one starting with `<slow>` waits `SLOW` s
    before it streams (the marker is stripped)."""

    def turn(self, h, req, text):
        if text.startswith(SLOW_MARK):
            time.sleep(SLOW)
            text = text[len(SLOW_MARK):]
        return super().turn(h, req, text)


class Bg:
    """One REST call on a thread — the caller of a call that is in flight."""

    def __init__(self, hk: HostKernel, target: str, payload: dict):
        self.out: dict | None = None
        self.t = threading.Thread(target=self._run, args=(hk, target, payload), daemon=True)
        self.t.start()

    def _run(self, hk, target, payload):
        try:
            self.out = hk.send(target, payload, timeout=BOUND)
        except Exception as e:  # noqa: BLE001 — a dropped connection is the evidence
            self.out = {"transport_error": f"{type(e).__name__}: {e}"}

    def join(self) -> dict | None:
        self.t.join(BOUND)
        return self.out


def last_user(req: dict) -> str:
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    lead_m = VllmMock(model="mock-lead").start()
    work_m = SlowVllm(model="mock-worker").start()
    work = Path(tempfile.mkdtemp(prefix="e2e-deadletter-deadline-"))
    hk = HostKernel(spec, work, reach=[LEAD, WORKER, f"{LEAD}_prov", f"{WORKER}_prov"]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def callable_by(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        rec = next(a for a in r.get("agents") or [] if a.get("id") == aid)
        return dict(rec.get("callable_by") or {})

    def turns_at_least(m: VllmMock, n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.turns()

    def idle(bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            # a chain's wake watchdog sleeps out the send ceiling: it is not work
            left = [t for t in tasks if t.get("agent") in (LEAD, WORKER)
                    and t.get("verb") != "chain_wake_watch"]
            if not left:
                return True
            time.sleep(0.05)
        print(f"  (ps still shows: {left})", flush=True)
        return False

    def send_call(text: str, timeout: str = "") -> str:
        t = f"<timeout>{timeout}</timeout>" if timeout else ""
        return (f'<call name="send">\n<routing><target_id>{WORKER}</target_id>{t}</routing>\n'
                f"<payload><type>send</type><text>{text}</text></payload>\n</call>")

    state = {"resent": False}

    def lead_responder(req: dict) -> str | None:
        u = last_user(req)
        if TOKEN in u and "Ping the worker" in u:
            return send_call(f"PING-{TOKEN}")
        m = re.search(r"dead letter (\d+)", u)
        if m and not state["resent"]:
            state["resent"] = True
            return (send_call(f"RESEND-{TOKEN}", "30")
                    + f'\n<call name="send">\n<routing><target_id>{root}</target_id></routing>\n'
                      f"<payload><type>ack_deadletter</type><id>{m.group(1)}</id></payload>\n</call>")
        return None  # the queue: `done`

    try:
        r = hk.send(root, {"type": "update_agent", "id": root, "send_timeout": 3,
                           "callable_by": {**callable_by(root), LEAD: ["ack_deadletter"]}})
        need(r.get("updated") is True, f"the root carries send_timeout 3 and admits the lead's ack: {str(r)[:200]}")
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), LEAD: "*", WORKER: "*",
                                           f"{LEAD}_prov": "*", f"{WORKER}_prov": "*"}})
        need(r.get("updated") is True, f"memory accepts the AIs: {str(r)[:200]}")

        def make_ai(aid: str, mock: VllmMock, can_call: dict, cb: dict) -> None:
            common = dict(max_tokens=8192, context_window=262144, memory_store_id="memory")
            if RUNTIME == "rust":  # a provider (the vllm_backend) and the chat naming it
                prov = f"{aid}_prov"
                rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=prov,
                                endpoint=mock.url, model=mock.model, kv_slots=4,
                                kv_overflow="evict", **common,
                                can_call={"$self": "*", "memory": "*"},
                                callable_by={"$self": "*", "$parent": "*", aid: "*"})
                need(rec.get("id") == prov, f"create {prov}: {str(rec)[:200]}")
                rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                                provider_id=prov, model=mock.model, **common,
                                can_call={**can_call, prov: ["complete", "watch"]}, callable_by=cb)
            else:
                rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=aid,
                                endpoint=mock.url, model=mock.model, kv_slots=4,
                                kv_overflow="evict", **common, can_call=can_call, callable_by=cb)
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        make_ai(WORKER, work_m, {"$self": "*", "memory": "*"},
                {"$self": "*", "$parent": "*", "rest": "*", LEAD: "*"})
        make_ai(LEAD, lead_m, {"$self": "*", "memory": "*", root: ["ack_deadletter"], WORKER: "*"},
                {"$self": "*", "$parent": "*", "rest": "*"})
        r = hk.send(root, {"type": "update_agent", "id": WORKER,
                           "callable_by": {**callable_by(WORKER), LEAD: "*"}})
        need(r.get("updated") is True, f"the worker admits the lead: {str(r)[:200]}")

        # 1 · the worker is busy on a slow answer
        work_m.reply(f'{SLOW_MARK}busy\n<call name="done"></call>')
        busy = Bg(hk, WORKER, {"type": "send", "text": "work on something slow"})
        end = time.monotonic() + 20
        st: dict = {}
        while time.monotonic() < end and not st.get("generating"):
            st = hk.send(WORKER, {"type": "status"})
            time.sleep(0.05)
        need(bool(st.get("generating")), "the worker is generating (busy)")
        # 2 · status answers at once while the turn runs
        t0 = time.monotonic()
        st = hk.send(WORKER, {"type": "status"})
        need(time.monotonic() - t0 < 1.0 and st.get("generating") is True,
             f"status answers at once on a busy agent: {time.monotonic() - t0:.2f}s")

        # 3 · the lead pings the worker; the message is held and counted
        lead_m.responder = lead_responder
        work_m.reply('Got RESEND.\n<call name="done"></call>')
        lead_task = Bg(hk, LEAD, {"type": "send", "text": f"Ping the worker ({TOKEN})."})
        end = time.monotonic() + 3
        seen = {}
        while time.monotonic() < end:
            seen = hk.send(WORKER, {"type": "status"})
            if LEAD in (seen.get("pending_from") or []) and (seen.get("others_pending") or 0) >= 1:
                break
            time.sleep(0.05)
        need(LEAD in (seen.get("pending_from") or []) and (seen.get("others_pending") or 0) >= 1,
             f"status counts the held message and names the lead: {str(seen)[:300]}")

        # 4 · past its deadline: dead letter, notice, withdrawn row
        turns = turns_at_least(lead_m, 2)
        need(len(turns) >= 2, "the lead was told something after its ping")
        notice = re.search(r"hit its 3\.0s deadline → dead letter (\d+); resend or drop",
                           last_user(turns[1]["body"]) if len(turns) > 1 else "")
        need(notice is not None,
             f"the lead's request carries the dead-letter notice: {last_user(turns[1]['body'])[:300] if len(turns) > 1 else ''!r}")
        pid = notice.group(1) if notice else "?"
        hist = hk.send(WORKER, {"type": "history", "limit": 500})
        wrows = [r for r in hist.get("rows") or [] if r.get("kind") == "withdrawn"]
        need(len(wrows) == 1, f"the worker's history holds one withdrawn row: {len(wrows)}")
        full = hk.send(WORKER, {"type": "recall", "id": wrows[0].get("id")}) if wrows else {}
        text = json.dumps(full)
        need(f"dead letter {pid}" in text, f"the withdrawn row names the dead letter {pid}: {text[:200]}")

        # 5 · resend + drop; the worker answers only the resend
        lead_task.join()
        busy.join()
        need(idle(), "the chains settle (no AI task left in `ps`)")
        time.sleep(0.5)
        w_turns = work_m.turns()
        need(len(w_turns) == 2,
             f"the worker made exactly 2 requests (the slow one, the wake for the resend): {len(w_turns)}")
        need(not any(f"PING-{TOKEN}" in last_user(t["body"]) for t in w_turns),
             "the withdrawn message was never the worker's turn to answer")
        l_turns = lead_m.turns()
        last = last_user(l_turns[-1]["body"]) if l_turns else ""
        need("(deadline 30s)" in last, f"the resend's reply states its deadline: {last[:300]!r}")
        need("Got RESEND" in last, "the resend was answered")
        need("<acked>true</acked>" in last, "the dead letter was dropped (ack_deadletter)")
    finally:
        hk.close()
        lead_m.stop()
        work_m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
