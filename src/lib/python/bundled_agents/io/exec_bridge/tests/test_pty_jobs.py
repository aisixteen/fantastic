"""A terminal is a PTY JOB of exec_bridge — terminal_backend merged in (a4).

`start pty={cols,rows}` spawns through the same `proc`, behind the same `allowed`
gate, with the same record-owned `cwd`/`env`; the job streams `output` events and is
driven with `write`/`resize`/`ack`/`output`/`paste_image`. There is no second spawner.
"""

from __future__ import annotations

import asyncio
import os
import sys

import pytest

from exec_bridge import proc
from exec_bridge import tools as ebt

PY = sys.executable
# A tiny program that proves it sits on a terminal: its stdin is a tty, TERM is
# set, the window size is the one asked for, and a typed line reaches it.
_CHILD = (
    "import fcntl,os,struct,sys,termios,signal\n"
    "def size():\n"
    "    r,c=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,b'\\0'*8))[:2]\n"
    "    return c,r\n"
    "signal.signal(signal.SIGWINCH, lambda *a: print('WINCH %d %d' % size(), flush=True))\n"
    "print('TTY', os.isatty(0), os.environ.get('TERM'), flush=True)\n"
    "print('SIZE %d %d' % size(), flush=True)\n"
    "line = sys.stdin.readline()\n"
    "print('GOT', line.strip(), flush=True)\n"
)


async def _mk(kernel, agent_id="term", **extra):
    meta = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "exec_bridge.tools",
        "ingress_rule": "allow_all_ingress",
        "cwd": ".",
        "allowed": [rf"{PY} -c .*"],
        **extra,
    }
    return (await kernel.send([{"target": kernel.id, "payload": meta}]))[0]["result"]


async def _call(kernel, payload, target="term"):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


@pytest.fixture
async def term(seeded_kernel):
    k = seeded_kernel
    assert (await _mk(k)).get("id") == "term"
    k.ctx.ensure_inbox("viewer")
    k.watch("term", "viewer")
    yield k
    for j in list(ebt._jobs.values()):
        if j.get("owner") == "term":
            ebt._teardown(j)


async def _until(k, pred, secs=10.0):
    """Collect `output` text from the viewer until `pred(text)`."""
    q = k.ctx.inboxes["viewer"]
    text, events = "", []
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred(text):
        left = end - loop.time()
        assert left > 0, f"timed out; got {text!r}"
        ev = await asyncio.wait_for(q.get(), timeout=left)
        events.append(ev)
        # an `output` CALL to the job (a replay) is mirrored too — it has no data
        if ev.get("type") == "output" and "data" in ev:
            text += ev["data"]
    return text, events


async def _start(k, cols=91, rows=37):
    r = await _call(
        k,
        {
            "type": "start",
            "command": PY,
            "args": ["-c", _CHILD],
            "pty": {"cols": cols, "rows": rows},
        },
    )
    assert r.get("pty") is True, r
    return r["job_id"]


async def test_a_pty_job_runs_on_a_terminal_and_takes_keystrokes(term):
    jid = await _start(term)
    text, events = await _until(term, lambda t: "SIZE" in t)
    assert "TTY True xterm-256color" in text, text
    assert "SIZE 91 37" in text, text
    assert all(e["job_id"] == jid for e in events if e.get("type") == "output")
    assert (await _call(term, {"type": "write", "job_id": jid, "data": "hello\n"})) == {
        "written": 6
    }
    text, events = await _until(term, lambda t: "GOT hello" in t)
    done = None
    q = term.ctx.inboxes["viewer"]
    while done is None:
        ev = await asyncio.wait_for(q.get(), timeout=10)
        if ev.get("type") == "job_done":
            done = ev
    assert done == {"type": "job_done", "job_id": jid, "exit_code": 0, "ok": True}


