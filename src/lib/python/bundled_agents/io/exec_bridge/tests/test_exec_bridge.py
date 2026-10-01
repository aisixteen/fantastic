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
        # WHERE a command runs is the record's, required — never guessed
        "cwd": ".",
        # a bare command is found on the CHILD's PATH only
        "inherit_env": ["PATH"],
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
                [{"target": "nf", "payload": {"type": verb, "job_id": 999999}}]
            )
        )[0]["result"]
        assert r["reason"] == "not_found", (verb, r)
        assert r["job_id"] == 999999, (verb, r)


async def test_a_job_id_is_the_jobs_task_id_and_nothing_else_names_it(seeded_kernel):
    """One id space: the job's id IS its kernel task id — `ps` lists it, `cancel`
    takes it. A string (even a stringified int) is a bad argument, never looked up:
    the old random 8-hex ids came back from a model as ints or infinite floats
    (`04203581`, `458e5912`) and 3.7% of jobs could not be addressed again."""
    k = seeded_kernel
    await _mk(k, agent_id="ids", allowed=[rf"{sys.executable} .*"])
    r = await _start(k, "ids", sys.executable, ["-c", "import time; time.sleep(30)"])
    jid = r["job_id"]
    assert isinstance(jid, int) and not isinstance(jid, bool), r
    ps = (await k.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]["result"]
    assert any(t["id"] == jid and t["agent"] == "ids" for t in ps["tasks"]), ps
    for verb in ("status", "stop", "interrupt", "clear", "write", "output"):
        bad_arg = (
            await k.send(
                [{"target": "ids", "payload": {"type": verb, "job_id": str(jid)}}]
            )
        )[0]["result"]
        assert (bad_arg["reason"], bad_arg["arg"]) == ("bad_arg", "job_id"), (
            verb,
            bad_arg,
        )
    snap = (
        await k.send([{"target": "ids", "payload": {"type": "status", "job_id": jid}}])
    )[0]["result"]["jobs"][0]
    assert snap["job_id"] == jid and "task_id" not in snap, snap
    await k.send([{"target": "ids", "payload": {"type": "stop", "job_id": jid}}])


async def test_write_time_refusal_for_broken_regex(seeded_kernel):
    r = await _mk(seeded_kernel, agent_id="r2", allowed=["("])
    assert "broken regex" in r["error"]


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


async def test_the_records_env_reaches_the_child(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"/bin/sh -c .*"], env={"MARK": "x42"})
    r = await _start(seeded_kernel, "runner", "/bin/sh", ["-c", "echo $MARK"])
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["last_line"] == "x42"


# ─── the child's WHOLE environment is the record's (audit 2026-09-28) ───────
# A per-call `env` (`LD_PRELOAD`, `PATH`, …) or `cwd` changed what an allowlisted
# line actually ran, and every child inherited the kernel's `.env` keys.


async def test_a_per_call_env_or_cwd_is_refused(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"/bin/sh -c .*"])
    for arg, val in (("env", {"LD_PRELOAD": "/tmp/x.so"}), ("cwd", "/")):
        r = await _start(
            seeded_kernel, "runner", "/bin/sh", ["-c", "true"], **{arg: val}
        )
        assert (r["reason"], r["arg"]) == ("bad_arg", arg), r


async def test_a_leg_with_no_cwd_runs_nothing(seeded_kernel):
    await _mk(seeded_kernel, agent_id="nocwd", allowed=[r"/bin/sh -c .*"], cwd=None)
    r = await _start(seeded_kernel, "nocwd", "/bin/sh", ["-c", "true"])
    assert (r["reason"], r["arg"]) == ("missing_arg", "cwd"), r


async def test_the_kernel_env_reaches_a_child_only_by_name(seeded_kernel, monkeypatch):
    monkeypatch.setenv("FT_SECRET", "s3cret")
    monkeypatch.setenv("FT_OPEN", "open")
    line = ["-c", "echo ${FT_SECRET:-none}/${FT_OPEN:-none}"]
    for aid, inherit, want in (
        ("bare", None, "none/none"),
        ("named", ["FT_OPEN"], "none/open"),
        ("all", ["*"], "s3cret/open"),
    ):
        await _mk(
            seeded_kernel, agent_id=aid, allowed=[r"/bin/sh -c .*"], inherit_env=inherit
        )
        r = await _start(seeded_kernel, aid, "/bin/sh", line)
        row = await _wait_done(seeded_kernel, aid, r["job_id"])
        assert row["last_line"] == want, (aid, row)


