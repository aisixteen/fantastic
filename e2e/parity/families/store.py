"""store — a WIRED store: persisted bytes, reflect flags and paging, the messages
digest, the AI self verbs, and the tool-parse faults
(`SCR/parity_capture_store.py` s1–s5, plus the msgprobe/msgverify edge rows and
the tpf_py_probe table).

The store is a `file_bridge` rooted at `.fantastic` — the persistence provider
the root discovers — so everything an agent writes lands on disk and the capture
can read the SAME bytes two ways: the harness's own `os.walk` (ground truth,
taken first) and the store's own `list`/`read_stream` (what a model would see).
A verb that disagrees with the disk beside it is the whole point of taking both.

`ledger.json` is REWRITTEN WHILE IT IS READ (C47: every flush is itself an
execution whose settle re-kicks the flusher), so the read and the disk copy have
different shas by design. `rules.yaml` N12 parses it instead of comparing bytes.
"""

from __future__ import annotations

import hashlib
import json
import re
import sys
import threading
import time
from pathlib import Path

from families import (BASE, MEMORY, OPEN, ROOT_ID, VLLM, ai, call, disk_walk, page_walk, quiesce,
                      save_kernel_log, vllm)
from spec import CTX_RESERVE, CTX_WINDOW
from stubs.openai_chat import Handler as OpenAIHandler, OpenAIChatStub

NAME = "store"

DONE = '<call name="done"></call>'
BIG_LINES = 300
NEEDLE = "NEEDLE total=42 naïve — 日本語 😀"

CASES = {
    "chain": [
        '<call name="send"><target_id>totals</target_id><type>read</type><path>readings/big.txt</path><title>read the big file</title></call>\n'
        '<call name="send"><target_id>readings</target_id><type>list</type><path></path></call>',
        '<call name="send"><target_id>totals</target_id><type>write</type><path>t.txt</path><bytes>42</bytes><title>store the total</title></call>',
        DONE,
    ],
    "f01_text_outside_tags": [
        "Let me look at the readings first.\n"
        '<call name="send"><target_id>readings</target_id><type>list</type><path></path></call>\n'
        "That should show every file.",
    ],
    "f02_duplicated_child": [
        '<call name="send"><target_id>readings</target_id><type>list</type><path>a</path><path>b</path></call>',
    ],
    "f03_unknown_call_name": [
        '<call name="fetch"><target_id>readings</target_id><type>list</type><path></path></call>',
    ],
    "f04_bad_head": [
        '<call nam="send"><target_id>readings</target_id><type>list</type><path></path></call>',
    ],
    "f05_unclosed_bytes": [
        '<call name="send"><target_id>totals</target_id><type>write</type><path>u.txt</path><bytes>this document never closes</call>',
    ],
    "f06_multi_addressee_to": [
        '<call name="send"><to><item>readings</item><item>totals</item></to><type>reflect</type></call>',
    ],
    "f07_call_nested_in_think": [
        '<think>I could run <call name="send"><target_id>readings</target_id><type>list</type><path></path></call> but I will not.</think>\n'
        "Thinking is over.",
    ],
    "f08_empty_send": ['<call name="send"></call>'],
    "f09_non_string_target": [
        '<call name="send"><target_id>42</target_id><type>reflect</type></call>',
    ],
    "f10_bytes_with_xml": [
        '<call name="send"><target_id>totals</target_id><type>write</type><path>x.xml</path>'
        '<bytes><call name="done"></call>\n<note a="1">&amp; <b>bold</b></note>\n</bytes></call>',
    ],
    "f11_text_inside_call_body": [
        '<call name="send"><target_id>readings</target_id> stray words <type>list</type><path></path></call>',
    ],
    # ── §5.4 additions: the tag-argument faults measured only in `SCR/tpf_py_probe*` ──
    "f12_exec_id_tag": [
        '<call name="send"><target_id>readings</target_id><type>list</type><path></path><exec_id>7</exec_id></call>',
    ],
    "f13_timeout_word": [
        '<call name="send"><target_id>readings</target_id><type>list</type><path></path><timeout>soon</timeout></call>',
    ],
    "f14_retries_word": [
        '<call name="send"><target_id>readings</target_id><type>list</type><path></path><retries>x</retries></call>',
    ],
}
FAULT_CASES = [c for c in CASES if c.startswith("f")]
CTX_RECALL_ANSWER = '{"expand": [], "why": "stub: keep the mechanical fit"}'


