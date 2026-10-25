"""Python twins of Rust `fantastic-exec-bridge` tests (#170).

Each test is named `test_<rust_test>` after the Rust test it mirrors
(`src/lib/rust/crates/bundles/fantastic-exec-bridge/src/tests.rs`). The Rust crate
is the spec: where a twin shows Python answering differently, the note on that
test says so.
"""

from __future__ import annotations

import asyncio
import json
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

import pytest
from _testkit import LF_STDIO, child_environ, per_os
from test_exec_bridge import (
    PY,
    PY_RX,
    _inbox_ready,
    _mk,
    _start,
    _wait_done,
    _wire,
    _wire_read,
    _wire_write,
)

import exec_bridge.tools as ebt
from exec_bridge import _sys, proc
from exec_bridge._connector import CONNECTOR_SRC
from kernel._kernel import _store_safe

# `python -u -c <program>` — what the Rust suite's `py_start` runs.
ALLOW_PY = rf"{PY_RX} -u -c .*"


@pytest.fixture
def fresh_jobs(monkeypatch):
    """An empty job table. It is module-global and keyed by job id, while every
    test's kernel counts ids from the start — so a row an earlier test left behind
    can share an id with this test's job and change what a listing returns."""
    monkeypatch.setattr(ebt, "_jobs", {})


async def _ask(k, payload, target="runner"):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


def _prog(code: str, connector: bool = False) -> list[str]:
    """`-u -c` args running `code`, with the wire client prepended on request."""
    head = LF_STDIO + (CONNECTOR_SRC + "\n" if connector else "")
    return ["-u", "-c", head + code]


async def _event(q, pred, secs=15.0):
    """The first event on queue `q` that `pred` accepts — or fail."""
    end = time.monotonic() + secs
    seen = []
    while True:
        left = end - time.monotonic()
        assert left > 0, f"no matching event within {secs}s; saw {seen}"
        try:
            ev = await asyncio.wait_for(q.get(), left)
        except asyncio.TimeoutError:
            continue
        seen.append(ev)
        if pred(ev):
            return ev


async def _printed(k, jid, stream="stdout"):
    """What a finished job printed on `stream`, lines joined by newline."""
    r = await _ask(k, {"type": "tail", "job_id": jid, "stream": stream, "lines": 500})
    return "\n".join(x["line"] for x in r["lines"])


async def _stop_all(k):
    await _ask(k, {"type": "stop"})


# ─── the docs / reflect / gate ──────────────────────────────────


def test_readme_states_the_command_model():
    """The readme teaches the general-purpose COMMAND model, gated by the
    allowlist, and is honest that it is not a sandbox."""
    readme = (Path(ebt.__file__).parent / "readme.md").read_text(encoding="utf-8")
    assert "COMMAND" in readme, readme
    assert "allowlist" in readme, readme
    assert "Not a sandbox" in readme, readme


async def test_reflect_advertises_the_allowlist_not_interpreters(seeded_kernel):
    k = seeded_kernel
    await _mk(k, agent_id="rf", allowed=["ls( .*)?"])
    r = await _ask(k, {"type": "reflect"}, "rf")
    assert r["id"] == "rf", r
    assert r["sentence"] == (
        "Process edge of the io family — a general-purpose COMMAND runner, "
        "sealed by default, gated by the `allowed` regexes. Spawns commands in "
        "the background; a child may speak the kernel over a private control fd."
    ), r
    assert r["running"] == 0, r
    # THE GATE is advertised.
    assert r["allowed"] == ["ls( .*)?"], r
    assert "wire" not in r and "verbs" not in r, r


async def test_start_refuses_unlisted_command_and_sealed_leg_denies(seeded_kernel):
    k = seeded_kernel
    await _mk(k, agent_id="only_ls", allowed=["ls( .*)?"])
    r = await _start(k, "only_ls", "/bin/rm", ["-rf", "/"])
    assert "not allowed" in r["error"], r
    assert r.get("job_id") is None, r
    st = await _ask(k, {"type": "status"}, "only_ls")
    assert st["jobs"] == [], st

    # SEALED BY DEFAULT: no ingress_rule denies every owned verb but reflect.
    await _mk(k, agent_id="sealed", ingress_rule=None)
    d = await _start(k, "sealed", "ls")
    assert d["reason"] == "unauthorized", d
    assert "ingress_rule=allow_all_ingress" in d["hint"], d
    refl = await _ask(k, {"type": "reflect"}, "sealed")
    assert refl["sealed"] is True, refl
    assert isinstance(refl["allowed"], list), refl


