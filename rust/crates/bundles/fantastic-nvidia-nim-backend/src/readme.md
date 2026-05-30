# nvidia_nim_backend — NVIDIA NIM LLM agent
OpenAI-compatible LLM backend. api_key stored out-of-band via `file_agent_id` sidecar; rate-limit retry. Same surface as ollama_backend.

This crate implements the **LLM backend contract** documented in
`fantastic-ollama-backend`. Transport differs (HTTPS + Bearer auth +
SSE + per-index tool-call argument aggregation + 429 rate-limit retry)
but verbs and emitted events are byte-for-byte identical so the chat
UI (`fantastic-ai-chat-webapp`) can swap providers by changing one
`handler_module` field on the agent's record.

Extras beyond the contract:

- `set_api_key {api_key}` — persists `api_key` to
  `<file_agent.root>/.fantastic/agents/{id}/api_key` via the file
  agent. Drops the cached HTTP client so the next `send` reads fresh.
- `clear_api_key` — deletes the sidecar via the file agent.
- `reflect` reports `has_api_key: bool` only — never the value.

On HTTP 429 BEFORE any chunk has been yielded, the bundle retries
once after sleeping `min(60s, max(1s, Retry-After || 5s))`. A `say`
event + `status(thinking, waiting_on=rate_limit)` event surface the
wait so the chat UI can pulse "waiting on provider". Mid-stream 429
(rare — quota usually checked up front) propagates as an error.
