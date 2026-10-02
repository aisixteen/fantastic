"""bundle_verbs — the refusals, the argument coercions and the OS errors.

These are the items §9.1 marks `source` on BOTH rounds: nothing ever drove a
`delete` of a populated directory, a `cols:70000`, a manifest that is a YAML
scalar `no`, a `paste_image` with mime `''`. They were read out of python and
written into rust and swift from the reading. Reading is not measuring, so they
are driven here.

One boot, no model stub, every case a `{target, payload}` over REST and its whole
reply — including the HTTP envelope, because a refusal's STATUS is an item too. A
case may carry `pre` (steps to run first) and `post` (steps to run after, e.g.
a `reflect` that shows what the verb did). Order inside a surface is fixed.
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

from families import BASE, OPEN, VLLM, ai, call, save_kernel_log, vllm

NAME = "bundle_verbs"


def fixtures(work: Path, interp: str) -> list:
    """The agents the cases run against. Several are DELIBERATELY broken (a root
    outside the tree, a shell that does not exist, a cwd that does not exist) —
    that is the case, not a mistake."""
    return [
        # ── file_bridge ──
        ("files", "file_bridge.tools", dict(allow=["."], root=".")),
        ("files_ro", "file_bridge.tools", dict(allow=["."], root="sub", readonly=True)),
        ("files_escape", "file_bridge.tools", dict(allow=["."], root="../outside")),
        ("files_symroot", "file_bridge.tools", dict(allow=["."], root="symroot")),
        ("files_env_rule", "file_bridge.tools", dict(allow=["."], root=".", ingress_rule={"env": "X"})),
        ("files_null_rule", "file_bridge.tools", dict(allow=["."], root=".", ingress_rule=None,
                                                      auth_token="tok")),
        ("files_auth_stem", "file_bridge.tools", dict(allow=["."], root=".", ingress_rule="password")),
        # ── the text verbs, on a second bridge over the same root ──
        ("editor", "file_bridge.tools", dict(allow=["."], root=".")),
        # ── exec_bridge ──
        ("runner", "exec_bridge.tools", dict(cwd=".", allowed=[rf"{interp} .*"])),
        ("runner_badre", "exec_bridge.tools", dict(cwd=".", allowed=["(unclosed"])),
        ("runner_named", "exec_bridge.tools", dict(cwd=".", allowed=[r"(?P<n>\S+) .*"])),
        ("runner_alt", "exec_bridge.tools", dict(cwd=".", allowed=[r"/bin/echo a|/bin/echo ab"])),
        ("runner_rulename", "exec_bridge.tools", dict(cwd=".", allowed=["x"],
                                                      ingress_rule="no_such_rule")),
        # the child's own shell: SHELL passed down + the `shell` permission; and
        # the same permission with nothing passed down
        ("term_shell", "exec_bridge.tools", dict(cwd=".", env={"SHELL": "/bin/sh"}, shell=True)),
        ("term_noshell", "exec_bridge.tools", dict(cwd=".", shell=True)),
        # ── scheduler ──
        ("sched", "scheduler.tools", dict(can_call={**BASE, "files": "*"})),
        # ── static_loader ──
        ("sload", "static_loader.tools", dict(root=".")),
        # ── cli ──
        ("cli", "cli.tools", {}),
        # ── probes ──
        ("plant", "plant_probe.tools", {}),
        ("dispenser", "dispenser_probe.tools", {}),
        ("purchase", "purchase_probe.tools", {}),
        ("user", "user_probe.tools", dict(answer="yes")),
        ("user2", "user_probe.tools", dict(answer="no")),
        ("oracle", "oracle_probe.tools", dict(lines=["the answer is 42"])),
        ("oracle2", "oracle_probe.tools", dict(lines=[42, None, True])),
        # ── web ──
        ("web4", "web.tools", {}),
        # ── AI (no endpoint reached: every case is a NON-send verb) ──
        ("ai", VLLM, ai(**vllm("http://127.0.0.1:9", model="m"))),
        ("olla", "ollama_backend.tools", ai(model="m", endpoint="http://127.0.0.1:9")),
    ]


def file_bridge_cases(interp: str) -> list:
    return [
        ("validate_meta_env_rule", "files_env_rule", {"type": "reflect"}),
        ("validate_meta_null_rule_auth", "files_null_rule", {"type": "reflect"}),
        ("auth_stem_only", "files_auth_stem", {"type": "reflect"}),
        ("root_dotdot", "files_escape", {"type": "list", "path": ""}),
        ("root_symlink_outside", "files_symroot", {"type": "list", "path": ""}),
        ("list_missing_path", "files", {"type": "list"}),
        ("list_not_a_dir", "files", {"type": "list", "path": "hello.txt"}),
        ("list_escape_middle", "files", {"type": "list", "path": "sub/../../x"}),
        ("unknown_verb_sealed_leg", "files_null_rule", {"type": "nope"}),
        ("boot_on_sealed_leg", "files_null_rule", {"type": "boot"}),
        ("shutdown_on_sealed_leg", "files_null_rule", {"type": "shutdown"}),
        ("readonly_write", "files_ro", {"type": "write_stream", "path": "x.txt", "offset": 0,
                                        "bytes": "nope"}),
        ("readonly_escape", "files_ro", {"type": "read_stream", "path": "../hello.txt",
                                         "offset": 0}),
        ("pump_falsy_args", "files", {"type": "pump", "source": "", "sink": "", "length": 0}),
        ("delete_populated_dir", "files", {"type": "delete", "path": "sub"}),
        ("delete_recursive_1", "files", {"type": "delete", "path": "sub", "recursive": 1}),
        ("delete_truncate_1", "files", {"type": "delete", "path": "hello.txt", "truncate": 1}),
        ("list_hidden_list", "files", {"type": "list", "path": "", "hidden": ["a", "a", 1]}),
        ("list_dangling_symlink", "files", {"type": "list", "path": "links"}),
        ("stat_symlinked_file", "files", {"type": "list", "path": "links", "hidden": True}),
        ("read_stream_negative_offset", "files", {"type": "read_stream", "path": "hello.txt",
                                                  "offset": -1}),
        ("write_stream_bytes_int", "files", {"type": "write_stream", "path": "n.txt",
                                             "offset": 0, "bytes": 42}),
    ]


def text_cases() -> list:
    """The file_bridge text verbs over their argument tables, driven once each."""
    one = [{"path": "doc.txt", "hunks": 1}]
    return [
        ("write_new", "editor", {"type": "write", "path": "doc.txt", "bytes": "one\ntwo\nthree\n"}),
        ("write_no_bytes", "editor", {"type": "write", "path": "doc.txt"}),
        ("write_bytes_int", "editor", {"type": "write", "path": "doc2.txt", "bytes": 7}),
        ("read_whole", "editor", {"type": "read", "path": "doc.txt"}),
        ("read_offset_limit", "editor", {"type": "read", "path": "doc.txt", "offset": 1, "limit": 1}),
        ("read_offset_past_end", "editor", {"type": "read", "path": "doc.txt", "offset": 999}),
        ("read_missing", "editor", {"type": "read", "path": "nope.txt"}),
        ("edit_ok", "editor", {"type": "edit", "edits": one,
                               "bytes": "<<<<<<< SEARCH\ntwo\n=======\nTWO\n>>>>>>> REPLACE\n"}),
        ("edit_empty_search", "editor", {"type": "edit", "edits": one,
                                         "bytes": "<<<<<<< SEARCH\n=======\nHEAD\n>>>>>>> REPLACE\n"}),
        ("edit_no_match", "editor", {"type": "edit", "edits": one,
                                     "bytes": "<<<<<<< SEARCH\nabsent\n=======\nx\n>>>>>>> REPLACE\n"}),
        ("edit_malformed", "editor", {"type": "edit", "edits": one, "bytes": "not a patch"}),
        ("edit_hunk_count", "editor", {"type": "edit", "edits": [{"path": "doc.txt", "hunks": 3}],
                                       "bytes": "<<<<<<< SEARCH\nTWO\n=======\ntwo\n>>>>>>> REPLACE\n"}),
        ("write_if_match_stale", "editor", {"type": "write", "path": "doc.txt", "bytes": "x",
                                            "if_match": "0000000000000000"}),
        ("append_ok", "editor", {"type": "append", "path": "doc.txt", "bytes": "four\n"}),
        ("append_missing_file", "editor", {"type": "append", "path": "nope.txt", "bytes": "x"}),
        ("undo_append", "editor", {"type": "undo", "path": "doc.txt"}),
        ("read_after_undo", "editor", {"type": "read", "path": "doc.txt"}),
        ("undo_again_is_redo", "editor", {"type": "undo", "path": "doc.txt"}),
        ("undo_no_slot", "editor", {"type": "undo", "path": "never.txt"}),
    ]


def exec_cases(interp: str) -> list:
    return [
        ("shutdown_mail", "runner", {"type": "shutdown"}),
        ("boot_again", "runner", {"type": "boot"}),
        ("spawn_failure", "runner", {"type": "start", "command": "/no/such/program", "args": []}),
        ("status_task_id_int", "runner", {"type": "status", "task_id": 1}),
        ("job_id_empty", "runner", {"type": "status", "job_id": ""}),
        ("args_null", "runner", {"type": "start", "command": interp, "args": None}),
        ("args_bool", "runner", {"type": "start", "command": interp, "args": True}),
        ("allowlist_repr_refusal", "runner", {"type": "start", "command": "/bin/echo",
                                              "args": ["not allowed"]}),
        ("broken_regex", "runner_badre", {"type": "start", "command": "/bin/echo", "args": ["x"]}),
        ("named_group_regex", "runner_named", {"type": "start", "command": "/bin/echo",
                                               "args": ["x"]}),
        ("fullmatch_alternation", "runner_alt", {"type": "start", "command": "/bin/echo",
                                                 "args": ["ab"]}),
        ("unknown_rule_name", "runner_rulename", {"type": "reflect"}),
        ("long_line_65537", "runner", {"type": "start", "command": interp,
                                       "args": ["-c", "print('x' * 65537)"]}),
        ("cr_and_invalid_bytes", "runner", {"type": "start", "command": interp,
                                            "args": ["-c",
                                                     "import sys; sys.stdout.buffer.write(b'a\\r\\nb\\xff\\n')"]}),
        ("stdout_closed_while_alive", "runner", {"type": "start", "command": interp,
                                                 "args": ["-c",
                                                          "import os,sys,time; os.close(1); time.sleep(1)"]}),
        ("env_self_id_override", "runner", {"type": "start", "command": interp,
                                            "args": ["-c",
                                                     "import os; print(os.environ.get('FANTASTIC_SELF_ID'))"],
                                            "env": {"FANTASTIC_SELF_ID": "impostor"}}),
        ("program_sends_watch", "runner", {"type": "start", "command": interp,
                                           "args": ["-c",
                                                    "import json,os,sys;"
                                                    "fd=int(os.environ.get('FANTASTIC_CTRL_FD') or 1);"
                                                    "os.write(fd, (json.dumps({'type':'watch','src':'files'})+chr(10)).encode())"]}),
        # a PTY start with no command → the child's $SHELL, reported resolved
        ("shell_autodiscover", "term_shell", {"type": "start", "pty": {"cols": 80, "rows": 24}}),
        ("shell_status", "term_shell", {"type": "status"}),
        ("shell_stop", "term_shell", {"type": "stop"}),
        ("shell_missing", "term_noshell", {"type": "start", "pty": {"cols": 80, "rows": 24}}),
        ("shell_extra_args", "term_shell", {"type": "start", "args": ["-c", "id"],
                                            "pty": {"cols": 80, "rows": 24}}),
        ("shell_pipe_needs_command", "term_shell", {"type": "start"}),
        ("shell_reflect", "term_shell", {"type": "reflect"}),
        ("status_after_all", "runner", {"type": "status"}),
        ("clear_then_status", "runner", {"type": "clear"}),
    ]


def scheduler_cases() -> list:
    return [
        ("shutdown", "sched", {"type": "shutdown"}),
        ("two_fault_validation", "sched", {"type": "schedule"}),
        ("schedule_id_empty", "sched", {"type": "unschedule", "schedule_id": ""}),
        ("schedule_ok", "sched", {"type": "schedule", "target": "files",
                                  "payload": {"type": "list", "path": ""},
                                  "interval_seconds": 3600}),
        ("paused_1", "sched", {"type": "pause", "paused": 1}),
        ("repr_in_refusal", "sched", {"type": "schedule", "target": ["a"],
                                      "payload": {"type": "list"}, "interval_seconds": "soon"}),
        ("history", "sched", {"type": "history"}),
        ("pause_reply_order", "sched", {"type": "pause"}),
        ("resume", "sched", {"type": "resume"}),
    ]


def probe_cases() -> list:
    return [
        ("plant_pump_1", "plant", {"type": "pump"}),
        ("plant_pump_2", "plant", {"type": "pump"}),
        ("plant_pump_3", "plant", {"type": "pump"}),
        ("plant_valve", "plant", {"type": "valve"}),
        ("plant_count", "plant", {"type": "count"}),
        ("plant_zap", "plant", {"type": "zap"}),
        ("dispenser_zap", "dispenser", {"type": "zap"}),
        ("purchase_zap", "purchase", {"type": "zap"}),
        ("user_ask_text", "user", {"type": "ask", "question": "text?", "kind": "text"}),
        ("user_ask_single", "user", {"type": "ask", "question": "which?", "kind": "single",
                                     "options": ["a", "b"]}),
        ("user_ask_multi", "user", {"type": "ask", "question": "which?", "kind": "multi",
                                    "options": ["a", "b"]}),
        ("user_ask_confirm", "user", {"type": "ask", "question": "ok?", "kind": "confirm"}),
        ("user2_ask_confirm", "user2", {"type": "ask", "question": "ok?", "kind": "confirm"}),
        ("oracle_ask", "oracle", {"type": "ask", "question": "what?"}),
        ("oracle2_non_string_lines", "oracle2", {"type": "ask", "question": "what?"}),
        ("oracle_limit_empty", "oracle", {"type": "ask", "question": "what?", "limit": ""}),
    ]


def loader_cases() -> list:
    return [
        ("manifest_missing", "sload", {"type": "apply", "path": "missing.yaml"}),
        ("manifest_unreadable", "sload", {"type": "apply", "path": "unreadable.yaml"}),
        ("manifest_empty", "sload", {"type": "apply", "path": "empty.yaml"}),
        ("manifest_falsy_doc", "sload", {"type": "apply", "path": "falsy.yaml"}),
        ("manifest_agents_not_list", "sload", {"type": "apply", "path": "notlist.yaml"}),
        ("manifest_no_id", "sload", {"type": "apply", "path": "noid.yaml"}),
        ("manifest_bad_yaml", "sload", {"type": "apply", "path": "bad.yaml"}),
        ("manifest_yes_no_scalars", "sload", {"type": "apply", "path": "yesno.yaml"}),
        ("manifest_type_override", "sload", {"type": "apply", "path": "typed.yaml"}),
        ("manifest_absolute_path", "sload", {"type": "apply", "path": "/etc/hosts"}),
        ("manifest_symlinked", "sload", {"type": "apply", "path": "linked.yaml"}),
        ("manifest_ok", "sload", {"type": "apply", "path": "good.yaml"}),
        ("manifest_reapply", "sload", {"type": "apply", "path": "good.yaml"}),
        ("reflect_after", "sload", {"type": "reflect"}),
    ]


def web_ai_cases() -> list:
    return [
        ("web4_no_port_boot", "web4", {"type": "boot"}),
        ("web4_mount", "web4", {"type": "mount", "path": "/x", "target": "files"}),
        ("web4_unmount", "web4", {"type": "unmount", "path": "/x"}),
        ("web4_unknown", "web4", {"type": "nope"}),
        ("cli_intro_booting", "cli", {"type": "intro_booting"}),
        ("cli_intro_booted", "cli", {"type": "intro_booted"}),
        ("cli_say_non_string", "cli", {"type": "say", "text": {"a": 1}}),
        ("ai_garbage_type", "ai", {"type": "garbage"}),
        ("ai_shutdown", "ai", {"type": "shutdown"}),
        ("olla_garbage_type", "olla", {"type": "garbage"}),
        ("olla_shutdown", "olla", {"type": "shutdown"}),
    ]


MANIFESTS = {
    "good.yaml": "agents:\n  - id: m_ok\n    handler_module: file_bridge.tools\n",
    "empty.yaml": "",
    "falsy.yaml": "agents:\n",
    "notlist.yaml": "agents: 7\n",
    "noid.yaml": "agents:\n  - handler_module: file_bridge.tools\n",
    "bad.yaml": "agents:\n  - id: x\n   handler_module: [unclosed\n",
    "yesno.yaml": "agents:\n  - id: m_yn\n    handler_module: file_bridge.tools\n"
                  "    on: yes\n    off: no\n",
    "typed.yaml": "agents:\n  - id: m_typed\n    type: file_bridge\n"
                  "    handler_module: file_bridge.tools\n",
    "unreadable.yaml": "agents: []\n",
}


def run(ctx) -> None:
    rec, ports = ctx.rec, ctx.ports
    w = ctx.work
    (w / "sub").mkdir()
    (w / "sub" / "inner.txt").write_text("inner\n")
    (w / "hello.txt").write_text("hello bundles\n")
    (w / "links").mkdir()
    os.symlink(w / "hello.txt", w / "links" / "to_hello.txt")
    os.symlink(w / "sub", w / "links" / "to_sub")
    os.symlink("/no/such/target", w / "links" / "dangling")
    os.symlink("/", w / "symroot")
    for name, body in MANIFESTS.items():
        (w / name).write_text(body)
    os.chmod(w / "unreadable.yaml", 0o000)
    os.symlink(w / "good.yaml", w / "linked.yaml")

    interp = sys.executable
    fx = fixtures(w, interp)
    reach = [a[0] for a in fx] + ["m_ok", "m_yn", "m_typed", "web", "rest", "ws"]
    k = ctx.boot(reach=reach, port=ports.take("main"))
    try:
        cb = {**BASE, "rest": "*", "web": "*", "ws": "*", "editor": "*", "sched": "*", "runner": "*"}
        for aid, hm, meta in fx:
            rec.json(f"create/{aid}", k.create(
                **{"parent_id": k.root_id, "handler_module": hm, "id": aid,
                   **OPEN, "callable_by": cb, **meta}))
        time.sleep(1.0)

        groups = [
            ("file_bridge", file_bridge_cases(interp)),
            ("text", text_cases()),
            ("exec_bridge", exec_cases(interp)),
            ("scheduler", scheduler_cases()),
            ("static_loader", loader_cases()),
            ("probes", probe_cases()),
            ("web_cli_ai", web_ai_cases()),
        ]
        for surface, cases in groups:
            for case, target, payload in cases:
                before = len(k.http)
                reply = call(k, target, payload, timeout=45)
                rec.json(f"{surface}/{case}", {
                    "target": target, "payload": payload, "reply": reply,
                    "http": k.http[before:]})
                # Some exec cases need the child to finish before the next one
                # asks about it.
                if surface == "exec_bridge":
                    time.sleep(0.4)
        # What the verbs LEFT: the same question the reply answers, asked of disk.
        from families import disk_walk

        rec.json("disk_after", disk_walk(w))
        rec.json("kernel_list_agents_after", call(k, "kernel", {"type": "list_agents"}))
        rec.json("kernel_ps_after", call(k, "kernel", {"type": "ps"}))
    finally:
        os.chmod(w / "unreadable.yaml", 0o644)
        rec.http("http", k.http)
        k.close()
        save_kernel_log(rec, k)
