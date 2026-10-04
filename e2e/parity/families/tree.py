"""tree — every bundle's create/reflect/readme surface, plus the web legs, in ONE
boot (`SCR/parity_capture.py` + `SCR/parity_web.py`).

STORELESS ON PURPOSE. Nothing wires a persistence provider here, so an agent's
`readme` comes from wherever the runtime keeps it with no disk behind it —
python answers a null non-root `readme=true` while swift seeds straight to disk,
and that difference only exists when there is no store to hide it. `store`
covers the wired case.

Three changes from round 1, each for a measured reason:
  * `grep {query:'reading'}` — round 1 sent `pattern` (`parity_capture.py:116`),
    which is not the argument name, so the item under test never ran;
  * `yaml_state.tools` → `absent_bundle.tools` — the yaml entry point is a STALE
    record in the source venv (D12) and `yaml` measured the venv, not the kernel.
    An id whose handler module does not exist anywhere is the real question;
  * `totals2`/`sload2` — the same two bundles with EMPTY-STRING arguments,
    because a falsy argument takes a different branch than a missing one.
"""

from __future__ import annotations

import re
import sys
import time

from families import MEMORY, OPEN, ROOT_ID, VLLM, ai, call, save_kernel_log, vllm
from spec import CTX_RESERVE, CTX_WINDOW
from stubs.openai_chat import Handler as OpenAIHandler, OpenAIChatStub

NAME = "tree"

SCRIPT = [
    '<call name="send"><target_id>readings</target_id><type>reflect</type></call>\n'
    '<call name="send"><target_id>readings</target_id><type>list</type><path></path></call>\n'
    '<call name="send"><target_id>ghost</target_id><type>reflect</type></call>\n'
    '<call name="send"><type>reflect</type></call>',
    "I have looked around and will think about the next step.",
    '<call name="await"></call>',
    '<call name="send"><target_id>totals</target_id><type>write</type><path>t.txt</path>'
    "<bytes>42</bytes><title>store the total</title></call>\n"
    '<call name="send"><target_id>kernel_store</target_id><type>read_stream</type>'
    "<path>readings/a.txt</path><offset>0</offset></call>\n"
    "<call name=\"send\"><target_id>totals",
    '<call name="done"></call>',
]


def roster(stub_url: str, cb: dict, interp: str) -> list:
    return [
        ("runner", "exec_bridge.tools", dict(cwd=".", allowed=[rf"{re.escape(interp)} \S+"])),
        ("readings", "file_bridge.tools",
         dict(allow=["."], root="readings", description="Sensor readings — one integer per file.")),
        ("kernel_store", "file_bridge.tools", dict(allow=["."], root=".", callable_by=cb)),
        ("totals", "file_bridge.tools", dict(allow=["."], root=".")),
        # The same bundle with a FALSY argument — a different branch everywhere.
        ("totals2", "file_bridge.tools", dict(allow=["."], root=".", description="")),
        ("sched", "scheduler.tools", {}),
        ("wsb", "ws_bridge.tools", dict(url="ws://127.0.0.1:9/nowhere")),
        ("relay", "relay_connector.tools", dict(url="ws://127.0.0.1:9/nowhere")),
        ("sload", "static_loader.tools", dict(root=".")),
        ("sload2", "static_loader.tools", dict(root=".", display_name="")),
        ("cli", "cli.tools", {}),
        ("olla", "ollama_backend.tools", ai(model="m", endpoint="http://127.0.0.1:9")),
        # NOT `yaml_state.tools`: see the module docstring.
        ("absent", "absent_bundle.tools", {}),
        ("dispenser", "dispenser_probe.tools", {}),
        ("plant", "plant_probe.tools", {}),
        ("purchase", "purchase_probe.tools", {}),
        ("user", "user_probe.tools", {}),
        ("oracle", "oracle_probe.tools", dict(lines=["the answer is 42"])),
        ("ai", VLLM,
         ai(**vllm(stub_url, context_window=CTX_WINDOW), sampling={"x": 1, "y": None},
              output_reserve=CTX_RESERVE,
              can_call={"readings": "*", "totals": "*", "runner": "*",
                        "kernel_store": "*", ROOT_ID: "*"})),
    ]


