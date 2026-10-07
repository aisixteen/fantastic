"""backends/provider_contract — what ai_core needs from a provider, measured through
a live AI agent on whichever backend plug `BACKEND` names.

The wire probe (`wire_responses`) checks the endpoint alone; this
checks the provider AS THE KERNEL USES IT. Three agents on one kernel, one ask each:

  ai         — reads a planted file through `notes` and reports the code word in it:
               tokens stream on its id, the `<call>` it wrote runs, the reply reaches
               it, the answer carries the word, the turn ends `done/ok`, and NO stored
               assistant row carries thinking;
  ai_cut     — the same backend with a 16-token budget: the cut main turn FAILS
               naming the cut (never a fake finish) and stores no assistant row;
  ai_cancel  — a long generation cancelled mid-stream through `ps` → `cancel`: the
               call settles `cancelled`, the turn closes `done/cancelled`, the token
               stream stops, nothing is left in flight.
"""

from __future__ import annotations

import asyncio
import secrets
import sys
import time
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from ai_core.core import THINK_OPEN
from backends import backend as _resolve_backend
from helpers import ctx_score, drive, memory, verdict
from helpers import mem_probe as M
from helpers.ai import _live_tasks
from helpers.kernel import open_kernel
from helpers.sse import check

BK = _resolve_backend()
DONE = M.DONE
AIS = ["ai", "ai_cut", "ai_cancel"]
#: A bound the model cannot finish a turn inside, thinking included.
CUT_BUDGET = 16


def _spy(root, core, aid: str, sink: list) -> asyncio.Task:
    """Every event the agent pushes on its own id, with its arrival time — from the
    live inbox in process, over the kernel's WS face when it is spawned (the same
    split `helpers.ai.connect` makes on `core`)."""
    if core is None:
        from helpers.host.ws import WSWatcher

        async def run_wire():
            watcher = WSWatcher(root.k.port, aid)
            try:
                await watcher.connect()
                async for ev in watcher.events():
                    sink.append((time.monotonic(), ev))
            finally:
                await watcher.close()

        return asyncio.create_task(run_wire())
    inbox = f"{aid}_contract_spy"
    root.ctx.ensure_inbox(inbox)
    root.watch(aid, inbox)

    async def run():
        q = root.ctx.inboxes[inbox]
        while True:
            sink.append((time.monotonic(), await q.get()))

    return asyncio.create_task(run())


def _tokens(events: list) -> list:
    return [(t, e) for t, e in events if e.get("type") == "token"]


def _done_reasons(events: list) -> list:
    return [(e.get("detail") or {}).get("reason") for _, e in events
            if e.get("type") == "status" and e.get("phase") == "done"]


