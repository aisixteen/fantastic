"""ctx — the long chain: four context strategies side by side, the too_small
failsafe, and a reboot that hydrates (`SCR/parity_capture_cli_ctx.py` ctx part).

Four AI agents with the SAME window, reserve and script and different
`context_strategy` — compact, truncate, strip, fold — so the diff shows what each
strategy DID to the same conversation rather than what four different
conversations happened to look like. `ai_tiny` gets a window smaller than one
turn, which trips the too_small failsafe on its first pass.

Two choices, both so the family can measure what it is for:
  * the data bridge is `bigfiles` rooted at `big`, so the name `store` is free
    for a PERSISTENCE provider — without one, the boot-time `context_recall` the
    reboot measures never runs (`[messages] no store wired`);
  * requests are saved as `req/<agent>_<kind>_t<turn>.json` with the ordinal kept
    in the index, so the diff aligns by (agent, kind, turn) instead of by a
    global counter that shifts the moment one runtime makes one extra call.
"""

from __future__ import annotations

import asyncio
import json
import re
import threading
import time

from families import BASE, MEMORY, OPEN, ROOT_ID, VLLM, ai, call, save_kernel_log, vllm
from spec import CTX_RESERVE, CTX_WINDOW, CTX_WINDOW_TINY
from stubs.openai_chat import Handler as OpenAIHandler, OpenAIChatStub, content_text

NAME = "ctx"

AIS = ["ai_compact", "ai_truncate", "ai_strip", "ai_fold"]
GOAL = ("Survey every file through docs, page anything large, keep notes, and finish "
        "with done. GOAL-MARKER-7731.")
PID_RE = re.compile(r"\(settled, \d+ bytes — NOT inlined.*?<pid>(\d+)</pid>", re.S)
YOU_ARE = re.compile(r"You are `([^`]+)`")


def filler(n: int) -> str:
    return f"Note {n}: " + " ".join(
        f"observation-{n}-{i:03d} the survey continues with steady readings;" for i in range(36))


def tag(target: str, verb: str, title=None, **args) -> str:
    inner = f"<target_id>{target}</target_id><type>{verb}</type>"
    for k, v in args.items():
        if isinstance(v, list):
            inner += f"<{k}>" + "".join(f"<item>{x}</item>" for x in v) + f"</{k}>"
        else:
            inner += f"<{k}>{v}</{k}>"
    if title:
        inner += f"<title>{title}</title>"
    return f'<call name="send">{inner}</call>'


def main_turn(n: int, me: str, pid: str) -> str:
    F = filler(n) + "\n"
    script = {
        0: "Starting the survey.\n" + tag("docs", "reflect", "look at docs"),
        1: "SEEN docs — text file agent over the store; verbs: read, write.\n" + F
           + tag("docs", "read", "read big a", path="a.txt"),
        2: F + tag(me, "pull", "page a", pid=pid, offset=0),
        3: F + tag("docs", "read", "read b", path="b.txt") + "\n"
           + tag("docs", "read", "read c", path="c.txt"),
        4: F + tag(me, "grep", "grep a", query="line 0007", ids=[pid]),
        5: F + tag("docs", "read", "read d", path="d.txt"),
        6: F + tag("docs", "write", "persist fact", path="memo.txt",
                   bytes="fact: the survey total is 42"),
        7: F + tag("docs", "read", "read e", path="e.txt"),
        8: F + tag(me, "recall", "recall note 3", query="observation-3-001", limit=2) + "\n"
           + tag(me, "context_status", "check context"),
        9: F + tag("docs", "read", "read f", path="f.txt"),
        10: F + tag("docs", "read", "read g", path="g.txt"),
        11: F + tag("docs", "read", "read h", path="h.txt"),
        12: F + tag("docs", "read", "reread b", path="b.txt", offset=10, limit=50),
        13: F + tag("docs", "read", "reread c", path="c.txt", offset=10, limit=50),
        14: F + tag("docs", "read", "reread d", path="d.txt", offset=10, limit=50),
        15: F + tag("docs", "read", "read tail a", path="a.txt", offset=300, limit=40),
        16: F + tag(me, "context_recall", "rebuild view"),
        17: F + tag("docs", "read", "reread e", path="e.txt", offset=5, limit=50),
        18: "Survey complete.\n" + '<call name="done"></call>',
    }
    return script.get(n, '<call name="done"></call>')


