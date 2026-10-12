# E75 `ai_cell` — the AI cell's sunshine paths, on a mock model

Free: the vLLM mock from `e2e/mocks` (no model); the AI's replies are scripted. One canvas, in order:

- S1 the very first request already carries the kernel's description (call grammar, its canvas, addressing) — no `lookaround` turn;
- S2 its first reply is a call to its canvas (`reflect` on the view): reached with no set-up, the result is the next turn, then it answers;
- S3 a plain turn answers; S4 a message sent while one is in flight queues, both answer in order;
- S5 a turn stopped while the model thinks settles idle, unanswered; the next answers;
- S6 `keep` survives a page reload with its history; S7 the kept dialog reopened from the popup's chats tile shows it;
- S9 an exec agent: start a ticker, monitor it, status, tail, stop — the process is gone. The monitor's delivery is its own check: KNOWN failing (2026-10-11) — the canvas AI's `callable_by` does not list the exec agent and `exec_bridge` drops the refusal silently;
- S8 at the kernel's fold window (`E2E_WINDOW`, default 12000; fold strategy, async + titled), long turns fold once: the chat says the context was compacted and the next turn answers.