async def test_a_typeless_rule_object_seals_like_python(seeded_kernel):
    """A rule object with no `type` is no rule at all — a plain SEAL, never an
    `{error}` without a `reason`; and the WRITE is accepted."""
    k = seeded_kernel
    r = await _mk(k, agent_id="typeless", allowed=[".*"], ingress_rule={"env": "X"})
    assert r.get("id") == "typeless" and "error" not in r, r
    denied = await _start(k, "typeless", PY, ["-c", "pass"])
    assert denied["reason"] == "unauthorized", denied
    assert denied["error"] == "edge sealed by policy", denied
    # the same through the gate directly
    gate = ebt._gate("typeless", {"type": "start"}, k)
    assert gate["reason"] == "unauthorized" and gate["error"] == "edge sealed by policy"


async def test_reflect_coerces_like_python(seeded_kernel):
    k = seeded_kernel
    await _mk(
        k,
        agent_id="co1",
        cwd="/wd",
        inherit_env=["PATH"],
        allowed=["ls", 7, True],
    )
    r = await _ask(k, {"type": "reflect"}, "co1")
    assert r["cwd"] == "/wd", r
    assert r["inherit_env"] == ["PATH"], r
    # `str(7)` and `str(True)` — not JSON's `7`/`true`.
    assert r["allowed"] == ["ls", "7", "True"], r

    await _mk(k, agent_id="co2", cwd=None, inherit_env=None)
    r2 = await _ask(k, {"type": "reflect"}, "co2")
    assert r2["cwd"] is None, r2
    assert r2["inherit_env"] == [], r2


async def test_shutdown_is_mail_not_a_verb(seeded_kernel):
    k = seeded_kernel
    # an agent of its own: the job table is module-global, and another test's
    # still-running job on a shared id would turn this into queued mail
    await _mk(k, agent_id="idle_sd", allowed=[".*"])
    r = await _ask(k, {"type": "shutdown"}, "idle_sd")
    assert (
        r["error"] == "exec_bridge: unknown type 'shutdown' (no running job to receive it)"
    ), r


async def test_refusal_texts_use_python_repr(seeded_kernel):
    k = seeded_kernel
    await _mk(k, agent_id="idle_rt", allowed=[".*"])
    # an ABSENT type is `None` — driven through the handler directly, since the
    # kernel refuses a typeless payload before any handler sees it
    r = await ebt.handler("idle_rt", {}, k)
    assert r["error"] == "exec_bridge: unknown type None (no running job to receive it)"
    r = await _ask(k, {"type": "status", "job_id": 424242}, "idle_rt")
    assert r["error"] == "exec_bridge: no job 424242", r
    assert r["reason"] == "not_found", r
    assert ebt._command_permitted({"allowed": ["ls( .*)?"]}, "rm -rf /") == (
        "exec_bridge: command not allowed: 'rm -rf /' — this leg permits: ['ls( .*)?']"
    )


def test_validate_meta_texts_match_python():
    assert ebt.validate_meta({"allowed": []}, {}) == (
        "allowed must be a non-empty list of regexes, e.g. "
        "<allowed><item>ls( .*)?</item></allowed>"
    )
    e = ebt.validate_meta({"allowed": ["("]}, {})
    assert e.startswith("allowed contains a broken regex '(': "), e
    assert ebt.validate_meta({"auth": "password"}, {}) == (
        "auth='password' is not applicable to the process edge: a leg with no wire "
        "envelope can never present a token — legal values: allow_all_ingress, "
        "deny_ingress"
    )


def test_validate_meta_refuses_only_unknown_ingress_rules():
    for silent in ("", None, {"env": "X"}):
        assert ebt.validate_meta({"ingress_rule": silent}, {}) is None, silent
    assert ebt.validate_meta({"ingress_rule": {"type": "nope"}}, {}) == (
        "ingress_rule={'type': 'nope'} is not a rule this bundle implements — "
        "legal values: allow_all_ingress, deny_ingress"
    )


