"""ai_paths — the AI paths that need a SCRIPTED model, one case at a time.

§9.1 marks this row `source` in both rounds: inquiry, stateless `messages`,
`send_to`, a reply that arrives late, a cancel mid-stream, a send timeout, a
cycle, a depth-9 chain, and every tag argument (`<timeout>`, `<exec_id>`,
`<retries>`, `<to>`, `<type>`). None of it was driven, because the round-1/2
stub answered one script per boot.

Here the stub keys on a `CASE=<name>#<n>` marker the harness plants in the send
text (`SCR/parity_capture_store.py:142-178`), so twenty independent scripts run
in ONE boot and none of them inherits another's turn counter. Each case records
its reply, the rows it left, and the events its WS watcher saw.
"""

from __future__ import annotations

import threading
import time

from families import BASE, MEMORY, OPEN, VLLM, ai, call, quiesce, save_kernel_log, vllm
from spec import AI_FILL_CHARS, AI_RESERVE, AI_WINDOW
from stubs.openai_chat import Handler as OpenAIHandler, OpenAIChatStub

NAME = "ai_paths"

DONE = '<call name="done"></call>'

#: The filler ONE scripted reply carries, so that this family's single
#: compaction is FORCED rather than stumbled into. See `spec.AI_WINDOW` for the
#: measurement and the owner ruling. Deterministic by construction: a fixed
#: sentence repeated to a fixed length, with no id, no timestamp, no path and no
#: `<call` in it, so the text is byte-identical on every runtime and every run.
FILL = ("The pump log is unremarkable and the valve is closed. " * 2000)[:AI_FILL_CHARS]


def tag(target: str, verb: str, **args) -> str:
    inner = f"<target_id>{target}</target_id><type>{verb}</type>"
    for k, v in args.items():
        if isinstance(v, list):
            inner += f"<{k}>" + "".join(f"<item>{x}</item>" for x in v) + f"</{k}>"
        else:
            inner += f"<{k}>{v}</{k}>"
    return f'<call name="send">{inner}</call>'


