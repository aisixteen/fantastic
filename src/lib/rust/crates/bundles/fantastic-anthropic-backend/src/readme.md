# anthropic_backend — Anthropic (Claude) LLM agent
Anthropic Messages-API LLM backend. Thin Provider + Bundle over
`fantastic-ai-core`: this crate supplies only the Anthropic `Provider`
(HTTPS + `x-api-key` auth + event-typed SSE + `tool_use` block
aggregation + OpenAI↔Anthropic message/tool translation + 429
rate-limit retry) and a thin `Bundle` that dispatches every verb
through `fantastic-ai-core`. The FIFO lock, menu cache, prompt
assembly, agentic loop, and history persistence all live in
`fantastic-ai-core`.

This crate implements the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. The provider connection differs
(HTTPS + `x-api-key` + `anthropic-version` header + event-typed SSE)
but verbs and emitted events are byte-for-byte identical, so any
client can swap providers by changing one `handler_module` field on
the agent's record — no client-side change.

ai-core hands the provider OpenAI-shaped messages + the universal
`send` tool. This backend TRANSLATES on the wire: the `system` role
becomes Anthropic's top-level `system` param; `assistant`
`tool_calls` become `tool_use` content blocks; `role:tool` results
become a `user` message with a `tool_result` block; and the
`{type:function, function:{…}}` tool becomes `{name, description,
input_schema}`. The reverse — Anthropic's `content_block_delta`
(`text_delta` / `input_json_delta`) + `tool_use` blocks — is
aggregated back into ai-core's finalized `ProviderEvent`s.

**Streaming routing**: this backend uses per-client-inbox routing
(`CallerRoute::PerClientInbox`). Every stream event is emitted on the
CALLER's own `client_id` inbox — NOT the backend agent's own inbox.
A watcher of the backend agent's id does NOT see these events.

Verbs: `send` · `history` · `recall` · `context_status` ·
`interrupt` · `status` · `refresh_menu` · `boot` · `shutdown` ·
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