def roster(stub_url: str, cb: dict, interp: str) -> list:
    return [
        ("runner", "exec_bridge.tools", dict(cwd=".", allowed=[rf"{re.escape(interp)} \S+"])),
        ("readings", "file_bridge.tools",
         dict(allow=["."], root="readings", description="Sensor readings — one integer per file.")),
        ("kernel_store", "file_bridge.tools", dict(allow=["."], root=".", callable_by=cb)),
        ("totals", "file_bridge.tools", dict(allow=["."], root=".")),
        ("sched", "scheduler.tools", {}),
        ("wsb", "ws_bridge.tools", dict(url="ws://127.0.0.1:9/nowhere")),
        ("relay", "relay_connector.tools", dict(url="ws://127.0.0.1:9/nowhere")),
        ("sload", "static_loader.tools", dict(root=".")),
        ("cli", "cli.tools", {}),
        ("olla", "ollama_backend.tools", ai(model="m", endpoint="http://127.0.0.1:9")),
        ("absent", "absent_bundle.tools", {}),
        ("dispenser", "dispenser_probe.tools", {}),
        ("plant", "plant_probe.tools", {}),
        ("purchase", "purchase_probe.tools", {}),
        ("user", "user_probe.tools", {}),
        ("oracle", "oracle_probe.tools", dict(lines=["the answer is 42"])),
        ("ai", VLLM,
         ai(**vllm(stub_url, context_window=CTX_WINDOW), sampling={"x": 1, "y": None},
              context_async=True, output_reserve=CTX_RESERVE,
              can_call={"readings": "*", "totals": "*", "runner": "*",
                        "kernel_store": "*", ROOT_ID: "*"})),
    ]


