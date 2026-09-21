"""exec_bridge — the general-purpose COMMAND runner.

One suite for the whole surface: the regex gate, the write-time refusals, the
job lifecycle (start/status/stop/clear), the task-table row, the OS reap, and
the control-wire mail path. The predecessor suites (1,800 lines) tested a
deleted design — inline `code`, an interpreter allowlist, shims — and went with
it; this bundle runs COMMANDS, and its one gate is `allowed` (regexes
full-matched against the shell-joined command line).
"""

from __future__ import annotations

import asyncio
import json
import socket
import struct
import sys


async def _mk(kernel, agent_id="runner", **extra):
    meta = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "exec_bridge.tools",
        "ingress_rule": "allow_all_ingress",
        **extra,
    }
    r = (await kernel.send([{"target": kernel.id, "payload": meta}]))[0]["result"]
    return r


async def _start(kernel, agent_id, command, args=None, **kw):
    return (
        await kernel.send(
            [
                {
                    "target": agent_id,
                    "payload": {
                        "type": "start",
                        "command": command,
                        "args": args or [],
                        **kw,
                    },
                }
            ]
        )
    )[0]["result"]


async def _wait_done(kernel, agent_id, job_id, secs=5.0):
    """Poll status until the job leaves `running` — event-driven tests poll the
    THING, not a sleep guess."""
    for _ in range(int(secs / 0.05)):
        st = (
            await kernel.send(
                [{"target": agent_id, "payload": {"type": "status", "job_id": job_id}}]
            )
        )[0]["result"]
        row = st["jobs"][0]
        if row["status"] != "running":
            return row
        await asyncio.sleep(0.05)
    raise AssertionError(f"job {job_id} still running after {secs}s")


async def _until(pred, secs=5.0, what="condition"):
    """Poll the THING, never a sleep guess."""
    for _ in range(int(secs / 0.02)):
        if pred():
            return
        await asyncio.sleep(0.02)
    raise AssertionError(f"{what} never held within {secs}s")


# ─── speaking the control wire directly ─────────────────────────
#
# The relay is the unit under test in a few of these, so they drive it over a
# socketpair the way a spawned program would — no subprocess, no timing race
# between the child registering a callback and the frame coming back.


async def _wire(kernel, owner="runner", job_id="j1"):
    """Start a relay on one end of a socketpair; return the PROGRAM's end + task.

    The sender contextvar is set around `create_task` because that is exactly how
    the real relay inherits it from `_start`'s dispatch — the program's frames are
    gated as the RUNNER, not as the operator."""
    import exec_bridge.tools as ebt
    from kernel._agent import _current_sender

    parent, child = socket.socketpair()
    child.setblocking(False)
    tok = _current_sender.set(owner)
    try:
        task = asyncio.create_task(ebt._serve_ctrl(parent, owner, job_id, kernel))
    finally:
        _current_sender.reset(tok)
    return child, task


async def _wire_write(child, obj):
    body = b"T" + json.dumps(obj).encode()
    loop = asyncio.get_running_loop()
    await loop.sock_sendall(child, struct.pack(">I", len(body)) + body)


async def _wire_read(child, secs=5.0):
    loop = asyncio.get_running_loop()

    async def exactly(n):
        buf = b""
        while len(buf) < n:
            c = await loop.sock_recv(child, n - len(buf))
            if not c:
                raise AssertionError("the relay closed the socket")
            buf += c
        return buf

    hdr = await asyncio.wait_for(exactly(4), secs)
    (n,) = struct.unpack(">I", hdr)
    return json.loads((await asyncio.wait_for(exactly(n), secs))[1:])


# ─── the gate ───────────────────────────────────────────────────


async def test_no_allowlist_runs_nothing(seeded_kernel):
    await _mk(seeded_kernel)
    r = await _start(seeded_kernel, "runner", "echo", ["hi"])
    assert "no commands are allowed" in r["error"]
    # A POLICY wall, not a malformed argument — the model must be able to tell
    # a typo from "the operator has not allowed this".
    assert r["reason"] == "unauthorized", r
    assert "allowed" in r["hint"], r


