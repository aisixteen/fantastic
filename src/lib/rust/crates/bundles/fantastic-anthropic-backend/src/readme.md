# anthropic_backend — Anthropic (Claude) LLM agent
Anthropic Messages-API LLM backend. Thin Provider + Bundle over
`fantastic-ai-core`: this crate supplies only the Anthropic `Provider`
(HTTPS + `x-api-key` auth + event-typed SSE streaming raw text + 429
rate-limit retry) and a thin `Bundle` that dispatches every verb
through `fantastic-ai-core`. The FIFO lock, prompt assembly, ONE-PASS
generation, context management, and history all live in
`fantastic-ai-core`. **No agentic loop.**

This crate implements the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. Verbs + events are identical across
backends, so a caller retargets by changing one id.

## Bare completion — the tool loop is the CALLER's
`send` does exactly ONE provider pass. This backend NEVER sends Anthropic
`tools`/`input_schema` nor reads `tool_use` blocks; the Anthropic `Provider`
streams plain `text_delta` content, returned verbatim. Wire translation is just
the `system` role → Anthropic's top-level `system` param + alternating
user/assistant TEXT turns. Any tool convention is the caller's — an authored
authored agent teaches it via `system_prompt`, reads the completion, dispatches, and
feeds the result back as the next `{send, text}` turn.

## Context management — owned HERE
Per-client `chat.json` history, auto-compaction on overflow, and the `recall` /
`context_status` verbs. The conversation's owner owns its context.

**Streaming routing**: this backend uses per-client-inbox routing
(`CallerRoute::PerClientInbox`). Every stream event is emitted on the
CALLER's own `client_id` inbox — NOT the backend agent's own inbox.
A watcher of the backend agent's id does NOT see these events.

Verbs: `send` · `history` · `recall` · `context_status` ·
`interrupt` · `status` · `boot` · `shutdown` ·
`reflect` · `set_api_key` · `clear_api_key`.

Extras beyond the contract:

- `set_api_key {api_key}` — persists `api_key` to the store-relative
  `agents/{id}/api_key` (wire `file_bridge_id` to the `.fantastic` store) via the
  file agent. Drops the cached HTTP client so the next `send` reads fresh.
- `clear_api_key` — deletes the sidecar via the file agent.
- `reflect` reports `has_api_key: bool` only — never the value.

On HTTP 429 BEFORE any chunk has been yielded, the bundle retries
once after sleeping `min(60s, max(1s, Retry-After || 5s))`. A `say`
event + `status(thinking, waiting_on=rate_limit)` event surface the
wait via the caller's inbox. Mid-stream 429 propagates as an error.