def snapshot(ctx, k, tag: str) -> None:
    """Every persisted file, twice: the harness's disk walk FIRST, then through
    the store's own verbs.

    Each read is bounded by the size the LISTING reported, in one `read_stream`
    of that length. An eof-driven loop cannot terminate here: a kernel records
    each read's reply into its own log, which grows the file while it is being
    read (measured on rust, round 2).
    """
    rec = ctx.rec
    base = ctx.work / ".fantastic"
    disk = disk_walk(base)
    rec.json(f"store_files_{tag}/disk_walk_harness", disk)
    for entry in disk:
        if "bytes_len" in entry and entry["bytes_len"] <= 16 * 1024 * 1024:
            rec.raw(f"store_files_{tag}/disk_raw/{entry['path']}",
                    (base / entry["path"]).read_bytes())

    listing, to_read = [], []

    def walk(path: str, depth: int = 0) -> None:
        r = call(k, "store", {"type": "list", "path": path})
        listing.append({"path": path, "reply": r})
        entries = r.get("files") if isinstance(r, dict) else None
        if not isinstance(entries, list) or depth > 16:
            return
        for f in entries:
            if not isinstance(f, dict):
                continue
            name = f.get("name")
            if not isinstance(name, str):
                rec.note(kind="list_entry_without_name", tag=tag, path=path, entry=f)
                continue
            p = f"{path}/{name}" if path else name
            t = f.get("type")
            if t in ("dir", "directory"):
                walk(p, depth + 1)
            elif t == "file":
                to_read.append((p, f.get("size")))
            else:
                rec.note(kind="list_entry_unknown_type", tag=tag, path=p, entry=f)

    walk("")
    rec.json(f"store_files_{tag}/listing", listing)

    index = []
    for p, listed in to_read:
        bounded = isinstance(listed, int) and not isinstance(listed, bool) and listed >= 0
        if not bounded:
            rec.note(kind="list_entry_without_size", tag=tag, path=p, size=listed)
        off, chunks, replies = 0, [], []
        for _ in range(64):
            length = max(listed - off, 1) if bounded else 65536
            k.last_http = None
            r = call(k, "store", {"type": "read_stream", "path": p, "offset": off, "length": length})
            transport = {kk: vv for kk, vv in (k.last_http or {}).items()
                         if kk in ("status", "reason", "reply_kind")}
            b = r.get("bytes") if isinstance(r, dict) else None
            # The WIRE REPRESENTATION of `bytes` is data (an octet body vs a JSON
            # string vs a JSON int list), so it is recorded as such and the file
            # reassembled from whichever came.
            if isinstance(b, (bytes, bytearray)):
                repr_kind, b = "octet_body", bytes(b)
            elif isinstance(b, str):
                repr_kind, b = "json_string", b.encode("utf-8")
            elif isinstance(b, list) and all(isinstance(x, int) and 0 <= x < 256 for x in b):
                repr_kind, b = "json_int_list", bytes(b)
            else:
                replies.append({"payload": {"offset": off, "length": length},
                                "bytes_repr": f"absent_or_other:{type(b).__name__}",
                                "reply": _scrub(r), "transport": transport})
                break
            replies.append({"payload": {"offset": off, "length": length},
                            "bytes_repr": repr_kind, "reply": _scrub(r), "transport": transport})
            chunks.append(b)
            nxt = r.get("next_offset")
            off = nxt if isinstance(nxt, int) and not isinstance(nxt, bool) else off + len(b)
            if not b or (bounded and off >= listed) or (not bounded and r.get("eof") is True):
                break
        data = b"".join(chunks)
        rec.raw(f"store_files_{tag}/raw/{p}", data)
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            text = None
        entry = {"path": p, "listed_size": listed, "bytes_len": len(data),
                 "sha256": hashlib.sha256(data).hexdigest(), "chunk_replies": replies, "utf8": text}
        rec.json(f"store_files_{tag}/body/{p}", entry)
        index.append({k2: entry[k2] for k2 in ("path", "listed_size", "bytes_len", "sha256")})
    rec.json(f"store_files_{tag}/index", index)


def _scrub(reply):
    """A read reply with the body replaced by its LENGTH — the bytes are already
    in the raw sidecar, and inlining them a second time would put megabytes into
    a file whose subject is the envelope."""
    if not isinstance(reply, dict):
        return reply
    out = {}
    for k2, v2 in reply.items():
        if isinstance(v2, (bytes, bytearray)):
            out[k2] = {"__raw_bytes_in_raw_file__": len(v2)}
        elif k2 == "bytes" and isinstance(v2, str):
            out[k2] = {"__json_string_len__": len(v2)}
        else:
            out[k2] = v2
    return out


def store_census(k):
    """`{rows, flush}` of the memory store in ONE call: every row it holds, any
    owner, and how many are the kernel's `flush` bookkeeping rows. `dump` is one
    snapshot of the whole store, so the two numbers are of the same instant —
    the same thing the store's `reflect` counts in `rows`. `None` when the dump
    did not answer with tables."""
    r = call(k, MEMORY, {"type": "dump"})
    rows = (r.get("tables") or {}).get("rows") if isinstance(r, dict) else None
    if not isinstance(rows, list):
        return None, r
    flush = sum(1 for e in rows if isinstance(e, dict) and isinstance(e.get("row"), dict)
                and e["row"].get("kind") == "flush")
    return {"rows": len(rows), "flush": flush}, None


