"""scenario/E13_deadletter, the deadline case — a call past its deadline goes to the
dead letter; a message to a busy AI is acked, never held (#795). NO MODEL.

Two local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model of a LEAD and
a WORKER; the kernel's own `vllm_backend` agents (ai_core) run on the PACKAGED rust
host, every call from the test through `web_rest`. The root record carries
`send_timeout: 3`, written into the state folder before boot (an operator's hand
edit: no agent may update the root). Each chat holds its own lines — the store, its
provider, and the lead `send` on the worker and `sleep` on a `slow_probe` agent; the
worker holds none on the lead, so a send from it is the unknown answer.

  1. the WORKER is busy: its scripted answer is slow (`SLOW` s);
  2. `status` on the busy worker answers at once (never queued behind the pass);
  3. the LEAD sends the busy worker `PING-<token>`: it is acked at once, once stored
     (the ack, `<accepted>true</accepted>` and no dead letter, is a row of the lead's that
     wakes no pass), and the worker reads it in ONE pass after its slow one;
  4. the lead calls `slow sleep` (longer than the deadline): at 3 s the call goes to
     the dead letter and the lead is told `hit its 3.0s deadline → dead letter <pid>;
     resend or drop`;
  5. the lead resends (`<timeout>30</timeout>`) and drops the dead letter
     (`ack_deadletter`); the resend is answered inside it, its reply reaching the lead once.

Rust lane only, a second part (#568): the kernel's ledger store becomes a `sqlite_store`
(`stores/memory.db`: the `memory` record is rewritten while the kernel is stopped), and
  6. the lead's slow call passes the deadline again: a dead letter on the sqlite ledger,
     the lead is told its id;
  7. the kernel is stopped and a NEW one booted on the same folder; the lead (its own
     `ledger` call: the ledger is scoped to its caller) sees the dead letter, `timed_out`,
     with the same id, and resends the call: its reply reaches the lead once. The lead's
     `ack_deadletter` after the restart is printed, not required.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER, call as line_call, is_unknown
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
LEAD, WORKER, SLOWP = "lead", "worker", "slow"
TOKEN = secrets.token_hex(4)
TOKEN2 = secrets.token_hex(4)
RECALL_MARK = "rebuilding your own working context"
DB = "stores/memory.db"
SLOW = 8
SLOW_MARK = "<slow>"
#: the slow call's length: past the 3 s deadline, well inside the resend's 30 s
NAP = 5


class SlowVllm(VllmMock):
    """The vllm mock, plus a slow answer: one starting with `<slow>` waits `SLOW` s
    before it streams (the marker is stripped)."""

    def on_pass(self, h, req, text):
        if text.startswith(SLOW_MARK):
            time.sleep(SLOW)
            text = text[len(SLOW_MARK):]
        return super().on_pass(h, req, text)


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
    # every agent here is under `rest` (position)
    hk = HostKernel(spec, work)
    root = hk.root_id
    # THE DEADLINE ON THE ROOT, before boot: `rest` holds no `update` there
    (work / ".fantastic").mkdir(parents=True, exist_ok=True)
    (work / ".fantastic" / "agent.yaml").write_text(yaml.safe_dump(
        {"id": root, "handler_module": "kernel_state.tools", "send_timeout": 3}, sort_keys=False))
    hk.boot()
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def passes_at_least(m: VllmMock, n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.passes()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.passes()

    def idle(bound: float = 60) -> bool:
        end = time.monotonic() + bound
        left: list = []
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            left = [t for t in tasks if t.get("agent") in (LEAD, WORKER, SLOWP)]
            if not left:
                return True
            time.sleep(0.05)
        print(f"  (ps still shows: {left})", flush=True)
        return False

    def send_call(text: str) -> str:
        return (f'<call name="send">\n<routing><target_id>{WORKER}</target_id></routing>\n'
                f"<payload><type>send</type><text>{text}</text></payload>\n</call>")

    def sleep_call(timeout: str = "") -> str:
        t = f"<timeout>{timeout}</timeout>" if timeout else ""
        return (f'<call name="send">\n<routing><target_id>{SLOWP}</target_id>{t}</routing>\n'
                f"<payload><type>sleep</type><seconds>{NAP}</seconds></payload>\n</call>")

    def ack_call(pid_: str) -> str:
        return (f'\n<call name="send">\n<routing><target_id>{root}</target_id></routing>\n'
                f"<payload><type>ack_deadletter</type><id>{pid_}</id></payload>\n</call>")

    state = {"resent": False}

    def lead_responder(req: dict) -> str | None:
        u = last_user(req)
        if RECALL_MARK in u:
            return "The head is fine as it is."
        if TOKEN in u and "Ping the worker" in u:
            return send_call(f"PING-{TOKEN}")
        if TOKEN in u and "Take a nap" in u:
            return sleep_call()
        m = re.search(r"dead letter (\d+); resend or drop", u)
        if m and not state["resent"]:
            state["resent"] = True
            return sleep_call("30") + ack_call(m.group(1))
        return None  # the queue: `done`

    try:
        need(yaml.safe_load((work / ".fantastic" / "agent.yaml").read_text()).get("send_timeout") == 3,
             "the root record carries send_timeout 3")

        def make_ai(aid: str, mock: VllmMock, can: dict) -> None:
            common = dict(context_window=262144, memory_store_id="memory")
            # a provider (the vllm_backend) and the chat naming it
            prov = f"{aid}_prov"
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=prov,
                            endpoint=mock.url, model=mock.model, kv_slots=4,
                            kv_overflow="evict", **common, can={"memory": ALL})
            need(rec.get("id") == prov, f"create {prov}: {str(rec)[:200]}")
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                            provider_id=prov, model=mock.model, max_tokens=8192, **common,
                            can={**can, "memory": ALL, prov: PROVIDER})
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        rec = hk.create(parent_id="rest", handler_module="slow_probe.tools", id=SLOWP)
        need(rec.get("id") == SLOWP, f"create {SLOWP}: {str(rec)[:200]}")
        # `ledger` and `ack_deadletter` need no line: each answers the caller's own
        make_ai(WORKER, work_m, {})
        # the lead's lines name ids that exist now
        make_ai(LEAD, lead_m, {WORKER: line_call("send"), SLOWP: line_call("sleep")})
        # NO LINE, NO SEND: a hand holding the worker's exact lines sends to the lead
        # — the unknown answer (the lead holds a line on the worker, never the reverse)
        wcan = (hk.send(WORKER, {"type": "reflect"}) or {}).get("can") or {}
        hand = hk.create(parent_id="rest", handler_module="user_probe.tools", id="worker_hand",
                         answer="no", can=wcan)
        need(hand.get("id") == "worker_hand", f"create worker_hand: {str(hand)[:200]}")
        r = hk.send("worker_hand", {"type": "send", "target": LEAD,
                                    "payload": {"type": "send", "text": "x"}})
        need("sent" in r and is_unknown(r.get("result") or {}, LEAD),
             f"worker → lead send is the unknown answer: {str(r)[:200]}")
        r = hk.send(root, {"type": "delete_agent", "id": "worker_hand"})
        need(not r.get("error"), f"delete worker_hand: {str(r)[:200]}")

        # 1 · the worker is busy on a slow answer
        work_m.reply(f'{SLOW_MARK}busy\n<call name="done"></call>', 'Read the ping.\n<call name="done"></call>')
        busy = hk.send(WORKER, {"type": "send", "text": "work on something slow"})
        need(busy.get("accepted") is True, f"the worker's message is acked: {str(busy)[:200]}")
        end = time.monotonic() + 20
        st: dict = {}
        while time.monotonic() < end and not st.get("generating"):
            st = hk.send(WORKER, {"type": "status"})
            time.sleep(0.05)
        need(bool(st.get("generating")), "the worker is generating (busy)")
        # 2 · status answers at once while the pass runs
        t0 = time.monotonic()
        st = hk.send(WORKER, {"type": "status"})
        need(time.monotonic() - t0 < 1.0 and st.get("generating") is True,
             f"status answers at once on a busy agent: {time.monotonic() - t0:.2f}s")

        # 3 · the lead pings the busy worker: acked at once, never held. The ack is
        #     stored on the lead and wakes no pass of its own (#795, Q2)
        lead_m.responder = lead_responder
        t_ping = time.monotonic()
        hk.send(LEAD, {"type": "send", "text": f"Ping the worker ({TOKEN})."})

        def ack_row() -> str:
            rows = (hk.send(LEAD, {"type": "rows"}) or {}).get("rows") or []
            return next((str(r.get("content")) for r in rows if r.get("role") == "user"
                         and f"· {WORKER} send" in str(r.get("content"))), "")

        end = time.monotonic() + 30
        while time.monotonic() < end and not ack_row():
            time.sleep(0.05)
        took = time.monotonic() - t_ping
        told = ack_row()
        need("<accepted>true</accepted>" in told and "dead letter" not in told and took < 3,
             f"the lead holds the ack of its message to the busy worker, inside the deadline "
             f"({took:.2f}s): {told[:300]!r}")
        need(idle(), "the passes settle (no AI task left in `ps`)")
        need(len(lead_m.passes()) == 1, f"the ack woke no pass of the lead: {len(lead_m.passes())} requests")
        w_passes = passes_at_least(work_m, 2)
        need(len(w_passes) == 2 and f"PING-{TOKEN}" in json.dumps(w_passes[1]["body"].get("messages")),
             f"the worker reads the ping in ONE pass after its slow one: {len(w_passes)} requests")

        # 4 · a call past its deadline: the dead letter, and the lead is told its id
        seen_l = len(lead_m.passes())
        hk.send(LEAD, {"type": "send", "text": f"Take a nap ({TOKEN})."})
        got = passes_at_least(lead_m, seen_l + 2)[seen_l:]
        note = re.search(rf"your message to {SLOWP} hit its 3\.0s deadline → dead letter (\d+); resend or drop",
                         last_user(got[1]["body"]) if len(got) > 1 else "")
        need(note is not None,
             f"the lead's request carries the dead-letter notice: {last_user(got[1]['body'])[:300] if len(got) > 1 else ''!r}")

        # 5 · resend + drop; the resend's reply reaches the lead once
        passes_at_least(lead_m, seen_l + 3, bound=NAP + 30)
        need(idle(NAP + 30), "the passes settle (no AI task left in `ps`)")
        time.sleep(0.5)
        after = [last_user(t["body"]) for t in lead_m.passes()[seen_l:]]
        back = [u for u in after if f"· {SLOWP} sleep" in u and "<slept>" in u and "<error>" not in u]
        need(len(back) == 1, f"the resend, inside its 30 s deadline, is answered and its reply "
             f"reaches the lead once: {[u[:200] for u in after]}")
        need(any("<acked>true</acked>" in u for u in after),
             "the dead letter was dropped (ack_deadletter)")

        # ── part 2 (rust): the dead letter survives a restart on a sqlite ledger (#568) ──
        if RUNTIME != "rust":
            print("  (part 2, the sqlite ledger across a restart, runs on the rust lane)", flush=True)
        else:
            hk.close()
            rec_path = work / ".fantastic/agents/web/agents/rest/agents/memory/agent.yaml"
            rec = yaml.safe_load(rec_path.read_text())
            rec["handler_module"] = "sqlite_store.tools"
            rec["path"] = DB
            rec_path.write_text(yaml.safe_dump(rec, sort_keys=False))
            # a restarted agent first asks its model to rebuild its working context from
            # the rows it read back: answered apart, it takes no scripted answer
            work_m.responder = lambda req: ("The head is fine as it is."  # noqa: E731
                                            if RECALL_MARK in last_user(req) else None)
            hk.boot()
            refl = hk.send("memory", {"type": "reflect"})
            need(refl.get("durable") is True and refl.get("path") == DB,
                 f"the kernel's ledger store is a sqlite_store: {str(refl)[:200]}")
            need((work / DB).is_file(), f"the ledger file is on disk: {DB}")

            # 6 · the lead's call passes its deadline again
            seen_l = len(lead_m.passes())
            state2 = {"pid": None, "resent": False}
            ledger_call = (f'<call name="send">\n<routing><target_id>{root}</target_id></routing>\n'
                           "<payload><type>ledger</type></payload>\n</call>")

            def lead_responder2(req: dict) -> str | None:
                u = last_user(req)
                if RECALL_MARK in u:
                    return "The head is fine as it is."
                if TOKEN2 in u and "Take a nap again" in u:
                    return sleep_call()
                if "Check the ledger" in u:
                    return ledger_call
                pid_ = state2["pid"]
                if pid_ and pid_ in u and "timed_out" in u and not state2["resent"]:
                    state2["resent"] = True
                    return sleep_call("30") + ack_call(pid_)
                return None

            lead_m.responder = lead_responder2
            hk.send(LEAD, {"type": "send", "text": f"Take a nap again ({TOKEN2})."})
            got = passes_at_least(lead_m, seen_l + 2)[seen_l:]
            note = re.search(r"hit its 3\.0s deadline → dead letter (\d+); resend or drop",
                             last_user(got[1]["body"]) if len(got) > 1 else "")
            need(note is not None, "part 2: the lead is told the dead letter's id on the sqlite ledger: "
                 f"{last_user(got[1]['body'])[:300] if len(got) > 1 else ''!r}")
            pid2 = note.group(1) if note else "?"
            state2["pid"] = pid2
            need(idle(NAP + 30), "part 2: the passes settle before the restart")
            time.sleep(1.5)  # the flusher writes the rows a beat after they are made

            # 7 · the kernel is stopped and a new one booted on the same folder
            hk.close()
            need(hk.proc is not None and hk.proc.poll() is not None, "the first kernel process is gone")
            hk.boot()
            need((work / DB).is_file(), f"the ledger file is still on disk: {DB}")
            seen_l = len(lead_m.passes())
            hk.send(LEAD, {"type": "send", "text": "Check the ledger, then deal with it."})
            passes_at_least(lead_m, seen_l + 3, bound=NAP + 30)
            need(idle(NAP + 30), "part 2: the passes settle after the restart")
            time.sleep(0.5)
            l_after = [last_user(t["body"]) for t in lead_m.passes()[seen_l:]]
            ledger_seen = next((u for u in l_after if "timed_out" in u and pid2 in u), "")
            need(bool(ledger_seen),
                 f"after the restart the lead's ledger still holds dead letter {pid2} as timed_out: "
                 f"{[u[:200] for u in l_after]}")
            back = [u for u in l_after if f"· {SLOWP} sleep" in u and "<slept>" in u and "<error>" not in u]
            need(len(back) == 1, f"the resent call's reply reaches the lead once: {[u[:200] for u in l_after]}")
            print("  info: ack_deadletter after the restart: "
                  + ("acked" if any("<acked>true</acked>" in u for u in l_after) else "not acked"),
                  flush=True)
    finally:
        hk.close()
        lead_m.stop()
        work_m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