async def test_unmatched_command_refused_naming_the_patterns(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    r = await _start(seeded_kernel, "runner", "cat", ["/etc/passwd"])
    assert "not allowed" in r["error"] and "echo( .*)?" in r["error"]


async def test_match_is_anchored_not_a_prefix(seeded_kernel):
    """`echo.*` must not smuggle a second command past the gate — the line is
    shell-joined and FULL-matched, so the metachar arrives quoted and the
    pattern sees one argv, not a shell string."""
    await _mk(seeded_kernel, allowed=[r"echo [a-z ]*"])
    r = await _start(seeded_kernel, "runner", "echo", ["hi; rm -rf /"])
    assert "not allowed" in r["error"]


async def test_every_start_refusal_carries_a_code(seeded_kernel):
    """Sibling argument errors used to split — `args`/`env` answered `bad_arg`
    while `command`/`code`/the policy wall answered a bare `{error}`, so nothing
    could branch on the failure without matching prose."""
    await _mk(seeded_kernel, agent_id="codes", allowed=[r"echo( .*)?"])

    async def start(**payload):
        return (
            await seeded_kernel.send(
                [{"target": "codes", "payload": {"type": "start", **payload}}]
            )
        )[0]["result"]

    r = await start()
    assert (r["reason"], r["arg"]) == ("missing_arg", "command"), r
    r = await start(command="echo", code="x = 1")
    assert (r["reason"], r["arg"]) == ("bad_arg", "code"), r
    r = await start(command="echo", args=[{"nope": 1}])
    assert (r["reason"], r["arg"]) == ("bad_arg", "args"), r
    r = await start(command="echo", env={"K": ["nope"]})
    assert (r["reason"], r["arg"]) == ("bad_arg", "env"), r
    r = await start(command="rm", args=["-rf", "/"])
    assert r["reason"] == "unauthorized", r


async def test_an_unknown_job_id_is_not_found_on_every_verb(seeded_kernel):
    await _mk(seeded_kernel, agent_id="nf", allowed=[r"echo( .*)?"])
    for verb in ("status", "stop", "interrupt"):
        r = (
            await seeded_kernel.send(
                [{"target": "nf", "payload": {"type": verb, "job_id": "deadbeef"}}]
            )
        )[0]["result"]
        assert r["reason"] == "not_found", (verb, r)
        assert r["job_id"] == "deadbeef", (verb, r)


async def test_write_time_refusal_for_broken_regex(seeded_kernel):
    r = await _mk(seeded_kernel, agent_id="r2", allowed=["("])
    assert "broken regex" in r["error"]


async def test_write_time_refusal_for_dead_fields(seeded_kernel):
    for field in ("code", "shim", "interpreter", "interpreters", "argv_template"):
        r = await _mk(seeded_kernel, agent_id=f"r_{field}", **{field: "x"})
        assert "is not a field" in r["error"], (field, r)


# ─── the job lifecycle ──────────────────────────────────────────


async def test_command_runs_and_captures_output(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    r = await _start(seeded_kernel, "runner", "echo", ["hello", "world"])
    assert r.get("status") == "running", r
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["exit_code"] == 0
    assert row["last_line"] == "hello world"


async def test_nonzero_exit_is_failed(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r".*"])
    r = await _start(seeded_kernel, "runner", "false")
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] == "failed" and row["exit_code"] != 0


async def test_stop_kills_a_running_job(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    s = (
        await seeded_kernel.send(
            [{"target": "runner", "payload": {"type": "stop", "job_id": r["job_id"]}}]
        )
    )[0]["result"]
    assert s.get("killed"), s
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] != "running"


async def test_env_reaches_the_child(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"/bin/sh -c .*"])
    r = await _start(
        seeded_kernel, "runner", "/bin/sh", ["-c", "echo $MARK"], env={"MARK": "x42"}
    )
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["last_line"] == "x42"


async def test_many_jobs_run_in_parallel(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    ids = []
    for i in range(3):
        r = await _start(seeded_kernel, "runner", "echo", [str(i)])
        ids.append(r["job_id"])
    for i, j in enumerate(ids):
        row = await _wait_done(seeded_kernel, "runner", j)
        assert row["exit_code"] == 0 and row["last_line"] == str(i)


# ─── kernel integration ─────────────────────────────────────────


async def test_a_job_holds_a_task_row_and_cancel_reaps_it(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    st = (
        await seeded_kernel.send(
            [{"target": "runner", "payload": {"type": "status", "job_id": r["job_id"]}}]
        )
    )[0]["result"]
    task_id = st["jobs"][0]["task_id"]
    assert task_id
    c = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": task_id}}]
        )
    )[0]["result"]
    assert c.get("cancelled"), c
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] != "running"