#: Every case: the script the stub answers with, in order. A dict entry asks the
#: stub for something other than a clean 200 (a stall, a cut stream, a status).
CASES = {
    # B1.3 — three turns with no call in them at all.
    "call_less_turns": ["Thinking about it.", "Still thinking.", "Done thinking."],
    # B1.6 / B3.2 — an inquiry whose options and action are not strings.
    "inquiry_shapes": [
        '<call name="inquiry"><question>pick</question><options><item>true</item>'
        '<item>b</item></options><action><a>1</a><b>—</b></action></call>',
        DONE,
    ],
    # An empty answer — the stub returns nothing at all (a dead turn).
    "empty_text": ["", DONE],
    # B1.10 — a stateless `messages` send; the rows must not grow.
    "stateless_messages": [tag("kernel", "messages", ids=[1]), DONE],
    # B1.11 — `send_to` with a number and with blanks.
    "send_to_number": ['<call name="send"><to><item>5</item></to><type>reflect</type></call>', DONE],
    "send_to_blank": ['<call name="send"><to><item>  </item></to><type>reflect</type></call>', DONE],
    # B1.20 — one AI calling another.
    "ai_to_ai": [tag("ai2", "send", text="CASE=noop#1 hello from ai"), DONE],
    # B1.21 — a reply whose text is a bare number.
    "text_number": ["586", DONE],
    # B1.26 / B2.4 — a probe verb that raises.
    "probe_raise": [tag("plant", "valve"), DONE],
    # B3.10 — a cycle, and a chain deep enough to hit the depth guard.
    #
    # THE CYCLE IS BOUNDED: ai → ai2 → ai → done. It used to close on itself
    # (`cycle_back` re-planted `CASE=cycle#1`), and because the stub keys on the
    # marker in the newest message that restarted the case at step 0 on every
    # hop: 73 model turns, ~200 rows, and the two chains interleaving differently
    # every run. Measured on two python captures of the same immutable tree:
    # every one of those rows was a diff line, because each row's text cites the
    # id of another row and every id after the first crossing shifts.
    #
    # Nothing measured is lost by bounding it. Both guards B3.10 names — `cycle
    # detected` and `max call depth 8 reached` — were absent from the runaway
    # capture too (measured: zero occurrences anywhere under `a/ai_paths/`),
    # because each hop is a fresh execution and the depth never accumulates.
    # `depth9` is what drives the depth guard; what the cycle case records either
    # way is the rows one AI leaves when another AI calls it back.
    "cycle": [tag("ai2", "send", text="CASE=cycle_back#1 back to you"), DONE],
    "cycle_back": [tag("ai", "send", text="CASE=cycle_end#1 and back again"), DONE],
    "cycle_end": [DONE],
    "depth9": [tag("readings", "reflect")] * 9 + [DONE],
    # C30 — tag timeouts and payload-less calls.
    "tag_timeout_word": [tag("readings", "reflect", timeout="soon"), DONE],
    "tag_empty_id": ['<call name="send"><target_id>readings</target_id><type>reflect</type>'
                     "<id></id></call>", DONE],
    "tag_payloadless": ['<call name="send"><target_id>readings</target_id></call>', DONE],
    # C31 — a real timeout on a slow verb, then the SAME exec id with a smaller one.
    "tag_timeout_short": [tag("slow", "sleep", seconds=2, timeout="0.2"), DONE],
    "tag_exec_id_reuse": [
        tag("slow", "sleep", seconds=2, exec_id="reuse-1", timeout="5"),
        tag("slow", "sleep", seconds=2, exec_id="reuse-1", timeout="0.2"),
        DONE,
    ],
    # C38 — an exec id that is a number, and retries that are not.
    "tag_exec_id_int": [tag("readings", "reflect", exec_id=7), DONE],
    "tag_retries_word": [tag("readings", "reflect", retries="x"), DONE],
    # B5.12 / B5.13 — `<to>` with a number, and with mixed item types.
    "to_number": ['<call name="send"><to>42</to><type>reflect</type></call>', DONE],
    "to_mixed_items": ['<call name="send"><to><item>true</item><item>readings</item></to>'
                       "<type>reflect</type></call>", DONE],
    # B5.19 — a `<type>` that is a number.
    "type_number": ['<call name="send"><target_id>readings</target_id><type>42</type></call>', DONE],
    # B5.17 — a reply whose text carries CRLF.
    "crlf_reply": ["line one\r\nline two\r\n" + DONE],
    # B2.3 — a stall long enough for a `kernel cancel` to land mid-stream.
    "cancel_midstream": [{"text": DONE, "stall": 8}],
    # B1.17 — a slow local verb, then a `boot` of the AI while it is in flight.
    "boot_midflight": [tag("slow", "sleep", seconds=3), DONE],
    "noop": [DONE],
    # C-context — THE family's one compaction, forced. Turn 1 answers with a
    # call-less block of `AI_FILL_CHARS`; the nudge that follows carries it into
    # the history, and THAT turn is the one that crosses the budget — by ~8000
    # tokens, not by one character. Turn 2 closes the case.
    "context_fill": [FILL, DONE],
}