def test_an_unbalanced_allowed_pattern_permits_nothing():
    rec = {"allowed": ["ls)|(?:.*"]}
    assert ebt._command_permitted(rec, "rm -rf /") is not None
    assert ebt._command_permitted(rec, "ls") is not None


async def test_unknown_type_wire_shape_is_byte_exact(seeded_kernel):
    k = seeded_kernel
    await _mk(k, agent_id="idle_ut", allowed=[".*"])
    r = await _ask(k, {"type": "frob"}, "idle_ut")
    assert json.dumps(r, separators=(",", ":"), ensure_ascii=False) == (
        '{"error":"exec_bridge: unknown type \'frob\' (no running job to receive it)",'
        '"reason":"unhandled","verb":"frob"}'
    )
    r = await ebt.handler("idle_ut", {}, k)
    assert json.dumps(r, separators=(",", ":"), ensure_ascii=False) == (
        '{"error":"exec_bridge: unknown type None (no running job to receive it)",'
        '"reason":"unhandled","verb":null}'
    )


async def test_arg_refusals_are_byte_exact_in_key_order(seeded_kernel):
    k = seeded_kernel

    def text(r):
        return json.dumps(r, separators=(",", ":"), ensure_ascii=False)

    # no PATH in the child env for a bare command
    await _mk(k, agent_id="np", allowed=[".*"], inherit_env=None)
    r = await _ask(k, {"type": "start", "command": "echo"}, "np")
    assert text(r) == (
        "{\"error\":\"exec_bridge: 'echo' is a bare command and this leg gives the "
        "child no PATH to find it in — the kernel's own PATH is not the child's\","
        '"reason":"missing_arg","hint":"update_agent np inherit_env=[\\"PATH\\"] '
        '(or env={\\"PATH\\": …}), or start an absolute path","arg":"PATH"}'
    )
    # a PTY start with no command and no SHELL in the child env
    v = proc.SHELL_VAR
    r = await _ask(k, {"type": "start", "pty": {"cols": 80, "rows": 24}}, "np")
    assert text(r) == (
        "{\"error\":\"exec_bridge: start named no command and the child's environment "
        f"has no {v} to run — a PTY start without `command` runs the shell the record "
        'passes down, never a guessed one","reason":"missing_arg",'
        f'"hint":"update_agent np inherit_env=[\\"{v}\\"] '
        f'(or env={{\\"{v}\\": …}}), or name `command`","arg":"{v}"}}'
    )
    # a leg with no cwd
    await _mk(k, agent_id="nc", allowed=[".*"], cwd=None)
    r = await _ask(
        k, {"type": "start", "command": "/bin/sh", "args": ["-c", "true"]}, "nc"
    )
    assert text(r) == (
        '{"error":"exec_bridge: this leg names no `cwd` — where a command runs is the '
        "operator's decision, never guessed\",\"reason\":\"missing_arg\","
        '"hint":"update_agent nc cwd=<dir>","arg":"cwd"}'
    )


async def test_start_coerces_args_env_and_cwd_like_python(seeded_kernel):
    k = seeded_kernel
    await _mk(k, allowed=[".*"])
    # a command of nothing but whitespace is MISSING, not a spawn of ""
    r = await _ask(k, {"type": "start", "command": "  \t "})
    assert (r["reason"], r["arg"]) == ("missing_arg", "command"), r
    # an explicit null `args` is not a list
    r = await _ask(k, {"type": "start", "command": "ls", "args": None})
    assert (r["reason"], r["arg"]) == ("bad_arg", "args"), r
    # `env` and `cwd` are the RECORD's: any per-call value refuses, even an empty
    # one; only an explicit null is "absent"
    for arg, val in (("env", {}), ("env", ["A=1"]), ("cwd", "/")):
        r = await _ask(k, {"type": "start", "command": "ls", arg: val})
        assert (r["reason"], r["arg"]) == ("bad_arg", arg), r
    r = await _ask(
        k,
        {
            "type": "start",
            "command": PY,
            "args": ["-c", "pass"],
            "env": None,
            "cwd": None,
        },
    )
    assert isinstance(r["job_id"], int), f"an explicit null is absent: {r}"
    await _wait_done(k, "runner", r["job_id"])
    # bools are ints in python, so they are legal argv items — as `True`/`False`
    prog = "import sys; print(sys.argv[1:])"
    r = await _ask(
        k, {"type": "start", "command": PY, "args": ["-c", prog, True, 2, 1.5]}
    )
    assert isinstance(r["job_id"], int), r
    st = await _ask(k, {"type": "status", "job_id": r["job_id"]})
    assert st["jobs"][0]["args"] == ["-c", prog, "True", "2", "1.5"], st
    await _wait_done(k, "runner", r["job_id"])