async def test_resize_reaches_the_program(term):
    jid = await _start(term)
    await _until(term, lambda t: "SIZE" in t)
    r = await _call(term, {"type": "resize", "job_id": jid, "cols": 120, "rows": 40})
    assert r == {"resized": True, "cols": 120, "rows": 40}
    await _until(term, lambda t: "WINCH 120 40" in t)
    snap = (await _call(term, {"type": "status", "job_id": jid}))["jobs"][0]
    assert snap["pty"] is True and (snap["cols"], snap["rows"]) == (120, 40), snap
    await _call(term, {"type": "write", "job_id": jid, "data": "x\n"})


async def test_output_replays_the_ring_for_a_client_that_re_attaches(term):
    jid = await _start(term)
    await _until(term, lambda t: "SIZE" in t)
    ring = (await _call(term, {"type": "output", "job_id": jid}))["output"]
    assert "TTY True" in ring and "SIZE 91 37" in ring, ring
    tail = (await _call(term, {"type": "output", "job_id": jid, "max_bytes": 8}))[
        "output"
    ]
    assert len(tail.encode()) <= 8 and ring.endswith(tail), tail
    await _call(term, {"type": "write", "job_id": jid, "data": "x\n"})


async def test_flow_control_pauses_the_reader_until_acked(term, monkeypatch):
    monkeypatch.setattr(ebt, "HIGH_WATERMARK", 10)
    monkeypatch.setattr(ebt, "LOW_WATERMARK", 5)
    jid = await _start(term)
    await _until(term, lambda t: "TTY" in t)
    pt = ebt._jobs[jid]["pty"]
    for _ in range(50):
        if pt["paused"]:
            break
        await asyncio.sleep(0.02)
    assert pt["paused"], "past the high-water mark the reader detaches"
    # typed while paused: the program answers, but nothing reaches the viewer
    await _call(term, {"type": "write", "job_id": jid, "data": "hello\n"})
    await asyncio.sleep(0.4)
    q = term.ctx.inboxes["viewer"]
    while not q.empty():
        assert q.get_nowait().get("type") != "output", "output flowed while paused"
    r = await _call(term, {"type": "ack", "job_id": jid, "chars": 10_000})
    assert r == {"unacked": 0, "paused": False}, r
    # the ack re-attaches the reader: the backlog now arrives, acked chunk by chunk
    text = ""
    while "GOT hello" not in text:
        chunk, _ = await _until(term, lambda t: bool(t))
        text += chunk
        await _call(term, {"type": "ack", "job_id": jid, "chars": 10_000})


async def test_paste_image_types_a_saved_path(term):
    jid = await _start(term)
    await _until(term, lambda t: "SIZE" in t)
    # The mime is the caller's to state — never guessed.
    r = await _call(term, {"type": "paste_image", "job_id": jid, "data": b"\x89PNG"})
    assert (r["reason"], r["arg"]) == ("missing_arg", "mime"), r
    r = await _call(
        term,
        {
            "type": "paste_image",
            "job_id": jid,
            "data": b"\x89PNG....",
            "mime": "image/png",
        },
    )
    assert r["bytes"] == 8 and r["path"].endswith(".png"), r
    await _call(term, {"type": "write", "job_id": jid, "data": "\n"})
    text, _ = await _until(
        term, lambda t: "GOT" in t and ".png" in t.split("GOT", 1)[1]
    )
    assert r["path"] in text


async def test_a_pty_job_is_gated_like_any_start(term):
    r = await _call(
        term,
        {
            "type": "start",
            "command": "/bin/sh",
            "args": ["-i"],
            "pty": {"cols": 80, "rows": 24},
        },
    )
    assert r["reason"] == "unauthorized", r
    r = await _call(
        term, {"type": "start", "command": PY, "args": ["-c", "0"], "pty": {"cols": 0}}
    )
    assert r["reason"] == "bad_arg", r


async def test_the_pty_verbs_want_a_live_pty_job(term):
    r = await _call(term, {"type": "write", "job_id": 999999, "data": "x"})
    assert r["reason"] == "not_found", r
    pipe = await _call(
        term, {"type": "start", "command": PY, "args": ["-c", "input()"]}
    )
    r = await _call(term, {"type": "write", "job_id": pipe["job_id"], "data": "x"})
    assert (r["reason"], r["arg"]) == ("bad_arg", "job_id"), r
    await _call(term, {"type": "stop", "job_id": pipe["job_id"]})


