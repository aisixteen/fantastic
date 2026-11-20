# `line_summary_fold` (E163) — no threads: the line titles the fold, the view agrees (#709)

Free, Rust lane: no model. A scripted vLLM mock (`e2e/mocks/vllm.py`) plays the model for one
`ai_chat.tools` chat on the PACKAGED host: its record sets `line_summaries: true` and folds
(`context_strategy: fold`) over a 6000 token window; it completes through a `vllm_backend`
provider on the mock, which reports a prompt size near the chat's own estimate so the window
calibrates as on a real server. The chat's events are watched over the ws face. The rig is
`e2e/helpers/line_fold.py`, shared with E164. (The sidebar agent that passes the line on with
the tree state is in the `fantastic` product, not the packaged kernel host: its units cover it,
`src/tui/src/kern/sidebar.rs`.)

- L1: each answer writes `<call name="line_summary"><text>LINE-…-k</text></call>`: the chat
  publishes `line_summary_changed {text, ts}` once per line, in order; its `reflect` returns the
  latest as `line_summary {text, ts}`.
- F1: turns go on until a fold lands (a fold id in the last projection's `view_ids`,
  `context_status`). Its title (`history {fold}`) is the latest line among its
  members (read back by `recall`); the summariser's ask was the body-only one (no title line),
  and the fold's summary is the mock's body; the next request shows `[fold N · K refs · L1]
  "LINE-…"`.
- F2: the fold's title is a line the chat published.

Run (when the owner runs e2e): `--only-free --id line_summary_fold`.