# ─── the job lifecycle ──────────────────────────────────────────


async def test_interrupt_sends_sigint_to_the_job(seeded_kernel):
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY])
    inbox = k.ctx.ensure_inbox("runner")
    code = (
        "import signal,time,sys\n"
        "def h(s,f):\n"
        "    print('CAUGHT'); sys.stdout.flush(); sys.exit(0)\n"
        "signal.signal(getattr(signal, 'SIGBREAK', signal.SIGINT), h)\n"
        "print('READY', flush=True)\n"
        "for _ in range(300):\n"
        "    time.sleep(0.1)\n"
    )
    r = await _start(k, "runner", PY, _prog(code))
    # the handler is installed once the program says so
    await _event(inbox, lambda e: e.get("type") == "progress" and e["line"] == "READY")
    n = await _ask(k, {"type": "interrupt"})
    assert n["interrupted"] >= 1, n
    done = await _event(inbox, lambda e: e.get("type") == "job_done")
    assert done["job_id"] == r["job_id"], done
    assert "CAUGHT" in await _printed(k, r["job_id"]), done


def test_the_child_self_reaps_when_the_control_socket_closes():
    """The connector's lifeline: when the parent drops the control socket the child
    reaps ITSELF (`_o._exit(0)`), even parked forever."""
    code = CONNECTOR_SRC + "\nimport threading\nthreading.Event().wait()\n"
    parent, child_end = socket.socketpair()
    p = _sys.popen_inheriting(
        [sys.executable, "-u", "-c", code],
        child_end,
        env=child_environ(FANTASTIC_CTRL_FD=str(child_end.fileno())),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    child_end.close()
    try:
        time.sleep(0.4)
        assert p.poll() is None, "child exited early"
        parent.close()  # the only thing holding the socket open
        assert p.wait(timeout=10) == 0, "self-reap should exit 0"
    finally:
        if p.poll() is None:
            p.kill()
        p.communicate()


async def test_start_returns_a_job_id_immediately_and_records_the_invocation(
    seeded_kernel, fresh_jobs
):
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY])
    t0 = time.monotonic()
    r = await _start(k, "runner", PY, _prog("import time; time.sleep(30)"))
    assert isinstance(r["job_id"], int), r
    assert r["status"] == "running", r
    assert time.monotonic() - t0 < 5, "start blocked"
    st = await _ask(k, {"type": "status"})
    assert st["jobs"][0]["command"] == PY, st
    assert st["jobs"][0]["args"][0] == "-u", st
    await _stop_all(k)
    await _wait_done(k, "runner", r["job_id"])


async def test_progress_streams_per_line_with_a_shared_counter(seeded_kernel):
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY])
    inbox = k.ctx.ensure_inbox("runner")
    code = "import sys\nprint('o1')\nprint('e1', file=sys.stderr)\nprint('o2')\n"
    r = await _start(k, "runner", PY, _prog(code))
    jid = r["job_id"]
    events = []
    done = await _event(inbox, lambda e: events.append(e) or e["type"] == "job_done")
    assert done["ok"] is True, done
    prog = [e for e in events if e.get("type") == "progress"]
    assert len(prog) == 3, prog
    for e in prog:
        assert set(e) == {"type", "job_id", "n", "line", "stream"}, e
        assert e["job_id"] == jid, e
    # `n` is ONE sequence across stdout AND stderr
    assert sorted(e["n"] for e in prog) == [1, 2, 3], prog
    # order holds within a stream only (the pipes are read concurrently)
    assert [e["line"] for e in prog if e["stream"] == "stdout"] == ["o1", "o2"], prog
    assert [e["line"] for e in prog if e["stream"] == "stderr"] == ["e1"], prog


_PING_PONG = (
    "import threading, time\n"
    "time.sleep(2.0)\n"
    "seen = []\n"
    "def _on(m):\n"
    "    if m.get('type') == 'ping':\n"
    "        seen.append(1)\n"
    "        kernel.emit({'type': 'pong', 'count': len(seen)})\n"
    "kernel.on_message(_on)\n"
    "threading.Event().wait()\n"
)


