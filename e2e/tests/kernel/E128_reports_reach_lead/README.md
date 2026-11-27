# `reports_reach_lead` (E128) — reports reach a lead, each read once

Free: no model. Four local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model
of a LEAD and of three WORKERS (the kernel's own AI agents, `ai_chat.tools` chats each
naming its own `vllm_backend` provider, #521), driven over `web_rest`.

It was `await_set` (one combined message for a set). The owner removed `await_set` and
`await_cancel` with their rows (#795, Q1): everything is async and the model decides, so
nothing waits for a set and nothing is held. The protection moves to what replaced it:
each arrival wakes the lead, and arrivals during a pass are read by exactly one next pass.

The lead is told to expect three reports, answers plainly + `done`. The TEST then asks
the three workers at once; each one `send`s its report to the lead, then `done`.

What it checks: every send is accepted; each worker made exactly one request (the ack
of a send to an AI is stored, never a wake: Q2); the lead's last history holds each
report exactly once, as a message from its worker (`[from w<n> · msg …]`); every lead
pass after the kick reads a report for the first time (no wake without news); the lead
ran 1 to 3 passes after the kick; nothing is left in `ps`.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id reports_reach_lead --lanes 999
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E128_reports_reach_lead/probe.py
```
