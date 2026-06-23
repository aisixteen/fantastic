# ollama_backend — local LLM agent
Thin Provider + Bundle over `fantastic-ai-core`: this crate supplies
only the `OllamaProvider` (NDJSON transport to a local ollama process)
and a thin `Bundle`. The per-client chat threads, FIFO lock, menu
cache, history, prompt assembly, and agentic loop all live in
`fantastic-ai-core`.

Reference implementation for the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. Every backend (nvidia_nim,
apple_fm, …) speaks the same contract, so any caller retargets by
changing one record field — no caller-side change.

## Tool-calling is RAW (no native ollama tools)
This backend NEVER sends ollama's `tools` array nor reads `message.tool_calls`.
`OllamaProvider` streams pure `content` text; the shared `fantastic-ai-core` layer
teaches the `send` tool's text envelope in the system prompt and parses the call back
out of the stream (`tool_parse`): the model emits
`<tool_call>{"name":"send","arguments":{"target_id":"…","payload":{"type":"…"}}}</tool_call>`,
results return as `<tool_response>…</tool_response>` text. Model-agnostic — works even
with models ollama marks `completion`-only. Identical across the ollama / nvidia /
anthropic backends.

## Calling this agent as a workflow unit
`send {type:"send", text, client_id?}` runs ONE inference turn (per-backend FIFO
lock). It is SHARED COMPUTE reached by id: a scheduler, a host job, a
peer kernel, or another AI all `send` to it by id — threads are kept
per-`client_id`. Optional `system_prompt`/`messages` override the
auto-built prompt/history (stateless mode).

Verbs: `send` · `history` · `interrupt` · `status` ·
`refresh_menu` · `reflect` · `boot` · `shutdown`.

## Streaming routing
This backend uses `CallerRoute::CliRoundTrip`: when `client_id` is
`"cli"`, stream events are sent to the `cli` agent directly
(`kernel.send("cli", ev)`). For all other `client_id` values, events
are emitted on the backend agent's own inbox (`kernel.emit(self_id,
ev)`) tagged with `client_id` — only a watcher of the backend id who
supplies that `client_id` sees them.

## Routing is the model's decision
HOW a completion reaches its addressee is the model's: the per-call prompt names
who listens (possibly many); the system prompt carries the `send` signature; the
model `send()`s a named listener or just answers — no `reply_to`. A reserved
`_call_stack` guard refuses cycles / depth>8 before the lock (AI→AI can't deadlock).