async def test_mail_sent_before_on_message_registers_is_not_delivered_and_the_ack_does_not_prove_delivery(
    seeded_kernel,
):
    """Mail sent before the program registered its `on_message` is ACKed
    `{queued:true}` and dropped: no `_xbself_` watcher exists yet."""
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY])
    inbox = k.ctx.ensure_inbox("runner")
    await _start(k, "runner", PY, _prog(_PING_PONG, connector=True))
    # sent at once: the program is still in its startup sleep
    ack = await _ask(k, {"type": "ping"})
    assert ack == {"queued": True}, ack
    ps = await k.send([{"target": "kernel", "payload": {"type": "ps"}}])
    rows = ps[0]["result"]["tasks"]
    assert not any(t["verb"] == "inbox_drain" for t in rows), (
        f"the drain registered too early for this test: {rows}"
    )
    # …and it is never delivered once on_message registers
    await _inbox_ready(k, "runner")
    await asyncio.sleep(1.0)
    got = []
    while not inbox.empty():
        got.append(inbox.get_nowait())
    assert not [e for e in got if e.get("type") == "pong"], got
    await _stop_all(k)


async def test_the_program_is_gated_as_its_runner_not_as_the_operator(seeded_kernel):
    """A program's sends carry the RUNNER's identity and are gated by its grants:
    a sibling the runner holds no `can_call` for refuses (`unauthorized`)."""
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY], can_call={"friend": ["reflect"]})
    await _mk(
        k, agent_id="friend", allowed=["x"], callable_by={"runner": ["reflect"]}
    )
    await _mk(k, agent_id="sib", allowed=["x"])  # no edge from runner
    code = (
        "r = kernel.send('sib', {'type': 'reflect'})\n"
        "print('SIB', r.get('reason'))\n"
        "r2 = kernel.send('friend', {'type': 'reflect'})\n"
        "print('FRIEND', r2.get('id'))\n"
    )
    r = await _start(k, "runner", PY, _prog(code, connector=True))
    row = await _wait_done(k, "runner", r["job_id"], secs=20)
    assert row["exit_code"] == 0, row
    out = await _printed(k, r["job_id"])
    assert "SIB unauthorized" in out, out
    assert "FRIEND friend" in out, out


async def _fb_runner(k, tmp_path, monkeypatch, root):
    """A real file_bridge `fb` rooted at `root`, callable by an open `runner`."""
    monkeypatch.chdir(tmp_path)
    fb = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "id": "fb",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "ingress_rule": "allow_all_ingress",
                        "root": root,
                        "callable_by": {"runner": "*"},
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    await _mk(k, allowed=[ALLOW_PY], can_call={fb: "*"})
    return fb


async def test_a_text_sends_bytes_reply_comes_back_raw(
    seeded_kernel, tmp_path, monkeypatch
):
    """A TEXT send (kind T, no bytes in the call) whose reply holds bytes comes back
    kind B: the program needs no `bytes: b''` to get its chunk raw."""
    k = seeded_kernel
    await _fb_runner(k, tmp_path, monkeypatch, "sd")
    code = (
        "raw = bytes(range(256))\n"
        "kernel.send('fb', {'type': 'write_stream', 'path': 'b.bin', 'bytes': raw, "
        "'truncate': True})\n"
        "r = kernel.send('fb', {'type': 'read_stream', 'path': 'b.bin'})\n"
        "print('READ', r.get('error'), type(r.get('bytes')).__name__, "
        "r.get('bytes') == raw)\n"
    )
    r = await _start(k, "runner", PY, _prog(code, connector=True))
    row = await _wait_done(k, "runner", r["job_id"], secs=20)
    assert row["exit_code"] == 0, row
    assert "READ None bytes True" in await _printed(k, r["job_id"]), row