async def test_stop_ends_a_pty_job(term):
    jid = await _start(term)
    await _until(term, lambda t: "SIZE" in t)
    assert (await _call(term, {"type": "stop", "job_id": jid}))["killed"] == 1
    q = term.ctx.inboxes["viewer"]
    while True:
        ev = await asyncio.wait_for(q.get(), timeout=10)
        if ev.get("type") == "job_done":
            break
    snap = (await _call(term, {"type": "status", "job_id": jid}))["jobs"][0]
    assert snap["status"] == "stopped", snap


def _in_session(leader: int) -> list[int]:
    out = []
    for p in proc._all_pids():
        try:
            if p != leader and os.getsid(p) == leader:
                out.append(p)
        except OSError:
            continue
    return out


def _exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


async def test_every_teardown_kills_the_whole_session(term):
    """A shell puts each job it starts in its OWN process group inside the PTY
    session, so killing the leader alone (and the tty's hangup, which reaches the
    foreground group only) would leave `sleep &` and a `nohup` running, reparented to
    init. `stop` and `delete_agent` (the kernel's shutdown runs the same teardown)
    kill the whole session."""
    for aid, end in (("sh_stop", "stop"), ("sh_delete", "delete_agent")):
        await _mk(
            term, agent_id=aid, allowed=[r"/bin/sh -i"], env={"PATH": "/usr/bin:/bin"}
        )
        r = await _call(
            term,
            {
                "type": "start",
                "command": "/bin/sh",
                "args": ["-i"],
                "pty": {"cols": 80, "rows": 24},
            },
            target=aid,
        )
        jid = r["job_id"]
        leader = ebt._jobs[jid]["proc"].pid
        await _call(
            term,
            {
                "type": "write",
                "job_id": jid,
                "data": "sleep 300 &\nnohup sleep 301 >/dev/null 2>&1 &\nsleep 302\n",
            },
            target=aid,
        )
        members: list[int] = []
        for _ in range(100):
            members = _in_session(leader)
            if len(members) >= 3:
                break
            await asyncio.sleep(0.05)
        assert len(members) >= 3, (end, members)
        if end == "stop":
            await _call(term, {"type": "stop", "job_id": jid}, target=aid)
        else:
            await term.send(
                [{"target": term.id, "payload": {"type": "delete_agent", "id": aid}}]
            )
        alive = [leader, *members]
        for _ in range(100):
            alive = [p for p in alive if _exists(p)]
            if not alive:
                break
            await asyncio.sleep(0.05)
        assert not alive, (end, alive)


# Floods past the high-water mark, then prints a marker and waits on a line.
_FLOOD = (
    "import sys\n"
    "sys.stdout.write('x' * 150000 + '\\nDONE\\n')\n"
    "sys.stdout.flush()\n"
    "sys.stdin.readline()\n"
)


async def test_a_replay_clears_the_backlog_a_gone_viewer_left(term):
    """A viewer that streamed but never acked (a reloaded page) leaves `unacked`
    past HIGH and the reader paused. The new viewer's `output` replay hands it
    everything so far, so it zeroes the count and resumes the reader."""
    r = await _call(
        term,
        {
            "type": "start",
            "command": PY,
            "args": ["-c", _FLOOD],
            "pty": {"cols": 80, "rows": 24},
        },
    )
    jid = r["job_id"]
    for _ in range(250):
        st = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
        if st["paused"]:
            break
        await asyncio.sleep(0.02)
    assert st["paused"] and st["unacked"] > ebt.HIGH_WATERMARK, st
    await _call(term, {"type": "output", "job_id": jid})
    st = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
    # the count restarted at 0; the resumed reader may already have counted more
    assert not st["paused"] and st["unacked"] < ebt.HIGH_WATERMARK, st
    # the rest of the flood (under HIGH) arrives with no further ack
    await _until(term, lambda t: "DONE" in t)
    await _call(term, {"type": "write", "job_id": jid, "data": "\n"})