async def main():
    if blockers := BK.blockers():
        verdict(False, "; ".join(blockers))
        return
    root, core, close = await open_kernel(
        prefix="backend_contract_", reach=["work_fb", "notes", "cli", "user", *AIS]
    )
    events: dict[str, list] = {a: [] for a in AIS}
    spies: list[asyncio.Task] = []
    checks: list = []
    try:
        code = f"KITE-{secrets.token_hex(3)}"
        Path("probe_code.txt").write_text(f"The code word is {code}.\n")
        await M.files(root, AIS)

        # ── ai: stream, call, reply, answer, stripped thinking ─────────────────
        conn = await M.agent(root, core, BK, "ai")
        spies.append(_spy(root, core, "ai", events["ai"]))
        await asyncio.sleep(1.0)  # the watch subscribes before the first token
        capped = await drive(
            conn,
            f"Read `probe_code.txt` through `notes` and tell the user the code word "
            f"written in it. {DONE}",
            client_id="ai",
            via="user",
        )
        rows = await memory.rows_of(root, "ai", conversation_only=True)
        said = M.answers(rows)
        toks = _tokens(events["ai"])
        # `drive` reports a chain that simply finished as `CHAIN ENDED — nothing in
        # flight`; only a runaway / no-progress / max-turns cap or a failed turn is a
        # chain that did not run to its end.
        ended = not capped or capped.startswith("CHAIN ENDED")
        check(checks, "ai: the chain ran to its end", ended and not conn.trips,
              capped or conn.trips[-1:] or "")
        check(checks, "ai: tokens streamed on its id", len(toks) > 0, f"{len(toks)} token events")
        check(checks, "ai: a turn ended done/ok", "ok" in _done_reasons(events["ai"]),
              _done_reasons(events["ai"]))
        reads = [c["payload"].get("path") for c in ctx_score.calls_made(rows, target="notes", verb="read")]
        check(checks, "ai: its <call> to notes.read ran", "probe_code.txt" in reads, reads)
        check(checks, "ai: the read reply reached it", "probe_code.txt" in M.seen(rows, "probe_"))
        check(checks, "ai: the answer carries the code word", code in said, code)
        leaked = [r.get("id") for r in rows if r.get("role") == "assistant"
                  and THINK_OPEN in str(r.get("content") or "")]
        thought = any(THINK_OPEN in str(e.get("text") or "") for _, e in toks)
        check(checks, "ai: no stored assistant row carries thinking", not leaked,
              f"leaked={leaked} (streamed thinking: {'yes' if thought else 'none seen'})")
        st = await M.status(root, "ai")
        print(f"     reported_prompt_tokens={st.get('reported_prompt_tokens')} "
              f"calibration={st.get('calibration')}  (reported only)")

        # ── ai_cut: a main turn cut by the budget is an error, never a finish ──
        await M.agent(root, core, BK, "ai_cut", record={"max_tokens": CUT_BUDGET})
        spies.append(_spy(root, core, "ai_cut", events["ai_cut"]))
        await asyncio.sleep(1.0)
        out = (await root.send([{"target": "ai_cut", "payload": {
            "type": "send", "text": "Explain in detail how rivers form.", "client_id": "ai_cut"}}]))[0]
        # THE RESULT CARRIES THE VERDICT on both sides of the wire — a raised turn
        # settles `{error, raised: true}`. (The REST face returns the result alone,
        # not the settle's `status`.)
        res = out.get("result")
        err = str(res)
        check(checks, "ai_cut: the cut turn FAILED", isinstance(res, dict) and res.get("raised") is True,
              f"status={out.get('status')} result={err[:80]}")
        check(checks, "ai_cut: the error names the cut", "max_tokens" in err or "max_output_tokens" in err,
              err[:160])
        # The status event rides the agent's event stream, the reply rides the call:
        # over the wire they are two connections, so the event may land after the
        # reply (measured 2026-10-07 at 6 lanes: the check read an empty list).
        # Bounded wait for it — never a pass by timeout.
        for _ in range(50):
            if _done_reasons(events["ai_cut"]):
                break
            await asyncio.sleep(0.1)
        check(checks, "ai_cut: the turn closed done/error", "error" in _done_reasons(events["ai_cut"]),
              _done_reasons(events["ai_cut"]))
        cut_rows = await memory.rows_of(root, "ai_cut", conversation_only=True)
        check(checks, "ai_cut: no assistant row stored",
              not [r for r in cut_rows if r.get("role") == "assistant"],
              f"{len(cut_rows)} rows")

        # ── ai_cancel: ps → cancel mid-stream ─────────────────────────────────
        await M.agent(root, core, BK, "ai_cancel")
        spies.append(_spy(root, core, "ai_cancel", events["ai_cancel"]))
        await asyncio.sleep(1.0)
        pending = asyncio.create_task(root.send([{"target": "ai_cancel", "payload": {
            "type": "send", "client_id": "ai_cancel",
            "text": "Write a long, detailed 3000-word essay on the history of rivers. Call no tools."}}]))
        end = time.monotonic() + 240
        while time.monotonic() < end and not _tokens(events["ai_cancel"]):
            await asyncio.sleep(0.5)
        streaming = bool(_tokens(events["ai_cancel"]))
        check(checks, "ai_cancel: streaming before the cancel", streaming)
        live = [t for t in await _live_tasks(root, "ai_cancel") if t.get("verb") == "send"]
        for t in live:
            await root.send([{"target": "kernel", "payload": {"type": "cancel", "task_id": t["id"]}}])
        t_cancel = time.monotonic()
        try:
            item = (await asyncio.wait_for(pending, 60))[0]
        except TimeoutError:
            item = {"status": "still in flight 60s after cancel"}
        cres = item.get("result")
        check(checks, "ai_cancel: the call settled cancelled",
              isinstance(cres, dict) and cres.get("cancelled") is True,
              f"status={item.get('status')} result={str(cres)[:80]}")
        await asyncio.sleep(3)
        late = [1 for t, e in _tokens(events["ai_cancel"]) if t > t_cancel + 0.5]
        check(checks, "ai_cancel: the token stream stopped", streaming and not late,
              f"{len(late)} tokens after cancel")
        check(checks, "ai_cancel: the turn closed done/cancelled",
              "cancelled" in _done_reasons(events["ai_cancel"]), _done_reasons(events["ai_cancel"]))
        check(checks, "ai_cancel: nothing left in flight", not await _live_tasks(root, "ai_cancel"))

        ok = all(v for _, v in checks)
        verdict(ok, f"{sum(v for _, v in checks)}/{len(checks)} contract checks on "
                    f"{BK.name}" + ("" if ok else " — see XX lines"))
    finally:
        for s in spies:
            s.cancel()
        await close()


if __name__ == "__main__":
    asyncio.run(main())