def memory_reflect(ctx, k) -> None:
    """rules.yaml N24 — the memory store's reflect, bracketed by two store
    censuses.

    Its `rows` counts the kernel's `flush` bookkeeping rows too, and how many
    flushes a run made is the flusher's debounce against the runtime's speed
    (measured: python 334, rust/swift 333). The kernel has no verb that settles
    the flusher, so the count cannot be made still without a wait; instead the
    flush rows are COUNTED at the same instant. A flush appends a batch, so the
    census whose `rows` equals the reflect's is the store the reflect saw — a
    flush landing on either side of the reflect is told apart, not guessed; when
    neither census matches, N24 does not fire and the note says why. The
    censuses ride under `harness` (N14 drops it); `diff.py` N24 reads them first.
    """
    before, err_b = store_census(k)
    reply = call(k, MEMORY, {"type": "reflect", "readme": True, "tree": "none"})
    after, err_a = store_census(k)
    for err in (err_b, err_a):
        if err is not None:
            ctx.rec.note(kind="n24_store_census_failed", reply=err)
    rows = reply.get("rows") if isinstance(reply, dict) else None
    if not any(isinstance(c, dict) and c["rows"] == rows for c in (before, after)):
        ctx.rec.note(kind="n24_reflect_rows_matches_no_census", rows=rows,
                     before=before, after=after)
    ctx.rec.json(f"s1/readme_true/{MEMORY}", reply, harness={"flush_rows": [before, after]})


def ai_send(ctx, k, stub, case: str, text: str, rel: str) -> None:
    holder: dict = {}

    def go() -> None:
        holder["reply"] = call(k, "ai", {"type": "send", "text": text, "client_id": "probe"},
                               timeout=180)

    before = len(stub.requests)
    th = threading.Thread(target=go, daemon=True)
    th.start()
    time.sleep(1.0)
    q = quiesce(stub, k, rec=ctx.rec, label=case, max_s=150)
    th.join(timeout=5)
    ctx.rec.json(rel, {"case": case, "text": text,
                       "reply": holder.get("reply",
                                           {"__harness_error__": "send did not return in the join window"})},
                 harness={"quiesce": q,
                          "request_numbers": [r["n"] for r in stub.requests[before:]]})


