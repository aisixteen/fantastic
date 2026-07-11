# ollama_backend — local LLM, a context-managed completion agent
Thin Provider + Bundle over `fantastic-ai-core`: this crate supplies only the
`OllamaProvider` (NDJSON transport to a local ollama process) and a thin
`Bundle`. The per-client chat threads, FIFO lock, prompt assembly, ONE-PASS
generation, and context management all live in `fantastic-ai-core`.

Reference implementation for the **LLM backend contract** in `fantastic-ai-core`'s
module header. Every backend speaks it, so any caller retargets by changing one id.

## Bare completion — the tool loop is the CALLER's
`send` does exactly ONE provider pass: it streams the model's pure text and
returns it. **No agentic loop, no tool parsing.** `OllamaProvider` never sends
ollama's `tools` array nor reads `message.tool_calls`. Any tool convention is the
caller's — an authored agent teaches it in the prompt (via `system_prompt`),
reads the completion, dispatches, and feeds the result back as the next
`{send, text}` turn.

## Context management — owned HERE
Because this backend owns the per-client conversation, it owns the context:
per-client `chat.json` history, auto-compaction on overflow (`context_strategy`,
default `compact`, + `[context-notice]`), and the `recall` / `context_status` verbs.
The durable transcript is never trimmed. Requires `file_bridge_id` for the store.

Verbs: `send` (args `text`, `client_id?`, `system_prompt?`) · `history` · `recall` ·
`context_status` · `interrupt` · `status` · `reflect` · `boot` · `shutdown`.

## Streaming routing
`CallerRoute::CliRoundTrip`: `client_id == "cli"` → the `cli` agent; else emitted on
the backend's own inbox tagged with `client_id`. Events: `status`
(queued|thinking|streaming|done) · `token` · `done` · `context` (compaction).
