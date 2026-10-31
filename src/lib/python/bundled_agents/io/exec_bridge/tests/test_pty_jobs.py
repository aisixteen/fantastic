"""A terminal is a PTY JOB of exec_bridge — terminal_backend merged in (a4).

`start pty={cols,rows}` spawns through the same `proc`, behind the same `allowed`
gate, with the same record-owned `cwd`/`env`; the job streams `output` events and is
driven with `write`/`resize`/`ack`/`output`/`paste_image`. There is no second spawner.
"""

from __future__ import annotations

import asyncio
import sys

import pytest
from _testkit import child_env, command_rx, per_os

from exec_bridge import _sys
from exec_bridge import tools as ebt

PY = sys.executable
PY_RX = command_rx(PY)
# A tiny program that proves it sits on a terminal: its stdin is a tty, TERM is
# set, the window size is the one asked for, and a typed line reaches it. A resize
# reaches it as SIGWINCH on POSIX; a Windows console has no such signal, so there
# the program watches its window size (what a console program reads).
_CHILD = per_os(
    posix=(
        "import fcntl,os,struct,sys,termios,signal\n"
        "def size():\n"
        "    r,c=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,b'\\0'*8))[:2]\n"
        "    return c,r\n"
        "def say(s):\n"
        "    os.write(1, (s + '\\n').encode())\n"
        "signal.signal(signal.SIGWINCH, lambda *a: say('WINCH %d %d' % size()))\n"
        "say('TTY %s %s' % (os.isatty(0), os.environ.get('TERM')))\n"
        "say('SIZE %d %d' % size())\n"
        "say('READY')\n"
        "line = sys.stdin.readline()\n"
        "say('GOT ' + line.strip())\n"
    ),
    nt=(
        "import os,sys,threading,time\n"
        "def size():\n"
        "    s=os.get_terminal_size(1)\n"
        "    return s.columns,s.lines\n"
        "def watch(last):\n"
        "    while True:\n"
        "        time.sleep(0.05)\n"
        "        now=size()\n"
        "        if now!=last:\n"
        "            last=now\n"
        "            print('WINCH %d %d' % now, flush=True)\n"
        "threading.Thread(target=watch, args=(size(),), daemon=True).start()\n"
        "print('TTY', os.isatty(0), os.environ.get('TERM'), flush=True)\n"
        "print('SIZE %d %d' % size(), flush=True)\n"
        "print('READY', flush=True)\n"
        "line = sys.stdin.readline()\n"
        "print('GOT', line.strip(), flush=True)\n"
    ),
)
#: The Enter key as these tests type it: `\n` on a tty; a Windows console
#: takes `\r` (its line input ends on a carriage return).
ENTER = per_os(posix="\n", nt="\r")


async def _mk(kernel, agent_id="term", **extra):
    meta = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "exec_bridge.tools",
        "ingress_rule": "allow_all_ingress",
        "cwd": ".",
        "allowed": [rf"{PY_RX} -c .*"],
        # what the OS needs to start a child at all (`_sys.OS_ENV`)
        "inherit_env": child_env(),
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