async def test_delete_agent_reaps_running_jobs(seeded_kernel):
    import exec_bridge.tools as ebt

    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    proc = ebt._jobs[r["job_id"]]["proc"]
    await seeded_kernel.send(
        [
            {
                "target": seeded_kernel.id,
                "payload": {"type": "delete_agent", "id": "runner"},
            }
        ]
    )
    for _ in range(60):
        if proc.returncode is not None:
            break
        await asyncio.sleep(0.05)
    assert proc.returncode is not None, "OS subprocess survived delete_agent"


# ─── the control wire ───────────────────────────────────────────

_WIRE_CHILD = (
    "import os,socket,struct,json,sys\n"
    "fd=int(os.environ['FANTASTIC_CTRL_FD'])\n"
    "s=socket.socket(fileno=fd)\n"
    "def frame(o):\n"
    "  b=b'T'+json.dumps(o).encode(); s.sendall(struct.pack('>I',len(b))+b)\n"
    "frame({'op':'onmessage'})\n"
    "hdr=s.recv(4); n=struct.unpack('>I',hdr)[0]; m=json.loads(s.recv(n)[1:])\n"
    "print('got', m['payload']['type']); sys.stdout.flush()\n"
)


async def test_mail_reaches_a_live_job_over_the_wire(seeded_kernel):
    """A command may still speak the control wire — the fd survives the redesign.
    The child here is `python -c <program>`: just a COMMAND with args, which is
    the whole point of the general-purpose runner."""
    py = sys.executable
    await _mk(seeded_kernel, allowed=[rf"{py} -c .*"])
    r = await _start(seeded_kernel, "runner", py, ["-c", _WIRE_CHILD])
    await asyncio.sleep(0.4)  # let it register on_message
    ack = (
        await seeded_kernel.send([{"target": "runner", "payload": {"type": "ping"}}])
    )[0]["result"]
    assert ack == {"queued": True}, ack
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert "got ping" in (row.get("last_line") or "")