async def test_a_text_read_stream_is_answered_by_one_binary_frame(
    seeded_kernel, tmp_path, monkeypatch
):
    """The same reply AS IT CROSSES the fd, from the real `file_bridge`: a kind-T
    `send` of `read_stream` is answered by ONE kind-B frame whose `_binary_path` is
    `["data", "bytes"]`, the header holding null in that slot, the raw body beside."""
    k = seeded_kernel
    (tmp_path / "served").mkdir()
    raw = bytes(range(256))
    (tmp_path / "served" / "blob.bin").write_bytes(raw)
    await _fb_runner(k, tmp_path, monkeypatch, "served")
    child, task = await _wire(k, job_id="jb")
    await _wire_write(
        child,
        {
            "op": "send",
            "rid": 7,
            "target": "fb",
            "payload": {"type": "read_stream", "path": "blob.bin"},
        },
    )
    loop = asyncio.get_running_loop()

    async def exactly(n):
        buf = b""
        while len(buf) < n:
            c = await asyncio.wait_for(loop.sock_recv(child, n - len(buf)), 5)
            assert c, "the relay closed the socket"
            buf += c
        return buf

    (n,) = struct.unpack(">I", await exactly(4))
    body = await exactly(n)
    assert body[:1] == b"B", "a bytes reply must cross as a binary frame"
    (hlen,) = struct.unpack(">I", body[1:5])
    head = json.loads(body[5 : 5 + hlen])
    assert head["_binary_path"] == ["data", "bytes"], head
    assert body[5 + hlen :] == raw
    assert head["op"] == "reply" and head["rid"] == 7, head
    assert head["data"]["bytes"] is None, head
    assert head["data"]["eof"] is True and head["data"]["size"] == 256, head
    # ONE frame: nothing else follows
    try:
        extra = await asyncio.wait_for(loop.sock_recv(child, 1), 0.3)
    except asyncio.TimeoutError:
        extra = None
    assert extra is None, f"a second frame followed: {extra!r}"
    child.close()
    await asyncio.wait_for(task, 5)


# ─── status / output / exit codes / env ─────────────────────────


async def test_status_lists_jobs_in_start_order(seeded_kernel, fresh_jobs):
    k = seeded_kernel
    # its own agent: the job table is module-global, so a shared id would list
    # other tests' leftover rows
    await _mk(k, agent_id="ordered", allowed=[ALLOW_PY])
    started = []
    for _ in range(3):
        r = await _start(k, "ordered", PY, _prog("import time; time.sleep(30)"))
        started.append(r["job_id"])
    st = await _ask(k, {"type": "status"}, "ordered")
    assert [j["job_id"] for j in st["jobs"]] == started, st
    await _ask(k, {"type": "stop"}, "ordered")
    for jid in started:
        await _wait_done(k, "ordered", jid)


async def test_output_is_collected_raw_like_python(seeded_kernel):
    """An invalid byte is REPLACED while the pipe keeps draining, and a final line
    with no newline gets none added.

    NOTE (unresolved CR rule): Rust keeps a lone `\\r` in the stored line; Python
    drops a trailing `\\r` (`test_crlf_output.py`, #205). The CR is tolerated either
    way here until that rule is decided."""
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY])
    prog = "import sys\nsys.stdout.buffer.write(b'a\\r\\nb\\xff')\n"
    r = await _start(k, "runner", PY, ["-u", "-c", prog])
    row = await _wait_done(k, "runner", r["job_id"])
    assert row["exit_code"] == 0, row  # the pipe kept draining past the bad byte
    assert row["lines"] == 2, row
    assert row["last_line"] == "b�", row  # replaced, and no newline added
    t = await _ask(k, {"type": "tail", "job_id": r["job_id"]})
    lines = [x["line"] for x in t["lines"]]
    assert [ln.rstrip("\r") for ln in lines] == ["a", "b�"], t


async def test_a_killed_job_reports_negative_signal_exit_code(seeded_kernel):
    """A signal death reports `exit_code = -signal` (Windows: the code its Job
    Object was terminated with), `ok:false` — never None."""
    k = seeded_kernel
    await _mk(k, allowed=[ALLOW_PY])
    inbox = k.ctx.ensure_inbox("runner")
    code = "import time\nprint('UP', flush=True)\ntime.sleep(30)\n"
    r = await _start(k, "runner", PY, _prog(code))
    await _event(inbox, lambda e: e.get("type") == "progress" and e["line"] == "UP")
    await _ask(k, {"type": "stop"})
    done = await _event(inbox, lambda e: e.get("type") == "job_done")
    killed = per_os(posix=-9, nt=1)
    assert done["exit_code"] == killed, done
    assert done["ok"] is False, done
    row = await _wait_done(k, "runner", r["job_id"])
    assert row["exit_code"] == killed, row


