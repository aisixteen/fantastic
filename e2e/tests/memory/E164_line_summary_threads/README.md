# `line_summary_threads` (E164) — threads: each thread folds under its own lines (#709)

Free, Rust lane: no model. The rig of E163 (`e2e/helpers/line_fold.py`): one `ai_chat.tools`
chat with `line_summaries: true`, folding over a 6000 token window, completing through a
`vllm_backend` provider on a scripted vLLM mock, watched over the ws face.

- A1: thread A's answers write lines `A-…-k` until a fold lands; its title is the latest A line
  among its members.
- C: the person sends `context_clear`: the new thread's head holds none of A's rows (every row
  of A is older than the clear row: ids come from the kernel's one counter).
- B1: thread B's answers write no line until a fold lands: it keeps the summariser's title
  (asked with a title line), never an A line, and holds no A row.
- B2: thread B goes on with lines `B-…-k` until a second fold lands: its title is the latest B
  line among its members, and it holds no A row.
- V: the chat published every line of both threads once, in order; `reflect` returns the latest
  B line.

Run (when the owner runs e2e): `--only-free --id line_summary_threads`.