async def test_mail_with_no_live_job_is_a_loud_error(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    r = (await seeded_kernel.send([{"target": "runner", "payload": {"type": "ping"}}]))[
        0
    ]["result"]
    assert "error" in r


async def test_connector_verb_hands_over_the_reference_client(seeded_kernel):
    """A model pastes the client instead of re-deriving the socket framing
    (measured 2026-09-01, E30: ten messages lost to int(fd)/buffering/not-seekable).
    Ungated like reflect — discovery works on a sealed leg."""
    await _mk(seeded_kernel, "runner_c", allowed=["x"])
    r = (
        await seeded_kernel.send(
            [{"target": "runner_c", "payload": {"type": "connector"}}]
        )
    )[0]["result"]
    assert "FANTASTIC_CTRL_FD" in r["source"] and "def send(" in r["source"]
    assert "can_call" in r["howto"]
    # sealed leg (no ingress_rule) still answers — it is discovery
    await _mk(seeded_kernel, "sealed_c", ingress_rule=None)
    r2 = (
        await seeded_kernel.send(
            [{"target": "sealed_c", "payload": {"type": "connector"}}]
        )
    )[0]["result"]
    assert "source" in r2, r2


# ─── raw bytes on the wire ──────────────────────────────────────


_STREAM_CHILD = (
    "import os\n"
    "fb = os.environ['FB']\n"
    "raw = bytes(range(256)) * 3\n"
    "w = kernel.send(fb, {'type': 'write_stream', 'path': 'blob.bin', 'bytes': raw, "
    "'truncate': True})\n"
    "r = kernel.send(fb, {'type': 'read_stream', 'path': 'blob.bin', 'bytes': b''})\n"
    "print('WROTE', w['written'], 'READ', len(r['bytes']), r['bytes'] == raw, "
    "type(r['bytes']).__name__)\n"
)


async def test_a_stream_chunk_crosses_the_wire_raw(
    seeded_kernel, tmp_path, monkeypatch
):
    """`write_stream`/`read_stream` from a program: the bytes ride the fd as the
    io_bridge binary frame (kind B), never base64 — and never None. A `read_stream`
    reply was unserializable on this wire and reached the program as None
    (measured 2026-09-02, E30 subai:python: 50 messages lost to a correct worker)."""
    from exec_bridge._connector import CONNECTOR_SRC

    monkeypatch.chdir(tmp_path)
    py = sys.executable
    fb = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": "fb",
                        "handler_module": "file_bridge.tools",
                        "ingress_rule": "allow_all_ingress",
                        "root": "sd",
                        "callable_by": {"runner": "*"},
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    await _mk(seeded_kernel, allowed=[rf"{py} -c .*"], can_call={fb: "*"})
    r = await _start(
        seeded_kernel,
        "runner",
        py,
        ["-c", CONNECTOR_SRC + _STREAM_CHILD],
        env={"FB": fb},
    )
    row = await _wait_done(seeded_kernel, "runner", r["job_id"], secs=15.0)
    assert row["exit_code"] == 0, row
    assert row["last_line"] == "WROTE 768 READ 768 True bytes", row
    assert (tmp_path / "sd" / "blob.bin").read_bytes() == bytes(range(256)) * 3


def test_the_connector_raises_when_the_relay_cannot_deliver():
    """A relay-level failure frame (`{op:reply, rid, error}`) surfaces as an
    exception in the program — never a None reply the author cannot diagnose."""
    import json
    import socket
    import struct
    import subprocess

    from exec_bridge._connector import CONNECTOR_SRC

    code = CONNECTOR_SRC + (
        "try:\n"
        "    kernel.send('x', {'type': 'y'})\n"
        "    print('RETURNED')\n"
        "except RuntimeError as e:\n"
        "    print('RAISED', e)\n"
    )
    parent, child_end = socket.socketpair()
    proc = subprocess.Popen(
        [sys.executable, "-u", "-c", code],
        env={"FANTASTIC_CTRL_FD": str(child_end.fileno()), "PATH": "/usr/bin:/bin"},
        pass_fds=[child_end.fileno()],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    child_end.close()
    parent.settimeout(10)
    (n,) = struct.unpack(">I", parent.recv(4))
    body = b""
    while len(body) < n:
        body += parent.recv(n - len(body))
    assert body[:1] == b"T", body[:8]
    m = json.loads(body[1:])
    assert m["op"] == "send" and m["target"] == "x"
    reply = (
        b"T" + json.dumps({"op": "reply", "rid": m["rid"], "error": "boom"}).encode()
    )
    parent.sendall(struct.pack(">I", len(reply)) + reply)
    out, err = proc.communicate(timeout=10)
    parent.close()
    assert b"RAISED fantastic: send('x') failed in transit: boom" in out, (out, err)


# ─── the relay: denials, leaks, the frame bound ─────────────────


async def test_a_denied_watch_reaches_the_program_and_registers_nothing(seeded_kernel):
    """`kernel.watch` is gated like a send and can REFUSE. The refusal used to be
    discarded: the sub was registered anyway and the program saw a subscription
    that simply never fired — deaf, with nothing to diagnose."""
    await _mk(seeded_kernel, allowed=["x"])
    await _mk(seeded_kernel, agent_id="secret", allowed=["x"])  # no edge from runner
    child, task = await _wire(seeded_kernel)

    await _wire_write(child, {"op": "watch", "src": "secret"})
    ev = await _wire_read(child)
    assert ev["op"] == "event" and ev["src"] == "secret", ev
    assert ev["payload"]["reason"] == "unauthorized", ev

    assert "_xbw_j1_secret" not in seeded_kernel.ctx.inboxes
    assert not seeded_kernel.ctx.agents["secret"]._watcher_ids

    child.close()
    await asyncio.wait_for(task, 5)


async def test_an_oversized_frame_closes_the_control_socket(seeded_kernel):
    """A length prefix over the bound is a WIRE VIOLATION — the framing itself is
    unusable, so there is nothing to reply TO. Close, as every runtime does."""
    import exec_bridge.tools as ebt

    await _mk(seeded_kernel, allowed=["x"])
    child, task = await _wire(seeded_kernel, job_id="j_big")
    loop = asyncio.get_running_loop()
    await loop.sock_sendall(child, struct.pack(">I", ebt.MAX_FRAME + 1))
    assert await asyncio.wait_for(loop.sock_recv(child, 1), 5) == b""
    await asyncio.wait_for(task, 5)
    child.close()


def test_the_connector_refuses_to_send_an_oversized_frame():
    """The other end of the same bound: the reference client raises where the
    author has a stack, instead of writing a frame the relay closes the socket
    over — which the connector's own reader reads as `the kernel is gone`."""
    import subprocess

    from exec_bridge._connector import CONNECTOR_SRC

    code = CONNECTOR_SRC + (
        "try:\n"
        "    kernel.emit({'blob': 'x' * (17 * 1024 * 1024)})\n"
        "    print('SENT')\n"
        "except ValueError as e:\n"
        "    print('REFUSED', e)\n"
    )
    parent, child_end = socket.socketpair()
    proc = subprocess.Popen(
        [sys.executable, "-u", "-c", code],
        env={"FANTASTIC_CTRL_FD": str(child_end.fileno()), "PATH": "/usr/bin:/bin"},
        pass_fds=[child_end.fileno()],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    child_end.close()
    out, err = proc.communicate(timeout=30)
    parent.close()
    assert b"REFUSED fantastic: frame exceeds 16 MiB" in out, (out, err)
    assert b"write_stream" in out, (out, err)


_WATCHER_CHILD = (
    "import threading, sys\n"
    "kernel.watch(kernel.self_id, lambda p: None)\n"
    "kernel.on_message(lambda p: None)\n"
    "print('READY'); sys.stdout.flush()\n"
    "threading.Event().wait()\n"
)


async def test_stopping_a_watching_program_leaves_no_proxy_or_watcher(seeded_kernel):
    """Every proxy the relay held is given back on the way out — the watcher row,
    the INBOX row, and the drain task. A leaked inbox row makes `emit` auto-vivify
    into a Queue nobody drains, one per (job, watched src), forever."""
    from exec_bridge._connector import CONNECTOR_SRC

    py = sys.executable
    await _mk(seeded_kernel, allowed=[rf"{py} -u -c .*"])
    r = await _start(
        seeded_kernel, "runner", py, ["-u", "-c", CONNECTOR_SRC + _WATCHER_CHILD]
    )
    boxes = seeded_kernel.ctx.inboxes
    proxies = ("_xbw_%s_runner" % r["job_id"], "_xbself_%s" % r["job_id"])
    await _until(
        lambda: all(p in boxes for p in proxies), what="both proxies registered"
    )
    assert seeded_kernel.ctx.agents["runner"]._watcher_ids >= set(proxies)

    await seeded_kernel.send([{"target": "runner", "payload": {"type": "stop"}}])
    await _wait_done(seeded_kernel, "runner", r["job_id"])
    await _until(
        lambda: not any(p in boxes for p in proxies), what="both proxies released"
    )
    assert not (seeded_kernel.ctx.agents["runner"]._watcher_ids & set(proxies))


async def test_a_cancelled_job_finalises_to_stopped_and_clear_removes_it(seeded_kernel):
    """A cancelled job's row must not sit at `stopping` forever: `clear` refuses
    that label by design, so an immortal row would be unforgettable."""
    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    st = (
        await seeded_kernel.send(
            [{"target": "runner", "payload": {"type": "status", "job_id": r["job_id"]}}]
        )
    )[0]["result"]
    await seeded_kernel.send(
        [
            {
                "target": "kernel",
                "payload": {"type": "cancel", "task_id": st["jobs"][0]["task_id"]},
            }
        ]
    )
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] == "stopped", row
    c = (
        await seeded_kernel.send([{"target": "runner", "payload": {"type": "clear"}}])
    )[0]["result"]
    assert c["cleared"] >= 1, c
    left = (
        await seeded_kernel.send([{"target": "runner", "payload": {"type": "status"}}])
    )[0]["result"]
    assert left["jobs"] == [], left


async def test_the_connector_has_no_args(seeded_kernel):
    """`kernel.args` / FANTASTIC_ARGS was advertised by every runtime's howto and
    set by no spawner — a program written from the docs read `{}` and KeyError'd.
    A command runner takes its arguments as ARGV."""
    import re

    from exec_bridge._connector import CONNECTOR_SRC

    assert "FANTASTIC_ARGS" not in CONNECTOR_SRC
    assert not re.search(r"^\s+args\s*=", CONNECTOR_SRC, re.M), CONNECTOR_SRC
    await _mk(seeded_kernel, agent_id="noargs", allowed=["x"])
    r = (
        await seeded_kernel.send(
            [{"target": "noargs", "payload": {"type": "connector"}}]
        )
    )[0]["result"]
    assert "`kernel.self_id`, `kernel.args`" not in r["howto"], r["howto"]
    assert "ARGV" in r["howto"], r["howto"]


async def test_password_ingress_is_refused_on_fs_and_process_legs(seeded_kernel):
    """The process edge has no wire envelope — nothing can present a token, so
    `password_ingress` is refused at the write (both spellings) rather than
    sealing a leg the operator believes is password-gated."""
    for i, meta in enumerate(
        ({"ingress_rule": "password_ingress"}, {"auth": "password"})
    ):
        r = await _mk(seeded_kernel, agent_id=f"pw{i}", **meta)
        assert r.get("reason") == "invalid_meta", (meta, r)
        assert "wire envelope" in r["error"], r