def run(ctx) -> None:
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "readings").mkdir()
    (ctx.work / "readings" / "a.txt").write_text("7\n")

    stub = OpenAIChatStub(rec, "stub", ports.take("stub"))
    stub.start(OpenAIHandler)
    stub.set_chain("tree", SCRIPT)

    cb = {"rest": "*", "web": "*", "ws": "*", "ai": "*"}
    agents = roster(stub.base_url(), cb, sys.executable)
    ids = [a[0] for a in agents] + ["ghost", "web", "rest", "ws"]
    k = ctx.boot(reach=ids, port=ports.take("main"))
    try:
        rec.json("root_boot_reflect_full",
                 call(k, k.root_id, {"type": "reflect", "detail": "full"}))
        for aid, hm, meta in agents:
            m = {"parent_id": k.home, "handler_module": hm, "id": aid,
                 **OPEN, "callable_by": cb, **meta}
            rec.json(f"create_{aid}", k.create(**m))
        time.sleep(1.5)

        for aid, _hm, _meta in agents:
            rec.json(f"reflect_{aid}", call(k, aid, {"type": "reflect"}))
            rec.json(f"reflect_full_{aid}", call(k, aid, {"type": "reflect", "detail": "full"}))
            rec.json(f"readme_{aid}", call(k, aid, {"type": "reflect", "readme": True, "tree": "none"}))
        # The three legs the driver seeds are agents like any other and carry the
        # web bundles' own readmes (`SCR/parity_web.py`, folded in here).
        for aid in ("web", "rest", "ws"):
            rec.json(f"reflect_{aid}", call(k, aid, {"type": "reflect"}))
            rec.json(f"reflect_full_{aid}", call(k, aid, {"type": "reflect", "detail": "full"}))
            rec.json(f"readme_{aid}", call(k, aid, {"type": "reflect", "readme": True, "tree": "none"}))
        rec.json("reflect_absent_agent", call(k, "ghost", {"type": "reflect"}))

        rec.json("root_reflect", call(k, k.root_id, {"type": "reflect"}))
        rf = call(k, k.root_id, {"type": "reflect", "detail": "full"})
        rec.json("root_reflect_full", rf)
        idx = call(k, k.root_id, {"type": "reflect", "readme": "index", "tree": "none"})
        rec.json("root_readme_index", idx)

        keys: set[str] = set()

        def walk(x):
            if isinstance(x, dict):
                if isinstance(x.get("key"), str):
                    keys.add(x["key"])
                for v in x.values():
                    walk(v)
            elif isinstance(x, list):
                for v in x:
                    walk(v)

        walk(rf.get("readmes") if isinstance(rf, dict) else None)
        walk(idx)
        rec.json("root_readme_keys_seen", sorted(keys))
        for key in sorted(keys):
            rec.json(f"root_readme_{key.replace('/', '__')}",
                     call(k, k.root_id, {"type": "reflect", "readme": key, "tree": "none"}))

        for verb in ("list_agents", "ps", "ledger"):
            rec.json(f"kernel_{verb}", call(k, "kernel", {"type": verb}))

        # ── the scripted chain ──
        rec.json("ai_send_reply",
                 call(k, "ai", {"type": "send", "text": "Sum every reading and store the total in totals.",
                                "client_id": "probe"}, timeout=180))
        last, t0 = -1, time.monotonic()
        while time.monotonic() - t0 < 90:
            time.sleep(3)
            if len(stub.requests) == last and len(stub.requests) >= len(SCRIPT):
                break
            last = len(stub.requests)
        time.sleep(3)

        rec.json("ai_rows_read", call(k, MEMORY, {"type": "read", "owner": "ai"}))
        for verb, extra in (("history", {}), ("status", {"client_id": "probe"}),
                            ("context_status", {}), ("recall", {"query": "total"}),
                            # `query`, not `pattern` — round 1 aimed this at a name
                            # the verb does not take and measured the refusal.
                            ("grep", {"query": "reading"})):
            rec.json(f"ai_{verb}", call(k, "ai", {"type": verb, **extra}))

        rec.json("kernel_ps_after", call(k, "kernel", {"type": "ps"}))
        rec.json("kernel_ledger_after", call(k, "kernel", {"type": "ledger"}))

        rec.json("refusal_unknown_verb_file", call(k, "readings", {"type": "nope"}))
        rec.json("refusal_missing_arg_file", call(k, "readings", {"type": "rename"}))
        # minimal model (owner 2026-10-04): an id naming no agent is not the leg's
        # child, so the mutator refusal answers before the unknown-agent one.
        rec.json("refusal_unknown_agent", call(k, "kernel", {"type": "delete_agent", "id": "ghost"}))
        rec.json("refusal_no_type", call(k, "readings", {}))
    finally:
        stub.flush()
        rec.http("http", k.http)
        k.close()
        stub.stop()
        save_kernel_log(rec, k)
