# ollama_backend — local LLM, a context-managed completion agent
Thin Provider + Bundle over `fantastic-ai-core`: this crate supplies only the
`OllamaProvider` (NDJSON transport to a local ollama process) and a thin
`Bundle`. The per-client chat threads, FIFO lock, prompt assembly, ONE-PASS
generation, and context management all live in `fantastic-ai-core`.

Reference implementation for the **LLM backend contract** in `fantastic-ai-core`'s
module header. Every backend speaks it, so any caller retargets by changing one id.

## ONE pass — and the model routes its own output
`send` does exactly ONE provider pass. **No loop**: nothing iterates on the
model's behalf, and emitting no `<tool_call>` ends the turn — stop is the
default and costs nothing.

Tool-calling is RAW and belongs to `fantastic-ai-core`. `OllamaProvider` never
sends ollama's `tools` array nor reads `message.tool_calls`: the model writes the
envelope as TEXT, this transport streams that text through knowing nothing about
it, and ai-core parses and dispatches it.

```
<tool_call>{"name":"send","arguments":{"target_id":"<id>","payload":{"type":"<verb>"},"reply_to":"self"}}</tool_call>
```

Each call is dispatched and its reply delivered to that call's `reply_to`, which
DEFAULTS TO SELF — leave it out and the answer arrives as the model's next turn,
tagged `[reply to <id>]`; `null`/`"none"` discards it. `reply_to:"self"` is what
makes a chain, one pass at a time, and only because the model asked again. A
caller's optional `send_to` hands the pass's TEXT to one agent id instead, once,
without propagating. A `system_prompt` override replaces the default system block
whole, for a caller that wants a different role.

## Context management — owned HERE
Because this backend owns the per-client conversation, it owns the context:
per-client `chat.json` history, auto-compaction on overflow (`context_strategy`,
default `compact`, + `[context-notice]`), and the `recall` / `context_status` verbs.
The durable transcript is never trimmed. Requires `file_bridge_id` for the JSONL sidecar.

Verbs: `send` (args `text`, `client_id?`, `system_prompt?`, `send_to?`) · `history` ·
`recall` · `context_status` · `status` · `reflect` · `boot` · `shutdown`.

## Cancelling a turn
There is no `interrupt` verb: an in-flight `send` IS a kernel task. Kill it with the
substrate's universal system verbs — `send "kernel" {type:"ps"}` for the live task
table, find the row `{agent:<this id>, verb:"send"}`, then `send "kernel"
{type:"cancel", task_id:"<agent>:<n>"}`. The turn closes with `status(phase="done",
reason="cancelled")` + `done`, the HTTP stream and the FIFO lock are released, and the
blocked caller's `send` resolves to `{cancelled:true}`. Cancel kills the task's whole
SUBTREE, so work the turn spawned dies with it.

## Streaming routing
`CallerRoute::CliRoundTrip`: `client_id == "cli"` → the `cli` agent; else emitted on
the backend's own inbox tagged with `client_id`. Events: `status`
(queued|thinking|streaming|done) · `token` · `done` · `context` (compaction).