async def test_a_bare_command_is_found_on_the_childs_path_only(seeded_kernel, tmp_path):
    tool = tmp_path / "ft_only_here"
    tool.write_text("#!/bin/sh\necho found-by-child-path\n")
    tool.chmod(0o755)
    # No PATH in the child env: refused naming it — no kernel PATH, no default path.
    await _mk(seeded_kernel, agent_id="nopath", allowed=[r"echo .*"], inherit_env=None)
    r = await _start(seeded_kernel, "nopath", "echo", ["x"])
    assert (r["reason"], r["arg"]) == ("missing_arg", "PATH"), r
    # The child's PATH decides: a tool the kernel's PATH does not hold runs…
    await _mk(
        seeded_kernel,
        agent_id="ownpath",
        allowed=[r"(ft_only_here|echo)( .*)?"],
        inherit_env=None,
        env={"PATH": str(tmp_path)},
    )
    r = await _start(seeded_kernel, "ownpath", "ft_only_here")
    row = await _wait_done(seeded_kernel, "ownpath", r["job_id"])
    assert row["last_line"] == "found-by-child-path", row
    # …and one only the kernel's PATH holds is not found.
    r = await _start(seeded_kernel, "ownpath", "echo", ["x"])
    assert r["reason"] == "transport_error" and "No such file" in r["error"], r


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
    # the job's id IS its task id
    task_id = st["jobs"][0]["job_id"]
    assert isinstance(task_id, int)
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
                        "allow": ["."],
                        "ingress_rule": "allow_all_ingress",
                        "root": "sd",
                        "callable_by": {"runner": "*"},
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    await _mk(
        seeded_kernel, allowed=[rf"{py} -c .*"], can_call={fb: "*"}, env={"FB": fb}
    )
    r = await _start(seeded_kernel, "runner", py, ["-c", CONNECTOR_SRC + _STREAM_CHILD])
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
                "payload": {"type": "cancel", "task_id": st["jobs"][0]["job_id"]},
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


async def test_stop_and_delete_kill_everything_a_job_launched(seeded_kernel):
    """A plain job leads its own session, so `stop` and `delete_agent` take what it
    launched too — a script's `sleep &` and a `nohup` used to outlive it. Twin of
    the PTY case (`test_pty_jobs.py::test_every_teardown_kills_the_whole_session`)."""
    import os

    import exec_bridge.tools as ebt
    from exec_bridge import proc

    line = "sleep 303 & nohup sleep 304 >/dev/null 2>&1 & sleep 305"
    for aid, end in (("pipe_stop", "stop"), ("pipe_delete", "delete_agent")):
        await _mk(seeded_kernel, agent_id=aid, allowed=[r"/bin/sh -c .*"])
        r = await _start(seeded_kernel, aid, "/bin/sh", ["-c", line])
        leader = ebt._jobs[r["job_id"]]["proc"].pid
        members: list[int] = []
        for _ in range(100):
            members = []
            for p in proc._all_pids():
                try:
                    if p != leader and os.getsid(p) == leader:
                        members.append(p)
                except OSError:
                    continue
            if len(members) >= 3:
                break
            await asyncio.sleep(0.05)
        assert len(members) >= 3, (end, members)
        if end == "stop":
            await seeded_kernel.send(
                [{"target": aid, "payload": {"type": "stop", "job_id": r["job_id"]}}]
            )
        else:
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {"type": "delete_agent", "id": aid},
                    }
                ]
            )
        alive = [leader, *members]
        for _ in range(100):
            left = []
            for p in alive:
                try:
                    os.kill(p, 0)
                    left.append(p)
                except ProcessLookupError:
                    pass
            alive = left
            if not alive:
                break
            await asyncio.sleep(0.05)
        assert not alive, (end, alive)


# ─── the relay's watch proxies, pinned (reduction candidate H: one mirror) ───
#
# `watch`/`unwatch` and `onmessage`/`offmessage` each register a PROXY inbox the
# relay drains down the fd. A shared mirror helper must keep: the fd event frame
# `{op:event, src, payload}` (the ws leg's is `{type:event, src, payload}`), the
# inbox frame `{op:inbox, payload}` minus the job's own telemetry, a release that
# drops the watcher, the inbox row AND the drain task, and an owner-inbox
# supersede that never evicts its replacement.


from kernel._authz import _FROM_ABOVE  # noqa: E402


def _proxy_tasks(kernel, owner, verb):
    return [
        tid
        for tid, e in kernel.ctx.tasks.items()
        if e.get("agent") == owner and e.get("verb") == verb and not e["_task"].done()
    ]