async def test_caller_env_cannot_override_the_control_vars(seeded_kernel):
    """The two control vars are written OVER the record's `env`, so a program
    cannot claim another agent's identity or point the control fd elsewhere."""
    k = seeded_kernel
    await _mk(
        k,
        allowed=[ALLOW_PY],
        env={"FANTASTIC_SELF_ID": "root", "FANTASTIC_CTRL_FD": "0", "MINE": "kept"},
    )
    code = (
        "import os\n"
        "print('ID', os.environ['FANTASTIC_SELF_ID'], 'FD', "
        "os.environ['FANTASTIC_CTRL_FD'], 'MINE', os.environ['MINE'])\n"
    )
    r = await _start(k, "runner", PY, _prog(code))
    await _wait_done(k, "runner", r["job_id"])
    out = await _printed(k, r["job_id"])
    toks = out.split()
    assert toks[0:2] == ["ID", "runner"], out
    assert toks[2] == "FD" and toks[3].isdigit() and int(toks[3]) > 2, out
    assert toks[4:] == ["MINE", "kept"], out


# ─── the relay ──────────────────────────────────────────────────


async def test_relay_coerces_falsy_payload_and_target(seeded_kernel):
    """A FALSY payload becomes `{}` and an emit lands on the OWNER, whatever its
    `target` says."""
    k = seeded_kernel
    await _mk(k, allowed=["x"])
    owner = k.ctx.ensure_inbox("runner")
    child, task = await _wire(k, job_id="jf")
    await _wire_write(
        child, {"op": "send", "rid": 1, "target": "runner", "payload": None}
    )
    reply = await _wire_read(child)
    assert reply["op"] == "reply", reply
    # `payload.type required` (not `payload required`) proves null became `{}`
    assert reply["data"]["arg"] == "payload.type", reply

    await _wire_write(child, {"op": "emit"})
    ev = await _event(owner, lambda e: True, 5)
    assert ev == {}, f"a falsy payload must become {{}}: {ev}"
    await _wire_write(
        child,
        {"op": "emit", "target": "elsewhere", "payload": {"type": "ping"}},
    )
    ev = await _event(owner, lambda e: e.get("type") == "ping", 5)
    assert ev["type"] == "ping"
    child.close()
    await asyncio.wait_for(task, 5)


async def test_the_relay_forwards_a_send_untouched_and_a_binary_one_through_the_batch(
    seeded_kernel,
):
    from io_bridge import encode_frame

    k = seeded_kernel
    await _mk(k, allowed=["x"])
    k.ctx.ensure_inbox("tap_relay")
    assert k.watch("runner", "tap_relay") is None
    tap = k.ctx.inboxes["tap_relay"]
    child, task = await _wire(k, job_id="jt")

    # a program calling its OWN runner — a "cycle" by the call-stack guard — is
    # relayed like any send
    await _wire_write(
        child,
        {"op": "send", "rid": 1, "target": "runner", "payload": {"type": "status"}},
    )
    reply = await _wire_read(child)
    assert reply["rid"] == 1, reply
    assert "error" not in reply["data"], reply
    mirrored = tap.get_nowait()
    assert mirrored == {"type": "status"}, f"relayed untouched: {mirrored}"

    # kind B: `[len | 'B' | io_bridge binary frame]`
    head = {
        "op": "send",
        "rid": 2,
        "target": "runner",
        "payload": {"type": "status", "bytes": b"xyz"},
    }
    wire, binary = encode_frame(head)
    assert binary
    body = b"B" + wire
    loop = asyncio.get_running_loop()
    await loop.sock_sendall(child, struct.pack(">I", len(body)) + body)
    reply = await _wire_read(child)
    assert reply["rid"] == 2, reply
    calls = [
        r
        for r in k.ctx.rows.values()
        if r.get("kind") == "call"
        and (r.get("payload") or {}).get("type") == "status"
        and "bytes" in r["payload"]
    ]
    assert calls, "the binary send is a ledgered call"
    # the live row holds the body; what the store keeps is the `{len: N}` reduction
    assert _store_safe(calls[-1])["payload"]["bytes"] == {"len": 3}, calls[-1]
    child.close()
    await asyncio.wait_for(task, 5)