async def _start(k, cols=91, rows=37, program=_CHILD):
    r = await _call(
        k,
        {
            "type": "start",
            "command": PY,
            "args": ["-c", program],
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
    assert (
        await _call(term, {"type": "write", "job_id": jid, "data": "hello" + ENTER})
    ) == {"written": 6}
    text, events = await _until(term, lambda t: "GOT hello" in t)
    done = None
    q = term.ctx.inboxes["viewer"]
    while done is None:
        ev = await asyncio.wait_for(q.get(), timeout=10)
        if ev.get("type") == "job_done":
            done = ev
    assert done == {"type": "job_done", "job_id": jid, "exit_code": 0, "ok": True}


async def test_resize_reaches_the_program_and_status_reports_it(term):
    jid = await _start(term)
    # wait until the program is up and waiting: a resize sent while it was still
    # printing could be lost
    text, _ = await _until(term, lambda t: "READY" in t)
    assert "SIZE 91 37" in text, text
    r = await _call(term, {"type": "resize", "job_id": jid, "cols": 120, "rows": 40})
    assert r == {"resized": True, "cols": 120, "rows": 40}
    await _until(term, lambda t: "WINCH 120 40" in t)
    snap = (await _call(term, {"type": "status", "job_id": jid}))["jobs"][0]
    assert snap["pty"] is True and (snap["cols"], snap["rows"]) == (120, 40), snap
    await _call(term, {"type": "write", "job_id": jid, "data": "x" + ENTER})


@pytest.mark.skipif(sys.platform == "win32", reason="SIGWINCH is POSIX")
async def test_the_child_survives_a_resize_that_lands_mid_print(term):
    # the posix child never prints through a buffered writer a signal handler could
    # re-enter
    assert "print(" not in _CHILD
    jid = await _start(term)
    await _until(term, lambda t: "SIZE" in t)  # deliberately not READY: race the prints
    for i in range(20):
        await _call(
            term, {"type": "resize", "job_id": jid, "cols": 100 + i, "rows": 30 + i}
        )
    await _until(term, lambda t: "WINCH 119 49" in t)
    await _call(term, {"type": "write", "job_id": jid, "data": "x" + ENTER})
    q = term.ctx.inboxes["viewer"]
    while True:
        ev = await asyncio.wait_for(q.get(), timeout=10)
        if ev.get("type") == "job_done":
            break
    assert ev["exit_code"] == 0 and ev["ok"] is True, ev


async def test_output_replays_the_ring_for_a_client_that_re_attaches(term):
    jid = await _start(term)
    await _until(term, lambda t: "SIZE" in t)
    ring = (await _call(term, {"type": "output", "job_id": jid}))["output"]
    assert "TTY True" in ring and "SIZE 91 37" in ring, ring
    tail = (await _call(term, {"type": "output", "job_id": jid, "max_bytes": 8}))[
        "output"
    ]
    assert len(tail.encode()) <= 8 and ring.endswith(tail), tail
    await _call(term, {"type": "write", "job_id": jid, "data": "x" + ENTER})


async def test_flow_control_pauses_the_reader_until_acked(term, monkeypatch):
    monkeypatch.setattr(ebt, "HIGH_WATERMARK", 10)
    monkeypatch.setattr(ebt, "LOW_WATERMARK", 5)
    # The program stays up after its answer: a tty holds a writer's exit until its
    # output is read (the slave's close drains), a console does not — and a job
    # whose program has exited takes no `ack`.
    jid = await _start(term, program=_CHILD + "sys.stdin.readline()\n")
    pt = ebt._jobs[jid]["pty"]
    for _ in range(250):
        if pt["paused"]:
            break
        await asyncio.sleep(0.02)
    assert pt["paused"], "past the high-water mark the reader detaches"
    # what the terminal said before the pause (the program's first line — or, on
    # Windows, the console's own opening sequences) is already the viewer's
    q = term.ctx.inboxes["viewer"]
    # Deterministic drain: every char the reader took is counted in pt["unacked"] and
    # emitted exactly once, nothing is acked yet and the reader is detached — so when
    # the viewer holds pt["unacked"] chars, every in-flight emit (ConPTY's late
    # opening sequences included) has landed and no more can come.
    counted = pt["unacked"]
    await _until(term, lambda t: len(t) >= pt["unacked"])
    assert pt["unacked"] == counted, "the reader is detached after the pause"
    while not q.empty():
        q.get_nowait()
    # typed while paused: the program answers, but nothing reaches the viewer
    await _call(term, {"type": "write", "job_id": jid, "data": "hello" + ENTER})
    # a measured negative window: an absence has no event to wait on
    await asyncio.sleep(0.4)
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


async def test_flow_control_pause_leaves_no_emit_in_flight(term, monkeypatch):
    monkeypatch.setattr(ebt, "HIGH_WATERMARK", 10)
    monkeypatch.setattr(ebt, "LOW_WATERMARK", 5)
    jid = await _start(term, program=_CHILD + "sys.stdin.readline()\n")
    pt = ebt._jobs[jid]["pty"]
    for _ in range(250):
        if pt["paused"]:
            break
        await asyncio.sleep(0.02)
    assert pt["paused"]
    counted = pt["unacked"]
    text, _ = await _until(term, lambda t: len(t) >= pt["unacked"])
    assert len(text) == counted == pt["unacked"], (len(text), counted)
    # barrier: the replay call is mirrored to the viewer as a data-less `output`
    await _call(term, {"type": "output", "job_id": jid})
    q = term.ctx.inboxes["viewer"]
    while True:
        ev = await asyncio.wait_for(q.get(), timeout=5)
        if ev.get("type") == "output":
            assert "data" not in ev, "a chunk was still in flight at the pause"
            break
    await _call(term, {"type": "ack", "job_id": jid, "chars": 10_000})
    await _call(term, {"type": "write", "job_id": jid, "data": "x" + ENTER})
    await _until(term, lambda t: "GOT" in t)


async def test_paste_image_types_a_saved_path(term):
    # wide enough that the typed path never wraps (a console wraps what it renders)
    jid = await _start(term, cols=240)
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
    await _call(term, {"type": "write", "job_id": jid, "data": ENTER})
    text, _ = await _until(
        term, lambda t: "GOT" in t and ".png" in t.split("GOT", 1)[1]
    )
    assert r["path"] in text


async def test_a_pty_job_is_gated_like_any_start_and_its_verbs_want_a_pty_job(term):
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


# What a terminal job launches, per OS: an interactive shell's own jobs — each in
# its OWN process group inside the PTY session — and a `nohup`, typed in; on
# Windows a console program's three children (the job object holds them all).
_LAUNCHER = per_os(
    posix=(
        r"/bin/sh -i",
        "/bin/sh",
        ["-i"],
        "sleep 300 &\nnohup sleep 301 >/dev/null 2>&1 &\nsleep 302\n",
    ),
    nt=(
        rf"{PY_RX} -c .*",
        PY,
        [
            "-c",
            "import subprocess, sys, time\n"
            "for _ in range(3):\n"
            "    subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'])\n"
            "time.sleep(300)\n",
        ],
        "",
    ),
)


async def test_every_teardown_kills_the_whole_session(term):
    """A shell puts each job it starts in its OWN process group inside the PTY
    session, so killing the leader alone (and the tty's hangup, which reaches the
    foreground group only) would leave `sleep &` and a `nohup` running, reparented to
    init. `stop` and `delete_agent` (the kernel's shutdown runs the same teardown)
    kill the whole session (on Windows: the job's Job Object)."""
    allowed, command, args, typed = _LAUNCHER
    for aid, end in (("sh_stop", "stop"), ("sh_delete", "delete_agent")):
        await _mk(term, agent_id=aid, allowed=[allowed], env={"PATH": "/usr/bin:/bin"})
        r = await _call(
            term,
            {
                "type": "start",
                "command": command,
                "args": args,
                "pty": {"cols": 80, "rows": 24},
            },
            target=aid,
        )
        jid = r["job_id"]
        leader = ebt._jobs[jid]["proc"].pid
        if typed:
            await _call(
                term, {"type": "write", "job_id": jid, "data": typed}, target=aid
            )
        members: list[int] = []
        for _ in range(100):
            members = _sys.session_members(leader)
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
            alive = [p for p in alive if _sys.pid_alive(p)]
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


# The same flood after the child stays silent for 8 s: ConPTY's slow start (#93),
# where a started child can print nothing for several seconds first.
_SLOW_FLOOD = "import time\ntime.sleep(8)\n" + _FLOOD


async def _replay_clears_the_backlog(term, program):
    r = await _call(
        term,
        {
            "type": "start",
            "command": PY,
            "args": ["-c", program],
            "pty": {"cols": 80, "rows": 24},
        },
    )
    jid = r["job_id"]
    # wait on the observable state, not a fixed window: a slow start may print
    # nothing for a long while before its flood lands
    for _ in range(750):
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
    await _call(term, {"type": "write", "job_id": jid, "data": ENTER})


async def test_a_replay_clears_the_backlog_a_gone_viewer_left(term):
    """A viewer that streamed but never acked (a reloaded page) leaves `unacked`
    past HIGH and the reader paused. The new viewer's `output` replay hands it
    everything so far, so it zeroes the count and resumes the reader."""
    await _replay_clears_the_backlog(term, _FLOOD)


async def test_a_replay_clears_the_backlog_after_a_slow_start(term):
    """The same replay when the child stays silent for a while before its flood:
    the backlog must still pause the reader and the replay must still clear it."""
    await _replay_clears_the_backlog(term, _SLOW_FLOOD)


# Waits for a typed line, floods past HIGH, waits for a line, floods again, then
# waits — each flood is started by a line the test types, so the test decides when
# it lands.
_GATED_FLOOD = (
    "import sys\n"
    "sys.stdin.readline()\n"
    "sys.stdout.write('x' * 150000 + '\\nDONE1\\n')\n"
    "sys.stdout.flush()\n"
    "sys.stdin.readline()\n"
    "sys.stdout.write('x' * 150000 + '\\nDONE2\\n')\n"
    "sys.stdout.flush()\n"
    "sys.stdin.readline()\n"
)


async def test_a_flood_during_a_re_attach_grace_does_not_pause_the_reader(term):
    """A re-attaching viewer's replay opens a grace: a flood past HIGH during it does
    not pause the reader, and the first ack reports the backlog as it stands."""
    jid = await _start(term, program=_GATED_FLOOD)
    await _call(term, {"type": "output", "job_id": jid})
    await _call(term, {"type": "write", "job_id": jid, "data": "go" + ENTER})
    await _until(term, lambda t: "DONE1" in t)
    r = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
    assert r["paused"] is False and r["unacked"] > ebt.HIGH_WATERMARK, r


async def _printed(k, jid):
    """The job's printed lines, joined: what `tail` reads from the ring, with no ack."""
    r = await _call(k, {"type": "tail", "job_id": jid, "lines": 500})
    return "\n".join(row["line"] for row in r["lines"])


async def test_a_flood_after_the_viewer_stream_ends_pauses_the_reader(term):
    """A replay opens a grace, then the viewer's stream ends before any ack: the grace
    is over, so a flood past HIGH pauses the reader. The program prints DONE1 after its
    flood, and a paused reader never reads that far."""
    jid = await _start(term, program=_GATED_FLOOD)
    await _call(term, {"type": "output", "job_id": jid})
    term.unwatch("term", "viewer")
    await _call(term, {"type": "write", "job_id": jid, "data": "go" + ENTER})
    text = ""
    end = asyncio.get_running_loop().time() + 2.0
    while asyncio.get_running_loop().time() < end and "DONE1" not in text:
        text = await _printed(term, jid)
        await asyncio.sleep(0.02)
    assert "DONE1" not in text, "the reader ran past the flood"
    r = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
    assert r["paused"] and r["unacked"] > ebt.HIGH_WATERMARK, r


async def test_a_flood_after_the_first_ack_pauses_the_reader(term):
    """Once the re-attaching viewer has acked, the grace is over: a flood past HIGH
    pauses the reader again."""
    jid = await _start(term, program=_GATED_FLOOD)
    await _call(term, {"type": "output", "job_id": jid})
    r = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
    assert r == {"unacked": 0, "paused": False}, r
    await _call(term, {"type": "write", "job_id": jid, "data": "go" + ENTER})
    for _ in range(250):
        r = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
        if r["paused"]:
            break
        await asyncio.sleep(0.02)
    assert r["paused"] and r["unacked"] > ebt.HIGH_WATERMARK, r


async def test_with_no_attach_a_flood_pauses_the_reader(term):
    """With no attach there is no grace: a flood past HIGH pauses the reader as before."""
    jid = await _start(term, program=_GATED_FLOOD)
    await _call(term, {"type": "write", "job_id": jid, "data": "go" + ENTER})
    for _ in range(250):
        r = await _call(term, {"type": "ack", "job_id": jid, "chars": 0})
        if r["paused"]:
            break
        await asyncio.sleep(0.02)
    assert r["paused"] and r["unacked"] > ebt.HIGH_WATERMARK, r