def run(ctx) -> None:  # noqa: C901
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "big").mkdir()
    (ctx.work / "big" / "a.txt").write_text(
        "".join(f"line {i:04d} alpha reading value {i * 7:05d} steady\n" for i in range(400)))
    for j, name in enumerate("bcdefgh"):
        (ctx.work / "big" / f"{name}.txt").write_text(
            "".join(f"{name}-row {i:03d} reading {i * (j + 3):05d} nominal\n" for i in range(60)))

    state = {"current": None, "turns": {}, "reacted": set(), "folds": {}, "last": time.time(),
             "k": None}
    index: list = []
    lock = threading.Lock()
    stub = OpenAIChatStub(rec, "stub", ports.take("stub"))

    def classify(body: dict, raw: bytes):
        """WHICH prompt this is, from what the KERNEL put in it.

        There is no marker the harness controls here — the recall prompt, the fold
        prompt and the summarizer prompt are the kernel's own text, so the stub
        recognises them by it. That is deliberate: if a port words one of them
        differently the classification changes, and the change lands in the index
        as data instead of being papered over by a harness-planted tag.
        """
        # The system block is message 0 on this wire (vllm_backend merges every
        # system message into one there); `msgs` are the turns after it.
        every = body.get("messages") or []
        sysv = "\n\n".join(content_text(m) for m in every
                            if isinstance(m, dict) and m.get("role") == "system")
        msgs = [m for m in every if not (isinstance(m, dict) and m.get("role") == "system")]
        contents = [content_text(m) for m in msgs]
        allc = "\n".join(contents)
        m = YOU_ARE.search(sysv)
        cur = state["current"]
        if any(c.startswith("You are rebuilding your own working context") for c in contents):
            return "context_recall", cur, sysv, msgs, allc
        if any("FOLD this finished piece of work" in c for c in [sysv, *contents]):
            return "fold_summary", cur, sysv, msgs, allc
        if "Summarize the conversation excerpt" in sysv or any(
                c.startswith("Summarize the conversation excerpt") for c in contents):
            return "summary", cur, sysv, msgs, allc
        if m:
            return "main", m.group(1), sysv, msgs, allc
        return "unclassified", cur, sysv, msgs, allc

    def responder(body: dict, raw: bytes):
        kind, agent, sysv, msgs, allc = classify(body, raw)
        first = content_text(msgs[0]) if msgs else ""
        with lock:
            n = len(stub.requests) + 1
            turn = None
            if kind == "main":
                turn = state["turns"].get(agent, 0)
                state["turns"][agent] = turn + 1
        snap = None
        if kind == "main" and state["k"] is not None and agent:
            snap = call(state["k"], agent, {"type": "context_status"}, timeout=8)
        if kind == "main":
            pids = PID_RE.findall(allc)
            pid = pids[-1] if pids else "0"
            text = main_turn(turn, agent or "?", pid)
            if "[context-notice]" in allc and agent not in state["reacted"]:
                state["reacted"].add(agent)
                text = tag(agent, "recall", "recall after notice", query="Note", limit=3) + "\n" + text
        elif kind == "summary":
            text = "SUMMARY: the survey read several files through docs and paged one large body."
        elif kind == "fold_summary":
            k2 = state["folds"].get(agent, 0) + 1
            state["folds"][agent] = k2
            text = (f"Fold {k2}: survey chunk closed\n\nThe chain read files through docs and "
                    "kept notes. Nothing binds.")
        elif kind == "unclassified":
            text = "UNCLASSIFIED request: stub has no script for it."
        else:
            ids = re.findall(r"^(\d+) · ", first, re.M)
            pick = [int(ids[-1])] if ids else []
            text = json.dumps({"expand": pick, "why": "stub recall keeps the newest row in full"})
        name = f"{agent}_{kind}" + (f"_t{turn:02d}" if turn is not None else "")
        rec.raw(f"req/{name}.json", raw)
        rec.text(f"req/{name}.resp.txt", text)
        if snap is not None:
            rec.json(f"req/{name}.context_status", snap)
        index.append({"n": n, "agent": agent, "kind": kind, "turn": turn, "phase": state["current"],
                      "name": name, "bytes": len(raw), "system_chars": len(sysv),
                      "messages": len(msgs), "has_context_notice": "[context-notice]" in allc,
                      "has_stripped_stub": "(stripped:" in allc,
                      "has_settled_stub": "(settled, " in allc, "has_fold_row": "[fold " in allc,
                      "has_recall_row": "[recall " in allc,
                      "has_transient_dropped":
                          "full text dropped; your distillation follows" in allc,
                      "keys": sorted(body)})
        state["last"] = time.time()
        return text

    stub.responder = responder
    stub.start(OpenAIHandler)

    def quiesce_stub(max_s=240, idle=6):
        t0 = time.time()
        while time.time() - t0 < max_s:
            time.sleep(1)
            if time.time() - state["last"] > idle:
                return round(time.time() - t0, 1)
        return None

    cb = {**BASE, "rest": "*", "web": "*", "ws": "*"}
    ids = ["store", "bigfiles", "docs", *AIS, "ai_tiny"]
    k = ctx.boot(reach=ids, port=ports.take("main"))
    watchers: dict = {a: [] for a in [*AIS, "ai_tiny"]}
    ws_errors: dict = {}
    loop = asyncio.new_event_loop()
    live_ws: dict = {}
    ws_futs: list = []

    async def watch(agent: str, phase: str) -> None:
        from helpers.host.ws import WSWatcher

        w = WSWatcher(k.port, agent)
        live_ws[agent] = w
        try:
            await w.connect()
            watchers[agent].append({"harness": {"t": round(time.time(), 3)},
                                    "marker": f"connected ({phase})"})
            async for ev in w.events():
                watchers[agent].append({"harness": {"t": round(time.time(), 3)}, "payload": ev})
            watchers[agent].append({"harness": {"t": round(time.time(), 3)},
                                    "marker": f"stream ended ({phase})"})
        except Exception as e:  # noqa: BLE001 - a refused watch is data
            ws_errors.setdefault(agent, []).append(repr(e))

    def ws_open(agent: str, phase: str) -> None:
        ws_futs.append(asyncio.run_coroutine_threadsafe(watch(agent, phase), loop))
        time.sleep(0.8)

    def ws_close(agent: str) -> None:
        w = live_ws.pop(agent, None)
        if w is not None:
            watchers[agent].append({"harness": {"t": round(time.time(), 3)},
                                    "marker": "harness closing"})
            try:
                asyncio.run_coroutine_threadsafe(w.close(), loop).result(timeout=5)
            except Exception as e:  # noqa: BLE001
                ws_errors.setdefault(agent, []).append("close: " + repr(e))

    try:
        # A PERSISTENCE PROVIDER FIRST. The reboot below is the only way to
        # measure a boot-time `context_recall`, and it needs rows on disk.
        rec.json("create_store", k.create(
            parent_id=k.home, handler_module="file_bridge.tools", allow=["."], id="store", **OPEN,
            root=".fantastic",
            callable_by={**BASE, "rest": "*", "web": "*", "ws": "*"}))
        roster = [
            ("bigfiles", "file_bridge.tools", {"allow": ["."], "root": "big", "callable_by": cb}),
            ("docs", "file_bridge.tools", {"allow": ["."], "root": "big",
                                           "callable_by": {**cb, **{a: "*" for a in [*AIS, "ai_tiny"]}}}),
        ]
        for a in AIS:
            roster.append((a, VLLM, ai(**{
                **vllm(stub.base_url()), "context_window": CTX_WINDOW,
                "output_reserve": CTX_RESERVE, "context_strategy": a.split("_", 1)[1],
                "context_async": True, "context_titles": True, "recent_n": 4,
                "callable_by": cb, "can_call": {**BASE, "docs": "*", ROOT_ID: "*"}})))
        roster.append(("ai_tiny", VLLM, ai(**{
            **vllm(stub.base_url()), "context_window": CTX_WINDOW_TINY,
            "output_reserve": CTX_RESERVE, "context_strategy": "compact", "context_async": True,
            "context_titles": True, "recent_n": 4, "callable_by": cb,
            "can_call": {**BASE, "docs": "*", ROOT_ID: "*"}})))
        meta_all = {}
        for aid, hm, meta in roster:
            m = {"parent_id": k.home, "handler_module": hm, "id": aid, **OPEN, **meta}
            meta_all[aid] = m
            rec.json(f"create_{aid}", k.create(**m))
        rec.json("roster_meta", meta_all)
        time.sleep(1.5)
        state["k"] = k
        threading.Thread(target=loop.run_forever, daemon=True).start()
        for a in [*AIS, "ai_tiny"]:
            rec.json(f"{a}/reflect", call(k, a, {"type": "reflect"}))
            rec.json(f"{a}/context_status_0_boot", call(k, a, {"type": "context_status"}))

        for a in AIS:
            state["current"] = a
            ws_open(a, "chain")
            state["last"] = time.time()
            rec.json(f"{a}/send_goal_reply",
                     call(k, a, {"type": "send", "text": GOAL, "client_id": "probe"}, timeout=240))
            q = quiesce_stub()
            rec.json(f"{a}/quiesce", {"main_turns": state["turns"].get(a, 0),
                                      "folds": state["folds"].get(a, 0)},
                     harness={"idle_after_s": q})
            rec.json(f"{a}/context_status_1_after_chain", call(k, a, {"type": "context_status"}))
            rec.json(f"{a}/rows_read_1_after_chain",
                     call(k, MEMORY, {"type": "read", "owner": a}))
            rec.json(f"{a}/history_1", call(k, a, {"type": "history", "client_id": "probe"}))
            rec.json(f"{a}/status_1", call(k, a, {"type": "status", "client_id": "probe"}))
            rec.json(f"{a}/recall_goal",
                     call(k, a, {"type": "recall", "query": "GOAL-MARKER", "client_id": "probe"}))
            rec.json(f"{a}/grep_marker",
                     call(k, a, {"type": "grep", "query": "line 0007", "client_id": "probe"}))
            rec.json(f"{a}/context_recall_dry", call(k, a, {"type": "context_recall", "dry": True}))
            rec.json(f"{a}/context_status_2_after_dry_recall",
                     call(k, a, {"type": "context_status"}))
            state["last"] = time.time()
            rec.json(f"{a}/send_too_small_reply",
                     call(k, a, {"type": "send", "text": "HUGE " + ("x" * 60000),
                                 "client_id": "probe"}, timeout=120))
            quiesce_stub(max_s=30, idle=3)
            rec.json(f"{a}/context_status_3_after_too_small", call(k, a, {"type": "context_status"}))
            rec.json(f"{a}/rows_read_3_after_too_small",
                     call(k, MEMORY, {"type": "read", "owner": a}))
            # B2.2 / B3.9 / B5.5 — a strategy name nothing implements, and a null one.
            rec.json(f"{a}/update_strategy_bogus",
                     call(k, k.root_id, {"type": "update_agent", "id": a,
                                         "context_strategy": "bogus"}))
            rec.json(f"{a}/context_status_4_after_bogus", call(k, a, {"type": "context_status"}))
            time.sleep(0.5)
            ws_close(a)

        a = "ai_tiny"
        state["current"] = a
        ws_open(a, "too_small")
        state["last"] = time.time()
        rec.json(f"{a}/send_reply",
                 call(k, a, {"type": "send", "text": GOAL, "client_id": "probe"}, timeout=120))
        quiesce_stub(max_s=30, idle=3)
        rec.json(f"{a}/context_status_1", call(k, a, {"type": "context_status"}))
        rec.json(f"{a}/rows_read_1", call(k, MEMORY, {"type": "read", "owner": a}))
        time.sleep(0.5)
        ws_close(a)
        rec.json("kernel_ps_end", call(k, "kernel", {"type": "ps"}))
        time.sleep(1.0)
        rec.json("ws_events", {"events": watchers, "errors": ws_errors})

        # ── reboot: an agent with a store runs context_recall at boot ──
        state["current"] = "reboot"
        n_before = len(index)
        http_before = list(k.http)
        k.close()
        save_kernel_log(rec, k, "kernel_log_1.txt")
        k2 = ctx.boot(reach=ids, port=k.port)
        state["k"] = None
        time.sleep(3)
        quiesce_stub(max_s=60, idle=4)
        rec.json("reboot/stub_requests_during_boot", index[n_before:])
        for a in [*AIS, "ai_tiny"]:
            rec.json(f"reboot/{a}_context_status", call(k2, a, {"type": "context_status"}))
            rec.json(f"reboot/{a}_rows_read", call(k2, MEMORY, {"type": "read", "owner": a}))
        rec.json("reboot/kernel_list_agents", call(k2, "kernel", {"type": "list_agents"}))
        rec.http("http", http_before + k2.http)
        k = k2
    finally:
        try:
            loop.call_soon_threadsafe(loop.stop)
        except Exception:  # noqa: BLE001
            pass
        rec.json("requests_index", index)
        stub.flush()
        k.close()
        stub.stop()
        save_kernel_log(rec, k, "kernel_log_final.txt")
        from families import disk_walk

        rec.json("fantastic_after", disk_walk(ctx.work / ".fantastic"))
