"""coding/peer_review — one AI SENDS ANOTHER AN INQUIRY, and the answer decides the verdict.

Codex `auto_review.rs`: a second agent reviews the first's work. Two real AI
agents — but the harness no longer drives them in turn. THE HUMAN tasks the
coder, the coder applies a change that VIOLATES the spec, and then the CODER
SENDS THE REVIEWER AN INQUIRY, the same verb a person answers. The reviewer
reads `spec.md` and `git diff` itself and answers by THINKING; its answer comes
back as the reply, and the coder records it.

That is the whole point of ONE INQUIRY VERB: `send(<anyone>, {type:"inquiry"})`
reaches a human who answers from a record and a peer AI who answers from a
turn, and the sender never branches on which. Before this an AI sent an inquiry
got `unknown type 'inquiry'` — inquiry between agents had no shape at all.

Scored on the verdict file, the reviewer's own tally of turns, and the ledger
edge proving `ai -> ai_rev` actually happened.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, verdict
from helpers.facts_arm import store_rows
from helpers.kernel import open_kernel, home

BK = _resolve_backend()
FB, RUNNER = "pr_fb", "pr_runner"
INTERP = sys.executable
DONE = 'end your run by calling done — emit <call name="done"></call>'
SPEC = "The service MUST use `timeout = 30`. Any other value is a defect.\n"
CONFIG = "host = 127.0.0.1\ntimeout = 30\nworkers = 4\n"


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def _git(*args):
    subprocess.run(["git", *args], check=True, capture_output=True)


async def main():
    root, core, close = await open_kernel(
        prefix="coding_pr_", reach=[FB, RUNNER, "editor", "ai", "ai_rev", "cli", "user"]
    )
    try:
        Path("spec.md").write_text(SPEC)
        Path("config.py").write_text(CONFIG)
        _git("init", "-q"); _git("add", "-A")
        _git("-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "baseline")

        for payload in (
            {"type": "create_agent", "parent_id": home(root), "id": FB,
             "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {"ai": "*", "ai_rev": "*", "rest": "*", "web": "*"}},
            {"type": "create_agent", "parent_id": home(root), "id": "editor",
             "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
             "ingress_rule": "allow_all_ingress",
             "callable_by": {"ai": "*", "ai_rev": "*", "rest": "*", "web": "*"}},
            {"type": "create_agent", "parent_id": home(root), "id": RUNNER,
             "handler_module": "exec_bridge.tools", "cwd": os.getcwd(), "inherit_env": ["PATH", "HOME"],
             "ingress_rule": "allow_all_ingress", "allowed": ["git .*"],
             "callable_by": {"ai": "*", "ai_rev": "*", "rest": "*", "web": "*"}},
        ):
            r = await _call(root, root.id, payload)
            assert not r.get("error"), f"create {payload['id']}: {r}"

        # THE HUMAN who wants the work done. The task originates here, so the
        # coder's first turn arrives as a gated `user -> ai` send with a ledger
        # row rather than an operator injection from outside the kernel.
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": "user",
            "handler_module": "user_probe.tools",
            "answer": "do your best judgement",
            "description": "The human this kernel belongs to.",
            "can_call": {"ai": "*"},
            "callable_by": {"ai": "*", "rest": "*", "web": "*"},
        })
        assert not r.get("error"), f"create user: {r}"

        # THE REVIEWER, wired to RECEIVE INQUIRIES. It needs the spec and the diff to
        # answer, and the coder needs egress to it; the gate is two-sided and
        # these are siblings, so both halves are written here.
        # THE ANSWER IS THE REVIEWER'S OWN SEND (owner, 2026-10-06: inquiries are
        # fully async) — `ai_rev -> ai send`, both halves (the coder's below).
        rv = await connect(root, core, BK, agent_id="ai_rev",
                           can_call={FB: "*", "editor": "*", RUNNER: "*", "cli": "*",
                                     "ai": ["send"]},
                           callable_by={"ai": "*"},
                           limits=Limits.from_env(os.environ))

        coder_task = (
            "In `config.py`, change `timeout = 30` to `timeout = 300` using the "
            "editor `editor`'s `edit` verb (reflect it for the shape) — change "
            "nothing else. THEN GET IT REVIEWED: `ai_rev` is a colleague, "
            "another agent like you. `send` it an `inquiry` on whether the "
            "change satisfies `spec.md`, telling it to read the spec through "
            "the editor `editor` and the working-tree diff by running "
            "`git diff` on the runner `pr_runner`, and to answer with APPROVE "
            "or REJECT followed by the offending value. Its answer comes back "
            "as that call's reply. Write the answer VERBATIM to `review.md` "
            f"through the editor — do not judge it yourself. Then {DONE}."
        )
        c = await connect(root, core, BK, agent_id="ai",
                          can_call={FB: "*", "editor": "*", "cli": "*", "ai_rev": "*"},
                          callable_by={"user": "*", "ai_rev": ["send"]},
                          limits=Limits.from_env(os.environ))
        # THE TASK COMES FROM THE HUMAN — gated, ledgered, a real sender.
        await drive(c, coder_task, client_id="coder", via="user")
        tr_coder = "".join(c.proxy.transcript)
        await c.close()

        # THE REVIEWER'S OWN RECORD OF WORK. It was never driven by the harness,
        # so its turns exist only because the coder inquired: reading its history
        # is how we know a second mind actually ran.
        # THE DEFAULT CLIENT: the coder's question carried no `client_id`, so the
        # reviewer's turns live under ai_core's default — reading them under the
        # coder's own client name found an empty history and scored a second
        # mind that had plainly worked as never having run (measured 2026-09-04).
        rev_hist = {"messages": await store_rows(root, core, "ai_rev")}
        tr_rev = str(rev_hist.get("messages", rev_hist))
        await rv.close()

        review = Path("review.md").read_text() if Path("review.md").exists() else ""
        applied = "timeout = 300" in Path("config.py").read_text()
        rejected = review.strip().upper().startswith("REJECT")
        # THE INQUIRY ITSELF, off the ledger: an `ai -> ai_rev` execution is proof
        # a second mind was consulted. Prose claiming "I consulted the reviewer"
        # leaves no row.
        led = await _call(root, "kernel", {"type": "ledger"})
        inquired_peer = [r for r in (led.get("executions") or [])
                      if r.get("target") == "ai_rev"]
        # THE LEDGER VIEW IS ORIGIN-SCOPED on every runtime: a caller sees the
        # executions IT originated; only the operator sees all. In process the
        # probe IS the operator. Over the wire it arrives as the `rest` agent and
        # the coder's `ai -> ai_rev` row is not its to see — so the same proof
        # is read off the coder's OWN rows: a `[reply to <pid> · ai_rev inquiry`
        # tag is written only for a call the kernel really dispatched, and
        # names the row the ledger holds (measured 2026-09-11/12: four rust runs
        # scored "never inquired" with the inquiry plainly in the transcript).
        if not inquired_peer and core is None:
            # The coder's ROWS, not its transcript: a delivered reply is a
            # user-role row, and the connector's transcript keeps assistant
            # turns only.
            coder_rows = await store_rows(root, core, "ai")
            inquired_peer = [r for r in coder_rows
                             if r.get("role") == "user"
                             and str(r.get("content") or "").startswith("[reply to ")
                             and " · ai_rev inquiry" in str(r.get("content")).split("\n", 1)[0]]
        checks = [
            ("the coder applied the flawed change", applied),
            ("the coder INQUIRED of the reviewer (ledger row)", bool(inquired_peer)),
            ("the reviewer REJECTED it", rejected),
            ("the reviewer named the offending value", "300" in review),
            ("both minds did real work", "editor" in tr_coder and RUNNER in tr_rev),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        print(f"  review.md = {review.strip()[:120]!r}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "coder wrote it, reviewer caught it" if allok
                else "a peer_review check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
