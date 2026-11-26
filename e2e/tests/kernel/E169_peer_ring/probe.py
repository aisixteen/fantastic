"""kernel/peer_ring — three peers ask each other in a ring, with no deadlock (#795). NO MODEL.

Three local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the models of three
peers; the kernel's own `ai_chat.tools` chats, each completing through its own
`vllm_backend` provider, run on the PACKAGED rust host, every call from the test through
`web_rest`. The memory store is a `sqlite_store` (`stores/memory.db`), so the rows
outlive a kill.

THE RING (everything async): A asks B, B asks C, C asks A, all kicked at once. A scripted
peer reads its whole request, as a model does: a `KICK` makes it `send` its question to
the next peer; every question it has not answered yet makes it answer the asker with its
own `send`, `re` set to the question's `msg`; then `done`. The E37 deadlock was this
ring: each sender's call waited for the receiver's pass, which held the next question.

Checks: each kick is acked at once (`accepted`, its `re` a row); every peer reads the
answer to its own question, from the peer it asked, `re` naming its question's `msg`;
no task of a peer is left in `ps`; the ring ends well inside the mock's own time (no
600 s ceiling); each peer ran 2 or 3 passes.

THE RESTART LEG: B is sent one more message and its model holds that request; the
kernel process group is KILLED while the request is out, and a new kernel boots on the
same folder. B's pass re-runs ONCE over the message it never answered (stored and acked
before the kill: nothing acked is lost), and its answer is stored once.
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

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import oslayer
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
PEERS = ["ring_a", "ring_b", "ring_c"]
TOKEN = secrets.token_hex(4)
RECALL_MARK = "rebuilding your own working context"
DB = "stores/memory.db"
DONE = '<call name="done"></call>'


def send_call(target: str, text: str, re_: str | None = None) -> str:
    routing = f"<target_id>{target}</target_id>" + (f"<re>{re_}</re>" if re_ else "")
    return (f'<call name="send"><routing>{routing}</routing>'
            f"<payload><type>send</type><text>{text}</text></payload></call>")


def users(req: dict) -> list[str]:
    out = []
    for m in req.get("messages") or []:
        if m.get("role") == "user":
            c = m.get("content")
            out.append(c if isinstance(c, str) else json.dumps(c))
    return out


def fresh(req: dict) -> list[str]:
    """The user messages after the request's last answer: what its pass reads anew."""
    msgs = req.get("messages") or []
    at = max((i for i, m in enumerate(msgs) if m.get("role") == "assistant"), default=-1)
    return [m.get("content") if isinstance(m.get("content"), str) else json.dumps(m.get("content"))
            for m in msgs[at + 1:] if m.get("role") == "user"]


def is_recall(req: dict) -> bool:
    return any(RECALL_MARK in u for u in users(req))


