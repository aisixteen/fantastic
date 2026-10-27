"""kernel/sqlite_history_restart — an agent's history survives a kernel restart. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`) plays the model; the agent is the
kernel's own `vllm_backend` agent (ai_core) on the PACKAGED host of `RUNTIME` (python |
rust), every call from the test through `web_rest`. The agent's `memory_store_id` is a
`sqlite_store` agent `hist` whose file is its record's `path` (`stores/hist.db`).

SCRIPT:
  1. turn one: the test sends the agent a message carrying a unique token; the model
     answers with another token and `done`. The store `dump` is read: the agent's rows
     (user, assistant, call) are recorded with their ids.
  2. the kernel process is STOPPED (`hk.close()`) and a NEW one is booted on the SAME
     directory (`hk.boot()`: the tree and the file are all that is shared). Nothing is
     dumped or injected.
  3. turn two: a second message, a second scripted answer. (The restarted agent first
     asks the model to rebuild its working context from the rows it read back; the
     probe answers that request apart, so it takes no scripted answer.)

Checks: the first turn left user and assistant rows under `<guid>|bot`; after the
restart the store holds EXACTLY those rows (ids and content); the second request to the
model carries the first token (the agent hydrated its history from the file); the new
rows have ids above the old maximum; `stores/hist.db` is a file and the store's
`reflect` says `durable` with the record's `path`.
"""

from __future__ import annotations

import json
import os
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, STORE = "bot", "hist"
DB = "stores/hist.db"
MODEL = "mock-scripted"
Q1, A1 = "E136-q-" + secrets.token_hex(4), "E136-a-" + secrets.token_hex(4)
Q2, A2 = "E136-q-" + secrets.token_hex(4), "E136-a-" + secrets.token_hex(4)
DONE = '\n<call name="done"></call>'


def last_user(req: dict) -> str:
    """The last user message of a chat request, as text."""
    for msg in reversed(req.get("messages") or []):
        if msg.get("role") == "user":
            c = msg.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def recall_only(req: dict) -> str | None:
    """A restarted agent first asks the model to rebuild its working context from the
    rows it read back: answered here with a plain line, so it never takes a scripted
    answer meant for a turn."""
    if "rebuilding your own working context" in last_user(req):
        return "The head is fine as it is."
    return None


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = VllmMock(model=MODEL).start()
    m.responder = recall_only
    work = Path(tempfile.mkdtemp(prefix="e2e-sqlite-history-"))
    hk = HostKernel(spec, work, reach=[AI, STORE]).boot()
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def idle(bound: float = 60) -> bool:
        """Poll (bounded) until the agent has no task in flight (`ps`)."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def agent_rows() -> list[dict]:
        """The agent's rows in the store, as the store's `dump` holds them."""
        rows = (hk.send(STORE, {"type": "dump"}).get("tables") or {}).get("rows") or []
        return [r["row"] for r in rows if str(r.get("owner", "")).endswith(f"|{AI}")]

    def owners() -> list[str]:
        rows = (hk.send(STORE, {"type": "dump"}).get("tables") or {}).get("rows") or []
        return sorted({str(r.get("owner")) for r in rows})

    try:
        rec = hk.create(parent_id="rest", handler_module="sqlite_store.tools", id=STORE,
                        path=DB, callable_by={AI: "*", "$parent": "*"})
        need(rec.get("id") == STORE and not rec.get("error"),
             f"create the sqlite store from its record's path: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=AI,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id=STORE,
                        can_call={"$self": "*", STORE: "*"},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == AI, f"create the agent on that store: {str(rec)[:200]}")

        # ── turn one ─────────────────────────────────────────────────────────
        m.reply(f"Noted {A1}.{DONE}")
        out = hk.send(AI, {"type": "send", "text": f"Remember {Q1}."})
        need(not out.get("error"), f"the first send is accepted: {str(out)[:200]}")
        need(idle(), "turn one settles")
        before = agent_rows()
        text = json.dumps(before)
        need(Q1 in text and A1 in text,
             f"the store holds the agent's question and answer: {len(before)} rows")
        old_ids = sorted(r["id"] for r in before)
        old_max = old_ids[-1] if old_ids else 0
        need(len(before) >= 2 and len(set(old_ids)) == len(old_ids),
             f"user, assistant (and call) rows with distinct ids: {old_ids}")
        old_owners = owners()
        need(any(o.endswith(f"|{AI}") for o in old_owners), f"rows are keyed by the agent: {old_owners}")

        # ── stop the process, boot a NEW one on the same dir; nothing is injected ──
        hk.close()
        need(hk.proc is not None and hk.proc.poll() is not None, "the first kernel process is gone")
        hk.boot()
        need((work / DB).is_file(), f"the file is on disk: {DB}")
        after = agent_rows()
        need(sorted(r["id"] for r in after) == old_ids,
             f"the same row ids after the restart: {[r['id'] for r in after]} vs {old_ids}")
        by_id = {r["id"]: r for r in after}
        need(all(by_id.get(r["id"]) == r for r in before),
             "every old row is back with the same content")
        need(owners() == old_owners, f"the same owners (same guid): {owners()} vs {old_owners}")

        # ── turn two: the agent hydrated its history ─────────────────────────
        seen = len(m.turns())
        m.reply(f"Also noted {A2}.{DONE}")
        out = hk.send(AI, {"type": "send", "text": f"And now {Q2}."})
        need(not out.get("error"), f"the second send is accepted: {str(out)[:200]}")
        need(idle(), "turn two settles")
        turns = m.turns()[seen:]
        need(bool(turns), "the second message reached the model")
        req = json.dumps(turns[-1]["body"].get("messages") if turns else [])
        need(Q1 in req and A1 in req,
             "the second request carries the first turn: the agent read its history from the file")
        need(Q2 in req, "…and the new message")
        for i, t in enumerate(turns):
            ms = t["body"].get("messages") or []
            print(f"  ── request {i + 1} of turn two: {len(ms)} messages, last: "
                  f"{json.dumps(ms[-1] if ms else None)[:200]}", flush=True)
        final = agent_rows()
        new = [r for r in final if r["id"] not in old_ids]
        need(Q2 in json.dumps(new) and A2 in json.dumps(new),
             f"the second turn's rows were appended: {len(new)} new rows")
        need(bool(new) and min(r["id"] for r in new) > old_max,
             f"new ids are above the old maximum {old_max}: {sorted(r['id'] for r in new)}")

        # ── the store says what it is ────────────────────────────────────────
        refl = hk.send(STORE, {"type": "reflect"})
        need(refl.get("durable") is True and refl.get("path") == DB,
             f"reflect says durable and names the record's path: {str(refl)[:200]}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