def run(ctx) -> None:
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "readings").mkdir()
    (ctx.work / "readings" / "a.txt").write_text("7\n")
    with (ctx.work / "readings" / "big.txt").open("w", encoding="utf-8") as fh:
        for i in range(1, BIG_LINES + 1):
            mid = NEEDLE if i == 150 else "0123456789abcdefghijklmnopqrstuvwxyz" * 13
            fh.write(f"line {i:04d} {mid}\n")

    stub = OpenAIChatStub(rec, "stub", ports.take("stub"))
    stub.start(OpenAIHandler)
    stub.set_cases(CASES)

    cb = {"rest": "*", "web": "*", "ws": "*", "ai": "*"}
    agents = roster(stub.base_url(), cb, sys.executable)
    reach = ["store"] + [a[0] for a in agents] + ["web", "rest", "ws", "ghost"]
    k = ctx.boot(reach=reach, port=ports.take("main"))
    try:
        # ══ s1 — the store first, then the roster ══
        rec.json("s1/create_store", k.create(
            parent_id=k.home, handler_module="file_bridge.tools", allow=["."], id="store", **OPEN,
            root=".fantastic", callable_by={**BASE, **cb}))
        for aid, hm, meta in agents:
            rec.json(f"s1/create_{aid}", k.create(
                **{"parent_id": k.home, "handler_module": hm, "id": aid,
                   **OPEN, "callable_by": cb, **meta}))
        rec.json("s1/sched_schedule", call(k, "sched", {
            "type": "schedule", "target": "readings", "payload": {"type": "list", "path": ""},
            "interval_seconds": 3600}))
        # §5.4: fire it NOW rather than waiting an hour, and a bounded verb next
        # to an unbounded one.
        rec.json("s1/sched_tick_now", call(k, "sched", {"type": "tick_now"}))
        rec.json("s1/sched_history_timeout5", call(k, "sched", {"type": "history", "timeout": 5}))
        time.sleep(3.0)

        la = call(k, "kernel", {"type": "list_agents"})
        rec.json("s1/kernel_list_agents", la)
        all_ids = [a.get("id") for a in (la.get("agents") or [])
                   if isinstance(a, dict) and isinstance(a.get("id"), str)] \
            if isinstance(la, dict) else []
        if not all_ids:
            # NO SUBSTITUTE DATA. Round 2 filled in a guessed id list here
            # (`parity_capture_store.py:485-487`) and every readme below was then
            # taken against the harness's imagination.
            raise RuntimeError(f"list_agents returned no ids: {la!r}")
        rec.json("s1/agent_ids_seen", all_ids)
        for aid in all_ids:
            if aid == MEMORY:
                memory_reflect(ctx, k)
                continue
            rec.json(f"s1/readme_true/{aid}",
                     call(k, aid, {"type": "reflect", "readme": True, "tree": "none"}))
        rec.json("s1/root_reflect_persistence", call(k, k.root_id, {"type": "reflect", "tree": "none"}))
        snapshot(ctx, k, "after_create")

        # ══ s2 — reflect flags and paging ══
        (ctx.work / ".fantastic" / "docs").mkdir(parents=True, exist_ok=True)
        (ctx.work / ".fantastic" / "docs" / "bad.md").write_bytes(b"# head\n\xff\xfe not utf-8\n")
        (ctx.work / ".fantastic" / "docs" / ".md").write_bytes(b"a dotfile doc\n")
        for aid in (k.root_id, "readings", "runner", "ai"):
            label = "root" if aid == k.root_id else aid
            base = f"s2/{label}"
            # tree = the four shapes plus the two falsy ones round 2 never sent
            for val, tag in (("ids", "tree_ids"), ("none", "tree_none"), ("all", "tree_all"),
                             (None, "tree_null"), (True, "tree_true"), (7, "tree_7"),
                             ("ALL", "tree_ALL")):
                rec.json(f"{base}/{tag}", call(k, aid, {"type": "reflect", "tree": val}))
            page_walk(k, rec, aid, {"type": "reflect", "tree": "ids", "limit": 2}, "next",
                      f"{base}/tree_ids_limit2_pages")
            page_walk(k, rec, aid, {"type": "reflect", "tree": "none", "bundles": "all", "limit": 2},
                      "bundles_next", f"{base}/bundles_all_limit2_pages")
            page_walk(k, rec, aid, {"type": "reflect", "tree": "none", "bundles": "ids", "limit": 2},
                      "bundles_next", f"{base}/bundles_ids_limit2_pages")
            for sel, tag in (("index", "index"), ("reflect", "key_reflect"),
                             ("frontend", "key_frontend"), ("frontend/routing", "key_frontend_routing"),
                             ("no-such-key", "bad_key"), ("../escape", "escape"), ("bad", "byte_invalid_doc")):
                rec.json(f"{base}/readme_{tag}", call(k, aid, {"type": "reflect", "tree": "none", "readme": sel}))
            rec.json(f"{base}/detail_full_tree_ids",
                     call(k, aid, {"type": "reflect", "detail": "full", "tree": "ids"}))
        # `after` with three TYPES on every walkable verb — the coercion item.
        for verb, payload in (("list_agents", {}), ("ps", {}), ("ledger", {})):
            for val, tag in ((1, "int"), ("1", "str"), (True, "bool")):
                rec.json(f"s2/after_{verb}_{tag}",
                         call(k, "kernel", {"type": verb, "limit": 2, "after": val, **payload}))

        # ══ s3 — the messages digest ══
        seed_rows = [
            {"role": "user", "content": "naïve — 日本語 😀", "title": "naïve — 日本語 😀"},
            {"role": "assistant", "content": "line one\r\nline two\r\nline three", "title": ""},
            {"role": "user", "kind": "fold", "content": "a fold with an empty refs list",
             "title": "fold title", "refs": [], "level": 1},
            {"role": "assistant", "content": ("😀" * 60) + " 日本語 tail " + ("a" * 80),
             "title": "long non-ascii head"},
            {"role": "user", "content": "", "title": ""},
        ]
        rec.json("s3/seed_rows_input", seed_rows)
        w = call(k, MEMORY, {"type": "append", "owner": "readings", "rows": seed_rows})
        rec.json("s3/rows_write", w)
        ids = w.get("ids") if isinstance(w, dict) and isinstance(w.get("ids"), list) else []
        if not ids:
            raise RuntimeError(f"append returned no ids: {w!r}")
        rec.json("s3/messages_all", call(k, MEMORY, {"type": "read", "owner": "readings", "ids": ids}))
        rec.json("s3/messages_ids_as_strings",
                 call(k, MEMORY, {"type": "read", "owner": "readings", "ids": [str(i) for i in ids]}))
        rec.json("s3/messages_limit_str_2", call(k, MEMORY, {"type": "read", "owner": "readings", "ids": ids, "limit": "2"}))
        rec.json("s3/messages_limit_1", call(k, MEMORY, {"type": "read", "owner": "readings", "ids": ids, "limit": 1}))
        # The id shapes that are not ids: python's `int()` semantics, verbatim.
        rec.json("s3/messages_ids_edge", call(k, MEMORY, {"type": "read", "owner": "readings", "ids": [
            0, -1, "007", "+5", 12345678901234567890, "²"]}))
        if len(ids) > 1:
            rec.json("s3/messages_after_int", call(k, MEMORY, {"type": "read", "owner": "readings", "ids": ids, "after": ids[1]}))
            rec.json("s3/messages_after_str", call(k, MEMORY, {"type": "read", "owner": "readings", "ids": ids, "after": str(ids[1])}))
            rec.json("s3/messages_after_limit1",
                     call(k, MEMORY, {"type": "read", "owner": "readings", "ids": ids, "after": ids[1], "limit": 1}))
        rec.json("s3/rows_read", call(k, MEMORY, {"type": "read", "owner": "readings"}))
        if len(ids) > 1:
            rec.json("s3/rows_read_after_limit2",
                     call(k, MEMORY, {"type": "read", "owner": "readings", "after": ids[0], "limit": 2}))
        for raw, tag in (("\n", "newline"), ("\x1c", "fs")):
            rec.json(f"s3/rows_read_agent_{tag}", call(k, MEMORY, {"type": "read", "owner": raw}))
            rec.json(f"s3/rows_write_agent_{tag}",
                     call(k, MEMORY, {"type": "append", "owner": raw,
                                        "rows": [{"role": "user", "content": "x"}]}))

        # ══ s4 — a short chain, then every AI self verb ══
        ai_send(ctx, k, stub, "chain",
                "CASE=chain#1 Read readings/big.txt through totals, list readings, then store the total.",
                "s4/chain_send")
        pid, pid_src = None, None
        tag_re = re.compile(r"\[reply to (\d+) · totals read\b")
        for entry in stub.requests:
            if entry["answered"].get("case") != "chain":
                continue
            raw = (rec.out / f"stub/req_{entry['n']:03d}.bin").read_bytes()
            try:
                req = json.loads(raw)
            except ValueError:
                continue
            texts = []
            for msg in req.get("messages") or []:
                c = msg.get("content") if isinstance(msg, dict) else None
                if isinstance(c, str):
                    texts.append(c)
                elif isinstance(c, list):
                    texts += [b.get("text", "") for b in c
                              if isinstance(b, dict) and isinstance(b.get("text"), str)]
            m = tag_re.search("\n".join(texts))
            if m:
                pid, pid_src = int(m.group(1)), f"stub/req_{entry['n']:03d}.bin"
                break
        rec.json("s4/pid_from_reply_tag", {"pid": pid, "source": pid_src, "regex": tag_re.pattern})
        if pid is None:
            rec.note(kind="no_totals_read_reply_tag_in_prompts")

        h0 = call(k, "ai", {"type": "history"})
        rec.json("s4/history_noargs", h0)
        hrows = h0.get("rows") if isinstance(h0, dict) and isinstance(h0.get("rows"), list) else []
        rec.json("s4/history_limit2", call(k, "ai", {"type": "history", "limit": 2}))
        if len(hrows) > 1:
            rec.json("s4/history_limit2_after",
                     call(k, "ai", {"type": "history", "limit": 2, "after": hrows[1].get("id")}))
            rec.json("s4/history_limit2_after_str",
                     call(k, "ai", {"type": "history", "limit": 2, "after": str(hrows[1].get("id"))}))
        else:
            rec.note(kind="history_has_fewer_than_2_rows", reply=h0)
        rec.json("s4/grep_query", call(k, "ai", {"type": "grep", "query": "needle total=42"}))
        if pid is not None:
            rec.json("s4/grep_query_ids_pid", call(k, "ai", {"type": "grep", "query": "line 0007", "ids": [pid]}))
            rec.json("s4/pull_pid", call(k, "ai", {"type": "pull", "pid": pid}))
            rec.json("s4/pull_pid_str_offset_len",
                     call(k, "ai", {"type": "pull", "pid": str(pid), "offset": 100, "length": 200}))
            rec.json("s4/pull_offset_x", call(k, "ai", {"type": "pull", "pid": pid, "offset": "x"}))
            rec.json("s4/pull_past_end", call(k, "ai", {"type": "pull", "pid": pid, "offset": 10 ** 9}))
        rec.json("s4/pull_no_pid", call(k, "ai", {"type": "pull"}))
        rec.json("s4/recall_query", call(k, "ai", {"type": "recall", "query": "totals"}))
        rec.json("s4/recall_query_before", call(k, "ai", {"type": "recall", "query": "totals", "before": 3}))
        rec.json("s4/recall_before", call(k, "ai", {"type": "recall", "before": 2, "limit": 1}))
        rec.json("s4/context_status", call(k, "ai", {"type": "context_status"}))
        rec.json("s4/status_noargs", call(k, "ai", {"type": "status"}))
        rec.json("s4/status_client_probe", call(k, "ai", {"type": "status", "client_id": "probe"}))
        rec.json("s4/rows_read_ai_after_chain", call(k, MEMORY, {"type": "read", "owner": "ai"}))

        # ══ s5 — one human send per tool-parse fault ══
        for case in FAULT_CASES:
            ai_send(ctx, k, stub, case, f"CASE={case}#1 fault probe {case}.", f"s5/{case}_send")

        # context_recall LAST: it rewrites the view the fault prompts would use.
        rec.json("s4/context_recall_dry_true", call(k, "ai", {"type": "context_recall", "dry": True}, timeout=120))
        rec.json("s4/context_recall_dry_str_false",
                 call(k, "ai", {"type": "context_recall", "dry": "false"}, timeout=120))
        rec.json("s4/context_recall_dry_1", call(k, "ai", {"type": "context_recall", "dry": 1}, timeout=120))
        stub.responder = lambda req, raw: CTX_RECALL_ANSWER
        rec.json("s4/context_recall_model", call(k, "ai", {"type": "context_recall"}, timeout=120))
        for answer, tag in (("{}", "empty_obj"), ('{"why": 0}', "why_0"), ('{"expand": 5}', "expand_5")):
            stub.responder = (lambda a: (lambda req, raw: a))(answer)
            rec.json(f"s4/context_recall_answer_{tag}", call(k, "ai", {"type": "context_recall"}, timeout=120))
        stub.responder = None
        rec.json("s4/context_status_after_recall", call(k, "ai", {"type": "context_status"}))
        rec.json("s4/history_final", call(k, "ai", {"type": "history"}))
        rec.json("s4/rows_read_ai_final", call(k, MEMORY, {"type": "read", "owner": "ai"}))

        time.sleep(3.0)
        for verb in ("list_agents", "ps", "ledger"):
            rec.json(f"final/kernel_{verb}", call(k, "kernel", {"type": verb}))
        snapshot(ctx, k, "final")
    finally:
        stub.flush()
        rec.json("stub/index", stub.requests)
        rec.http("http", k.http)
        k.close()
        stub.stop()
        save_kernel_log(rec, k)
