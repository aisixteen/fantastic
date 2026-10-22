"""A job's stored lines drop a trailing CR, like the monitor and Rust (#205)."""

from __future__ import annotations

import sys

from _testkit import child_env
from test_exec_bridge import _mk, _start
from test_tail_grep import _until


async def test_crlf_output_is_stored_without_cr(seeded_kernel):
    k = seeded_kernel
    await _mk(k, allowed=[r".*"], inherit_env=child_env("PATH"))
    prog = "import sys; sys.stdout.write('a\\r\\nb\\r\\n')"
    r = await _start(k, "runner", sys.executable, ["-c", prog])
    jid = r["job_id"]
    t = await _until(k, jid, 2)
    assert [x["line"] for x in t["lines"]] == ["a", "b"], t
    assert all("\r" not in x["line"] for x in t["lines"]), t
