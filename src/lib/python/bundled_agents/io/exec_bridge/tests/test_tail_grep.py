"""`tail` / `grep` — a job's output SO FAR (owner, 2026-10-11). A pipe job's stdout and
stderr land in `status` only when it ends; while it runs, an AI reads the live line
ring: the last lines, or the lines matching a regex, each `{n, stream, line}`."""

from __future__ import annotations

import asyncio
import sys

from test_exec_bridge import _mk, _start, _wait_done

PROG = """import sys, time
for i in range(6):
    print(f"step {i} ok", flush=True)
print("ERROR disk full", file=sys.stderr, flush=True)
print("step 6 ok", flush=True)
time.sleep(30)
"""


async def _ask(k, payload):
    return (await k.send([{"target": "runner", "payload": payload}]))[0]["result"]


async def _until(k, jid, lines, secs=5.0):
    for _ in range(int(secs / 0.05)):
        r = await _ask(k, {"type": "tail", "job_id": jid, "lines": 50})
        if len(r.get("lines") or []) >= lines:
            return r
        await asyncio.sleep(0.05)
    raise AssertionError(f"job {jid} printed fewer than {lines} lines: {r}")


async def test_tail_and_grep_read_a_running_job(seeded_kernel, tmp_path):
    k = seeded_kernel
    prog = tmp_path / "w.py"
    prog.write_text(PROG)
    await _mk(k, allowed=[r".*"], inherit_env=["PATH"])
    r = await _start(k, "runner", sys.executable, [str(prog)])
    jid = r["job_id"]
    await _until(k, jid, 8)
    st = await _ask(k, {"type": "status", "job_id": jid})
    assert st["jobs"][0]["status"] == "running"

    # stdout and stderr are read concurrently: order holds WITHIN a stream only
    t = await _ask(k, {"type": "tail", "job_id": jid, "lines": 2, "stream": "stdout"})
    assert t["status"] == "running" and t["printed"] == 8, t
    assert [x["line"] for x in t["lines"]] == ["step 5 ok", "step 6 ok"], t

    err = await _ask(k, {"type": "tail", "job_id": jid, "stream": "stderr"})
    assert [x["line"] for x in err["lines"]] == ["ERROR disk full"], err

    g = await _ask(k, {"type": "grep", "job_id": jid, "match": r"ERROR|step [45]"})
    assert {x["line"] for x in g["matches"]} == {
        "step 4 ok",
        "step 5 ok",
        "ERROR disk full",
    }, g
    assert g["total"] == 3 and g["scanned"] == 8 and g["dropped"] == 0, g
    last = await _ask(
        k,
        {
            "type": "grep",
            "job_id": jid,
            "match": "step",
            "lines": 1,
            "stream": "stdout",
        },
    )
    assert [x["line"] for x in last["matches"]] == ["step 6 ok"] and last[
        "total"
    ] == 7, last

    await _ask(k, {"type": "stop", "job_id": jid})
    await _wait_done(k, "runner", jid)
    after = await _ask(
        k, {"type": "tail", "job_id": jid, "lines": 1, "stream": "stdout"}
    )
    assert after["status"] != "running" and after["lines"][-1]["line"] == "step 6 ok", (
        after
    )


async def test_bad_arguments_are_refused(seeded_kernel):
    k = seeded_kernel
    await _mk(k, allowed=[r".*"])
    r = await _start(k, "runner", "echo", ["hi"])
    jid = r["job_id"]
    for p, arg in [
        ({"type": "grep", "job_id": jid}, "match"),
        ({"type": "grep", "job_id": jid, "match": "("}, "match"),
        ({"type": "tail", "job_id": jid, "lines": 0}, "lines"),
        ({"type": "tail", "job_id": jid, "stream": "err"}, "stream"),
    ]:
        out = await _ask(k, p)
        assert out.get("reason") == "bad_arg" and out.get("arg") == arg, (p, out)
    nope = await _ask(k, {"type": "tail", "job_id": 999999})
    assert nope.get("reason") == "not_found", nope