def ring_model(me: str, nxt: str, hold: threading.Event | None = None,
               holding: threading.Event | None = None):
    """A scripted peer: see the module docstring. `hold`: a `SLOW` message's request is
    held until the event is set (the restart leg's in-flight request), `holding` set
    while it is."""
    answered: set[str] = set()
    lock = threading.Lock()

    def respond(req: dict) -> str | None:
        if is_recall(req):
            return "The head is fine as it is."
        calls = []
        with lock:
            for u in users(req):
                if f"KICK-{TOKEN}" in u and "kick" not in answered:
                    answered.add("kick")
                    calls.append(send_call(nxt, f"QUESTION-{TOKEN} from {me}"))
                m = re.match(r"\[from (\S+) · msg (\d+)", u)
                if m and f"QUESTION-{TOKEN}" in u and m.group(2) not in answered:
                    answered.add(m.group(2))
                    calls.append(send_call(m.group(1), f"ANSWER-{TOKEN} from {me}", m.group(2)))
                if f"SLOW-{TOKEN}" in u and hold is not None:
                    if holding is not None:
                        holding.set()
                    hold.wait(120)
        return "".join(calls) + DONE

    return respond


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    mocks = {p: VllmMock(model=f"mock-{p}").start() for p in PEERS}
    work = Path(tempfile.mkdtemp(prefix="e2e-peer-ring-"))
    hk = HostKernel(spec, work).boot()
    bad: list[str] = []
    hold = threading.Event()
    holding = threading.Event()

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def until(what: str, cond, bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if cond():
                return True
            time.sleep(0.1)
        print(f"  (waited {bound}s for {what})", flush=True)
        return False

    def quiet() -> bool:
        tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
        return not [t for t in tasks if t.get("agent") in PEERS]

    def passes(p: str) -> list[dict]:
        return [t for t in mocks[p].turns() if not is_recall(t["body"])]

    try:
        for p in PEERS:
            prov = f"{p}_prov"
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=prov,
                            endpoint=mocks[p].url, model=mocks[p].model, kv_slots=4,
                            kv_overflow="evict", context_window=262144,
                            memory_store_id="memory", can={"memory": ALL})
            need(rec.get("id") == prov, f"create {prov}: {str(rec)[:200]}")
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=p,
                            provider_id=prov, model=mocks[p].model, max_tokens=8192,
                            context_window=262144, memory_store_id="memory",
                            can={"memory": ALL, prov: PROVIDER})
            need(rec.get("id") == p, f"create {p}: {str(rec)[:200]}")
        # each peer's lines on the other two, once all three exist
        for p in PEERS:
            can = {"memory": ALL, f"{p}_prov": PROVIDER, **{q: ALL for q in PEERS if q != p}}
            r = hk.send(hk.root_id, {"type": "update_agent", "id": p, "can": can})
            need(not r.get("error"), f"{p}: its lines on the other peers: {str(r)[:200]}")

        # the rows go to a durable store, for the restart leg
        hk.close()
        rec_path = work / ".fantastic/agents/web/agents/rest/agents/memory/agent.yaml"
        rec = yaml.safe_load(rec_path.read_text())
        rec["handler_module"] = "sqlite_store.tools"
        rec["path"] = DB
        rec_path.write_text(yaml.safe_dump(rec, sort_keys=False))
        hk.boot()

        for i, p in enumerate(PEERS):
            nxt = PEERS[(i + 1) % len(PEERS)]
            mocks[p].responder = (ring_model(p, nxt, hold, holding) if p == "ring_b"
                                  else ring_model(p, nxt))

        # 1 · the kicks: each acked at once, before any pass
        started = time.monotonic()
        for p in PEERS:
            t0 = time.monotonic()
            ack = hk.send(p, {"type": "send", "text": f"KICK-{TOKEN}: ask the next peer."}, timeout=30)
            took = time.monotonic() - t0
            need(ack.get("accepted") is True and isinstance(ack.get("re"), int) and took < 5,
                 f"{p}: the kick is acked at once ({took:.2f}s): {str(ack)[:200]}")

        # 2 · every peer reads the answer to its own question
        def answered(p: str, frm: str) -> list[str]:
            return [u for t in passes(p) for u in users(t["body"])
                    if f"ANSWER-{TOKEN} from {frm}" in u]

        for i, p in enumerate(PEERS):
            nxt = PEERS[(i + 1) % len(PEERS)]
            need(until(f"{p}'s answer", lambda p=p, nxt=nxt: bool(answered(p, nxt))),
                 f"{p} reads the answer to its question, from {nxt}")
        need(until("the ring to settle", quiet), "no task of a peer is left in `ps`")
        took = time.monotonic() - started
        need(took < 60, f"the ring ends inside the mock's own time, no ceiling: {took:.1f}s")

        for i, p in enumerate(PEERS):
            nxt = PEERS[(i + 1) % len(PEERS)]
            # the question `p` sent, as `nxt` read it: `[from p · msg M`
            asked = [re.match(r"\[from \S+ · msg (\d+)", u).group(1)
                     for t in passes(nxt) for u in users(t["body"])
                     if u.startswith(f"[from {p} · msg ") and f"QUESTION-{TOKEN} from {p}" in u]
            got = answered(p, nxt)
            res = {m.group(1) for u in got for m in [re.search(r" · re (\d+)", u)] if m}
            need(bool(asked) and res == {asked[0]},
                 f"{p}: the answer's `re` names its question's `msg`: asked {asked[:1]}, re {sorted(res)}")
            n = len(passes(p))
            need(n in (2, 3), f"{p}: {n} passes (2, or 3 when the answer came in a pass of its own)")

        # 3 · THE RESTART LEG: B's request held, the kernel killed, a new one booted
        n_b = len(passes("ring_b"))
        ack = hk.send("ring_b", {"type": "send", "text": f"SLOW-{TOKEN}: one more thing."}, timeout=30)
        need(ack.get("accepted") is True, f"the last message is acked: {str(ack)[:200]}")
        need(until("B's held request", holding.is_set), "B's model holds the request (it is out)")
        time.sleep(1.5)  # the flusher writes the rows a beat after they are made
        before_kill = {p: len(passes(p)) for p in ("ring_a", "ring_c")}
        oslayer.kill_group(hk.proc.pid)
        try:
            hk.proc.wait(timeout=10)
        except Exception:  # noqa: BLE001 — named below
            pass
        need(hk.proc.poll() is not None, "the first kernel process is killed")
        hk.close()
        # free the mock's held handler thread (its answer reaches nobody); the mock
        # records a request once answered, so the held one is counted from now on
        held_n = len(passes("ring_b"))
        hold.set()
        until("the held request to be recorded", lambda: len(passes("ring_b")) > held_n, 10)
        n_b = len(passes("ring_b"))
        hk.boot()

        def slow_passes() -> list[dict]:
            # the passes that read the unanswered message anew (later ones carry it in
            # their history, above an answer)
            return [t for t in passes("ring_b")[n_b:]
                    if any(f"SLOW-{TOKEN}" in u for u in fresh(t["body"]))]

        need(until("B's pass after the restart", lambda: bool(slow_passes())),
             "after the restart, B's pass re-runs over the message it never answered")
        need(until("the agents to settle", quiet), "nothing of the peers is left in `ps`")
        time.sleep(1.0)
        later = [fresh(t["body"])[-1][:160] for t in passes("ring_b")[n_b:] if fresh(t["body"])]
        need(len(slow_passes()) == 1, f"B's pass re-runs ONCE over the message: {len(slow_passes())}: {later}")
        after = {p: len(passes(p)) for p in ("ring_a", "ring_c")}
        need(after == before_kill, f"nothing else runs after the restart: {before_kill} -> {after}")
    finally:
        hold.set()
        hk.close()
        for m in mocks.values():
            m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