async def test_a_granted_watch_frames_events_and_unwatch_releases_everything(
    seeded_kernel,
):
    k = seeded_kernel
    await _mk(k, allowed=["x"], can_call={"src_a": ["watch"]})
    await _mk(k, agent_id="src_a", allowed=["x"], callable_by={"runner": ["watch"]})
    child, task = await _wire(k, job_id="jw")
    await _wire_write(child, {"op": "watch", "src": "src_a"})
    proxy = "_xbw_jw_src_a"
    await _until(lambda: proxy in k.ctx.agents["src_a"]._watcher_ids, what="watch")
    assert proxy in k.ctx.inboxes
    assert len(_proxy_tasks(k, "runner", "watch_drain")) == 1
    # a second watch of the same src registers nothing new
    await _wire_write(child, {"op": "watch", "src": "src_a"})
    await k.send([{"target": "src_a", "payload": {"type": "status"}}])
    ev = await _wire_read(child)
    assert ev == {"op": "event", "src": "src_a", "payload": {"type": "status"}}
    assert list(ev) == ["op", "src", "payload"]
    assert len(_proxy_tasks(k, "runner", "watch_drain")) == 1

    await _wire_write(child, {"op": "unwatch", "src": "src_a"})
    await _until(lambda: proxy not in k.ctx.inboxes, what="proxy released")
    assert proxy not in k.ctx.agents["src_a"]._watcher_ids
    await _until(lambda: not _proxy_tasks(k, "runner", "watch_drain"), what="drain")
    child.close()
    await asyncio.wait_for(task, 5)


async def test_a_refused_watch_registers_no_drain_task(seeded_kernel):
    k = seeded_kernel
    await _mk(k, allowed=["x"])
    await _mk(k, agent_id="closed", allowed=["x"])
    child, task = await _wire(k, job_id="jr")
    await _wire_write(child, {"op": "watch", "src": "closed"})
    ev = await _wire_read(child)
    assert ev["op"] == "event" and ev["src"] == "closed"
    assert ev["payload"] == {
        "error": "runner may not call 'watch' on closed",
        "reason": "unauthorized",
        "hint": _FROM_ABOVE,
        "side": "egress",
    }
    assert not _proxy_tasks(k, "runner", "watch_drain")
    assert "_xbw_jr_closed" not in k.ctx.inboxes
    child.close()
    await asyncio.wait_for(task, 5)


async def test_onmessage_frames_the_owner_inbox_minus_job_telemetry(seeded_kernel):
    import exec_bridge.tools as ebt

    k = seeded_kernel
    await _mk(k, allowed=["x"])
    child, task = await _wire(k, job_id="jm")
    await _wire_write(child, {"op": "onmessage"})
    await _until(lambda: "_xbself_jm" in k.ctx.inboxes, what="owner proxy")
    assert ebt._owner_inbox["runner"]["job_id"] == "jm"
    await k.emit("runner", {"type": "progress", "n": 1})  # the job's own telemetry
    await k.send([{"target": "runner", "payload": {"type": "mail", "n": 2}}])
    ev = await _wire_read(child)
    assert ev == {"op": "inbox", "payload": {"type": "mail", "n": 2}}
    await _wire_write(child, {"op": "offmessage"})
    await _until(lambda: "_xbself_jm" not in k.ctx.inboxes, what="released")
    assert "runner" not in ebt._owner_inbox
    assert "_xbself_jm" not in k.ctx.agents["runner"]._watcher_ids
    child.close()
    await asyncio.wait_for(task, 5)


async def test_a_second_onmessage_supersedes_without_being_evicted(seeded_kernel):
    import exec_bridge.tools as ebt

    k = seeded_kernel
    await _mk(k, allowed=["x"])
    c1, t1 = await _wire(k, job_id="j1")
    await _wire_write(c1, {"op": "onmessage"})
    await _until(lambda: "_xbself_j1" in k.ctx.inboxes, what="first")
    c2, t2 = await _wire(k, job_id="j2")
    await _wire_write(c2, {"op": "onmessage"})
    await _until(lambda: "_xbself_j2" in k.ctx.inboxes, what="second")
    # the first is released, the second owns the inbox
    await _until(lambda: "_xbself_j1" not in k.ctx.inboxes, what="superseded")
    assert ebt._owner_inbox["runner"]["job_id"] == "j2"
    # the superseded job exits — its teardown must not evict its replacement
    c1.close()
    await asyncio.wait_for(t1, 5)
    assert ebt._owner_inbox["runner"]["job_id"] == "j2"
    assert "_xbself_j2" in k.ctx.inboxes
    assert "_xbself_j2" in k.ctx.agents["runner"]._watcher_ids
    await k.send([{"target": "runner", "payload": {"type": "mail"}}])
    assert await _wire_read(c2) == {"op": "inbox", "payload": {"type": "mail"}}
    c2.close()
    await asyncio.wait_for(t2, 5)
    assert "runner" not in ebt._owner_inbox
    assert "_xbself_j2" not in k.ctx.inboxes
