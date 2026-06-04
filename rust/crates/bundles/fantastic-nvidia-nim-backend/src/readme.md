# nvidia_nim_backend — NVIDIA NIM LLM agent
OpenAI-compatible LLM backend. Thin Provider + Bundle over
`fantastic-ai-core`: this crate supplies only the NIM `Provider`
(HTTPS + Bearer auth + SSE + per-index tool-call argument aggregation
+ 429 rate-limit retry) and a thin `Bundle` that dispatches every verb
through `fantastic-ai-core`. The FIFO lock, menu cache, prompt
assembly, agentic loop, and history persistence all live in
`fantastic-ai-core`.

This crate implements the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. The provider connection differs
(HTTPS + Bearer auth + SSE) but verbs and emitted events are
byte-for-byte identical, so any client can swap providers by changing
one `handler_module` field on the agent's record — no client-side
change.

**Streaming routing**: this backend uses per-client-inbox routing
(`CallerRoute::PerClientInbox`). Every stream event is emitted on the
CALLER's own `client_id` inbox — NOT the backend agent's own inbox.
A watcher of the backend agent's id does NOT see these events.

Verbs: `send` · `history` · `interrupt` · `status` · `refresh_menu` ·
`boot` · `shutdown` · `reflect` · `set_api_key` · `clear_api_key`.

Extras beyond the contract:

- `set_api_key {api_key}` — persists `api_key` to
  `<file_agent.root>/.fantastic/agents/{id}/api_key` via the file
  agent. Drops the cached HTTP client so the next `send` reads fresh.
- `clear_api_key` — deletes the sidecar via the file agent.
- `reflect` reports `has_api_key: bool` only — never the value.

On HTTP 429 BEFORE any chunk has been yielded, the bundle retries
once after sleeping `min(60s, max(1s, Retry-After || 5s))`. A `say`
event + `status(thinking, waiting_on=rate_limit)` event surface the
wait via the caller's inbox. Mid-stream 429 (rare — quota usually
checked up front) propagates as an error.