def run(ctx) -> None:  # noqa: C901
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "a.txt").write_text("7\n")

    stub = OpenAIChatStub(rec, "stub", ports.take("stub"))
    stub.set_cases(CASES)
    stub.start(OpenAIHandler)

    cb = {**BASE, "rest": "*", "web": "*", "ws": "*", "ai": "*", "ai2": "*"}
    reach = ["readings", "slow", "plant", "ai", "ai2", "web", "rest", "ws"]
    k = ctx.boot(reach=reach, port=ports.take("main"))
    try:
        rec.json("setup/readings", k.create(parent_id=k.root_id, id="readings",
                                            handler_module="file_bridge.tools", allow=["."], root=".",
                                            **OPEN, callable_by=cb))
        rec.json("setup/slow", k.create(parent_id=k.root_id, id="slow",
                                        handler_module="slow_probe.tools", **OPEN,
                                        callable_by=cb))
        rec.json("setup/plant", k.create(parent_id=k.root_id, id="plant",
                                         handler_module="plant_probe.tools", **OPEN,
                                         callable_by=cb))
        ai_meta = vllm(stub.base_url(), context_window=AI_WINDOW,
                       output_reserve=AI_RESERVE, context_async=True)
        rec.json("setup/ai", k.create(parent_id=k.root_id, id="ai",
                                      handler_module=VLLM, **OPEN,
                                      callable_by=cb,
                                      **ai(can_call={**BASE, "readings": "*", "slow": "*",
                                                     "plant": "*", "kernel": "*", "ai2": "*"},
                                           **ai_meta)))
        rec.json("setup/ai2", k.create(parent_id=k.root_id, id="ai2",
                                       handler_module=VLLM, **OPEN,
                                       callable_by=cb,
                                       **ai(can_call={**BASE, "readings": "*", "kernel": "*",
                                                      "ai": "*"},
                                            **ai_meta)))
        time.sleep(1.2)

        from helpers.host.ws import WSWatcher
        import asyncio

        events: dict = {}
        loop = asyncio.new_event_loop()
        threading.Thread(target=loop.run_forever, daemon=True).start()

        # ONE SOCKET PER CASE, not one per run.
        #
        # The first version opened a watcher on `ai` and on `ai2` before the
        # first case and read both until the end. Measured on two python
        # captures: both runs stopped receiving after the ninth case, and they
        # stopped at a DIFFERENT frame (221 events against 233) — so fifteen
        # cases recorded an empty event list on both runs, and the one case at
        # the boundary recorded all of its frames on one run and none on the
        # other. A stream that dies silently mid-run measures nothing after it
        # dies and is not reproducible at the point where it does.
        #
        # A fresh socket per case bounds that: each case's events are its own,
        # opened before its send and closed after its quiesce, and a leg that
        # gives up during one case cannot empty the next fifteen.
        def open_watch(agent: str, sink: list):
            w = WSWatcher(k.port, agent)

            async def connect() -> None:
                await w.connect()

            asyncio.run_coroutine_threadsafe(connect(), loop).result(timeout=15)

            async def pump() -> None:
                try:
                    async for ev in w.events():
                        sink.append(ev)
                        events.setdefault(agent, []).append(ev)
                except Exception as e:  # noqa: BLE001 - a refused watch is data
                    sink.append({"__watch_error__": repr(e)})
            asyncio.run_coroutine_threadsafe(pump(), loop)
            return w

        def close_watch(w) -> None:
            try:
                asyncio.run_coroutine_threadsafe(w.close(), loop).result(timeout=5)
            except Exception:  # noqa: BLE001
                pass

        def run_case(case: str, *, agent: str = "ai", timeout: float = 120,
                     during=None) -> None:
            before_req = len(stub.requests)
            holder: dict = {}
            peer = "ai2" if agent != "ai2" else "ai"
            mine: list = []
            theirs: list = []
            w_mine = open_watch(agent, mine)
            w_peer = open_watch(peer, theirs)
            time.sleep(0.5)

            def go() -> None:
                holder["reply"] = call(k, agent, {
                    "type": "send", "text": f"CASE={case}#1 drive {case}.",
                    "client_id": "probe"}, timeout=timeout)

            th = threading.Thread(target=go, daemon=True)
            th.start()
            time.sleep(0.8)
            mid = during(case) if during else None
            q = quiesce(stub, k, agent=agent, rec=rec, label=case, max_s=90)
            th.join(timeout=10)
            time.sleep(0.5)
            close_watch(w_mine)
            close_watch(w_peer)
            rec.json(f"cases/{case}", {
                "case": case, "agent": agent,
                "reply": holder.get("reply", {"__harness_error__": "send did not return"}),
                "mid_flight": mid,
                "rows_read": call(k, MEMORY, {"type": "read", "owner": agent}),
                "events": list(mine), "events_peer": {"agent": peer, "events": list(theirs)}},
                harness={"quiesce": q,
                         "request_numbers": [r["n"] for r in stub.requests[before_req:]]})

        for case in ("call_less_turns", "inquiry_shapes", "empty_text", "stateless_messages",
                     "send_to_number", "send_to_blank", "text_number", "probe_raise",
                     "tag_timeout_word", "tag_empty_id", "tag_payloadless",
                     "tag_timeout_short", "tag_exec_id_reuse", "tag_exec_id_int",
                     "tag_retries_word", "to_number", "to_mixed_items", "type_number",
                     "crlf_reply", "depth9", "ai_to_ai", "cycle", "context_fill"):
            run_case(case)

        # B3.2 — answering an inquiry with three shapes that are not text.
        for answer, tag_name in (("...", "ellipsis"), (42, "int"), (None, "null")):
            rec.json(f"inquiry_answers/{tag_name}",
                     call(k, "ai", {"type": "answer", "text": answer, "client_id": "probe"}))

        # B2.3 — cancel in the middle of a stalling stream.
        def cancel_mid(case: str):
            ps = call(k, "kernel", {"type": "ps"})
            tasks = [t for t in (ps.get("tasks") or []) if isinstance(t, dict)
                     and t.get("agent") == "ai"]
            out = {"ps": ps, "cancels": []}
            for t in tasks:
                tid = t.get("task_id") or t.get("id") or t.get("pid")
                out["cancels"].append({"task_id": tid,
                                       "reply": call(k, "kernel", {"type": "cancel",
                                                                   "task_id": tid})})
            return out

        run_case("cancel_midstream", timeout=60, during=cancel_mid)

        # B1.17 — `boot` the AI while a local verb of its own is still running.
        def boot_mid(case: str):
            time.sleep(1.0)
            return {"boot": call(k, "ai", {"type": "boot"})}

        run_case("boot_midflight", timeout=90, during=boot_mid)

        rec.json("final/kernel_ps", call(k, "kernel", {"type": "ps"}))
        rec.json("final/rows_read_ai", call(k, MEMORY, {"type": "read", "owner": "ai"}))
        rec.json("final/rows_read_ai2", call(k, MEMORY, {"type": "read", "owner": "ai2"}))
        rec.json("events", events)
        loop.call_soon_threadsafe(loop.stop)
        http_1 = list(k.http)
        k.close()
        save_kernel_log(rec, k, "kernel_log_1.txt")

        # ── B2.8/B2.18 — A SECOND BOOT, because the send timeout is read at
        # IMPORT time (`PB/ai/ai_core/src/ai_core/core.py:143-145`) and cannot be
        # changed in a running kernel. 0.5 s against a 3 s stall: the send must
        # give up before the model does, and what it says then is the item.
        stub.set_cases({**CASES, "send_timeout": [{"text": DONE, "stall": 3}]})
        k2 = ctx.boot(reach=[*reach, "ai_to"], port=k.port,
                      env_extra={"FANTASTIC_AI_SEND_TIMEOUT": "0.5"})
        try:
            rec.json("send_timeout/create_ai", k2.create(
                parent_id=k2.root_id, id="ai_to", handler_module=VLLM,
                **OPEN, callable_by=cb, **ai(can_call={**BASE, "readings": "*"}, **ai_meta)))
            time.sleep(1.0)
            t0 = time.monotonic()
            reply = call(k2, "ai_to", {"type": "send", "text": "CASE=send_timeout#1 stall me",
                                       "client_id": "probe"}, timeout=60)
            rec.json("send_timeout/send_reply", {"reply": reply},
                     harness={"waited_s": round(time.monotonic() - t0, 2)})
            time.sleep(4.0)
            rec.json("send_timeout/rows_read",
                     call(k2, MEMORY, {"type": "read", "owner": "ai_to"}))
            rec.json("send_timeout/status", call(k2, "ai_to", {"type": "status",
                                                               "client_id": "probe"}))
        finally:
            rec.http("http", http_1 + k2.http)
            k2.close()
            save_kernel_log(rec, k2, "kernel_log_2.txt")
    finally:
        stub.flush()
        rec.json("stub/index", stub.requests)
        # The cap is DATA: which case ran away, and how far it got before the
        # harness ended the chain (see `OpenAIChatStub.case_cap`).
        rec.json("stub/case_turns", {"cap": stub.case_cap, "turns": stub.case_turns,
                                     "cap_hits": stub.cap_hits})
        stub.stop()
