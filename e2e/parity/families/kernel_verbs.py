"""kernel_verbs — the root's own verbs and their refusals (C13, C17–C35, C39).

§9.1 marks this whole row `source` in both rounds: four refusal captures in
round 1 and nothing after. Every id shape, every two-fault precedence pair,
every grant map with a token key, the blob verbs with and without a grant — all
of it was written into the ports from reading python, and none of it was driven.

The leg's grants are EXACT here (a seed override, as
`SCR/parity_capture_wire.py:161-195` did), because half these cases are about
authority and a leg granted `"*"` on everything cannot show a refusal:
  * `limited` is granted `['reflect']` and nothing else — C21;
  * `web_loader` is in the reach so C35's alias has somewhere to go.
"""

from __future__ import annotations

import json
import time

from families import BASE, OPEN, call, save_kernel_log

NAME = "kernel_verbs"


def run(ctx) -> None:  # noqa: C901
    rec, ports = ctx.rec, ctx.ports

    def seed_extra(k, w, port):
        """Narrow the legs: `limited` gets ONE verb, by name."""
        for p in ("agents/web/agent.json", "agents/web/agents/ws/agent.json",
                  "agents/web/agents/rest/agent.json"):
            r = json.loads((w / p).read_text())
            r["can_call"]["limited"] = ["reflect"]
            (w / p).write_text(json.dumps(r, indent=2))

    reach = ["files", "readings", "totals", "limited", "joiner", "web_loader", "child_of_loader",
             "blobber", "ghost", "web", "rest", "ws", "kernel_store"]
    k = ctx.boot(reach=reach, port=ports.take("main"), seed_extra=seed_extra)
    cb = {**BASE, "rest": "*", "web": "*", "ws": "*"}
    try:
        (ctx.work / "a.txt").write_text("a\n")
        rec.json("setup/files", k.create(parent_id=k.root_id, id="files",
                                         handler_module="file_bridge.tools", root=".", **OPEN,
                                         callable_by=cb))
        rec.json("setup/readings", k.create(parent_id=k.root_id, id="readings",
                                            handler_module="file_bridge.tools", root=".", **OPEN,
                                            callable_by=cb))
        rec.json("setup/limited", k.create(parent_id=k.root_id, id="limited",
                                           handler_module="cli.tools", **OPEN, callable_by=cb))
        rec.json("setup/kernel_store", k.create(parent_id=k.root_id, id="kernel_store",
                                                handler_module="file_bridge.tools",
                                                root=".fantastic", **OPEN, callable_by=cb))
        time.sleep(0.8)

        # ── C13 — persist_blob / load_blob ──
        C13 = [
            ("persist_no_grant", k.root_id, {"type": "persist_blob", "agent_id": "files",
                                             "key": "k1", "blob": "hello"}),
            ("load_no_grant", k.root_id, {"type": "load_blob", "agent_id": "files", "key": "k1"}),
            ("persist_foreign_agent", k.root_id, {"type": "persist_blob", "agent_id": "ghost",
                                                  "key": "k1", "blob": "hello"}),
            ("persist_non_utf8", k.root_id, {"type": "persist_blob", "agent_id": "files",
                                             "key": "k2", "blob": "\udcff"}),
            ("load_missing_key", k.root_id, {"type": "load_blob", "agent_id": "files",
                                             "key": "nope"}),
            ("persist_no_key", k.root_id, {"type": "persist_blob", "agent_id": "files",
                                           "blob": "x"}),
        ]
        for case, target, payload in C13:
            rec.json(f"C13/{case}", {"target": target, "payload": payload,
                                     "reply": call(k, target, payload)})

        # ── C17 — ps/ledger walks with int and decimal-string `after` ──
        for verb in ("ps", "ledger"):
            for val, tag in ((0, "int0"), (1, "int1"), ("1", "str1"), ("01", "str01"),
                             (-1, "neg"), (True, "bool")):
                rec.json(f"C17/{verb}_after_{tag}",
                         call(k, "kernel", {"type": verb, "limit": 2, "after": val}))

        # ── C18 — message ids and a cancel id that is not one ──
        rec.json("C18/messages_ids_shapes",
                 call(k, "kernel", {"type": "messages", "ids": [0, -1, "007", "+5"]}))
        rec.json("C18/cancel_plus_pid", call(k, "kernel", {"type": "cancel", "task_id": "+1"}))
        rec.json("C18/cancel_missing", call(k, "kernel", {"type": "cancel"}))

        # ── C19 — verbs on the root, and the ids they name ──
        rec.json("C19/create_id_dotdot",
                 call(k, k.root_id, {"type": "create_agent", "id": "../x",
                                     "handler_module": "cli.tools", "parent_id": k.root_id}))
        rec.json("C19/frob_on_root", call(k, k.root_id, {"type": "frob"}))
        rec.json("C19/can_call_int_value",
                 call(k, k.root_id, {"type": "create_agent", "id": "c19a",
                                     "handler_module": "cli.tools", "parent_id": k.root_id,
                                     "can_call": {"readings": 5}}))
        # ── C20 — a grant whose KEY is a token, and one whose key is `$x` ──
        rec.json("C20/read_without_grant", call(k, "kernel_store", {"type": "list", "path": ""}))
        for key, tag in (("$x", "unknown_token"), ("$loader", "loader_token"), ("*", "star_key")):
            rec.json(f"C20/create_can_call_{tag}",
                     call(k, k.root_id, {"type": "create_agent", "id": f"c20_{tag}",
                                         "handler_module": "cli.tools", "parent_id": k.root_id,
                                         "can_call": {key: "*"}}))
        # ── C21 — an empty verb LIST is not `"*"` ──
        rec.json("C21/limited_reflect", call(k, "limited", {"type": "reflect"}))
        rec.json("C21/limited_say", call(k, "limited", {"type": "say", "text": "x"}))
        rec.json("C21/create_can_call_empty_list",
                 call(k, k.root_id, {"type": "create_agent", "id": "c21a",
                                     "handler_module": "cli.tools", "parent_id": k.root_id,
                                     "can_call": {"limited": []}}))
        # ── C22 — the same execution id, a different parent ──
        for n, parent in ((1, k.root_id), (2, "files")):
            before = len(k.http)
            r = k.send(k.root_id, {"type": "create_agent", "id": "joiner", "parent_id": parent,
                                   "handler_module": "cli.tools"}, exec_id="kv-joined-1")
            rec.json(f"C22/joiner_{n}", {"parent": parent, "reply": r, "http": k.http[before:]})
        # ── C25/C26/C27 — id shapes, duplicate/missing, update and delete ──
        for ident, tag in (("123", "digits"), ("a:b", "colon"), ("", "empty"),
                           ("with space", "space"), ("Ünï", "nonascii")):
            rec.json(f"C25/create_id_{tag}",
                     call(k, k.root_id, {"type": "create_agent", "id": ident,
                                         "handler_module": "cli.tools", "parent_id": k.root_id}))
        C26 = [
            ("duplicate_id", {"type": "create_agent", "id": "files",
                              "handler_module": "cli.tools", "parent_id": k.root_id}),
            ("missing_parent_id", {"type": "create_agent", "id": "c26a",
                                   "handler_module": "cli.tools"}),
            ("empty_handler_module", {"type": "create_agent", "id": "c26b",
                                      "handler_module": "", "parent_id": k.root_id}),
            ("empty_parent_id", {"type": "create_agent", "id": "c26c",
                                 "handler_module": "cli.tools", "parent_id": ""}),
            ("non_map_grant", {"type": "create_agent", "id": "c26d",
                               "handler_module": "cli.tools", "parent_id": k.root_id,
                               "can_call": ["files"]}),
            ("two_faults_id_and_parent", {"type": "create_agent", "id": "",
                                          "handler_module": "cli.tools", "parent_id": ""}),
            ("two_faults_dup_and_handler", {"type": "create_agent", "id": "files",
                                            "handler_module": "", "parent_id": k.root_id}),
            ("two_faults_parent_and_handler", {"type": "create_agent", "id": "c26e",
                                               "handler_module": "", "parent_id": "ghost"}),
        ]
        for case, payload in C26:
            rec.json(f"C26/{case}", call(k, k.root_id, payload))
        C27 = [
            ("update_handler_module", {"type": "update_agent", "id": "files",
                                       "handler_module": "cli.tools"}),
            ("update_reserved_key", {"type": "update_agent", "id": "files", "parent_id": "ghost"}),
            ("update_id_empty", {"type": "update_agent", "id": ""}),
            ("update_unknown", {"type": "update_agent", "id": "ghost", "description": "x"}),
            ("delete_id_empty", {"type": "delete_agent", "id": ""}),
            ("delete_root", {"type": "delete_agent", "id": k.root_id}),
            ("delete_unknown", {"type": "delete_agent", "id": "ghost"}),
        ]
        for case, payload in C27:
            rec.json(f"C27/{case}", call(k, k.root_id, payload))
        # delete_lock with a locked child
        rec.json("C27/create_locked_child",
                 call(k, k.root_id, {"type": "create_agent", "id": "locked_child",
                                     "handler_module": "cli.tools", "parent_id": "files",
                                     "delete_lock": True}))
        rec.json("C27/delete_parent_of_locked", call(k, k.root_id, {"type": "delete_agent",
                                                                    "id": "files"}))
        rec.json("C27/delete_locked_child", call(k, k.root_id, {"type": "delete_agent",
                                                                "id": "locked_child"}))
        # ── C32 — a probe asked a kernel verb ──
        rec.json("C32/readings_shutdown_agents", call(k, "readings", {"type": "shutdown_agents"}))
        rec.json("C32/readings_list_agents", call(k, "readings", {"type": "list_agents"}))
        # ── C34 is driven in `wire` (it needs the raw octet leg) ──
        # ── C35 — an alias, a child of the alias, a delete, then a send ──
        rec.json("C35/create_alias",
                 call(k, k.root_id, {"type": "create_agent", "id": "web_loader",
                                     "handler_module": "static_loader.tools",
                                     "parent_id": k.root_id, "root": ".", **OPEN,
                                     "callable_by": cb}))
        rec.json("C35/send_to_alias", call(k, "web_loader", {"type": "reflect"}))
        rec.json("C35/child_of_alias",
                 call(k, k.root_id, {"type": "create_agent", "id": "child_of_loader",
                                     "handler_module": "cli.tools", "parent_id": "web_loader",
                                     **OPEN, "callable_by": cb}))
        rec.json("C35/delete_alias", call(k, k.root_id, {"type": "delete_agent",
                                                         "id": "web_loader"}))
        rec.json("C35/send_after_delete", call(k, "web_loader", {"type": "reflect"}))
        rec.json("C35/child_after_parent_delete", call(k, "child_of_loader", {"type": "reflect"}))
        # ── C39 — a control character as an agent name ──
        for raw, tag in (("\n", "newline"), ("\x1c", "fs"), ("", "empty")):
            rec.json(f"C39/rows_read_{tag}", call(k, "kernel", {"type": "rows_read", "agent": raw}))
            rec.json(f"C39/rows_write_{tag}",
                     call(k, "kernel", {"type": "rows_write", "agent": raw,
                                        "rows": [{"role": "user", "content": "x"}]}))
        # ── the shape of every kernel verb's refusal when it is asked nothing ──
        for verb in ("create_agent", "update_agent", "delete_agent", "list_agents", "ps", "ledger",
                     "messages", "rows_read", "rows_write", "cancel", "persist_blob", "load_blob",
                     "shutdown_agents", "nope"):
            rec.json(f"bare/{verb}", call(k, "kernel", {"type": verb}))
        rec.json("final/kernel_list_agents", call(k, "kernel", {"type": "list_agents"}))
        rec.json("final/kernel_ledger", call(k, "kernel", {"type": "ledger"}))
    finally:
        rec.http("http", k.http)
        k.close()
        save_kernel_log(rec, k)
