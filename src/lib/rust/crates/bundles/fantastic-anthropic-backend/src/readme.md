# anthropic_backend — Anthropic (Claude) LLM agent
Anthropic Messages-API LLM backend. Thin Provider + Bundle over
`fantastic-ai-core`: this crate supplies only the Anthropic `Provider`
(HTTPS + `x-api-key` auth + event-typed SSE streaming raw text + 429
rate-limit retry) and a thin `Bundle` that dispatches every verb
through `fantastic-ai-core`. The FIFO lock, prompt assembly, ONE-PASS
generation, tool-call parsing + reply routing, context management, and history
all live in `fantastic-ai-core`. **No loop.**

This crate implements the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. Verbs + events are identical across
backends, so a caller retargets by changing one id.

## ONE pass — and the model routes its own output
`send` does exactly ONE provider pass. **No loop**: nothing iterates on the
model's behalf, and emitting no `<tool_call>` ends the turn — stop is the
default and costs nothing.

Tool-calling is RAW and belongs to `fantastic-ai-core`. By default this backend
sends no Anthropic `tools`/`input_schema`: the model writes the envelope as TEXT,
the Anthropic `Provider` streams that text through as plain `text_delta` content
knowing nothing about it, and ai-core parses and dispatches it. Wire translation
is just the `system` role → Anthropic's top-level `system` param + alternating
user/assistant TEXT turns.

**Optional `tool_use` route.** Set `tool_use: true` on the record and this
provider declares the ONE `send` tool and TRANSLATES the `tool_use` blocks it
receives back into exactly the same `<tool_call>` text envelope — arguments are
accumulated from `input_json_delta` fragments and re-emitted when the block
closes. It is off by default and changes nothing else: ai-core's parser never
learns a vendor dialect, so RAW stays the contract and the translation lives at
the edge. Use it only for an endpoint that answers better in its native syntax.

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
Per-client `chat.json` history, auto-compaction on overflow, and the `recall` /
`context_status` verbs. The conversation's owner owns its context.

**Streaming routing**: this backend uses per-client-inbox routing
(`CallerRoute::PerClientInbox`). Every stream event is emitted on the
CALLER's own `client_id` inbox — NOT the backend agent's own inbox.
A watcher of the backend agent's id does NOT see these events.

Verbs: `send` · `history` · `recall` · `context_status` ·
`status` · `boot` · `shutdown` ·
`reflect` · `set_api_key` · `clear_api_key`.

There is NO `interrupt` verb: an in-flight `send` IS a kernel task. Cancel a
turn with the substrate's universal system verbs — `send "kernel" {type:"ps"}`,
find the row `{agent:<this id>, verb:"send"}`, then `send "kernel"
{type:"cancel", task_id:"<agent>:<n>"}`. The turn closes with
`status(phase="done", reason="cancelled")` + `done`, the HTTP stream and the
FIFO lock are released, and the blocked caller's `send` resolves to
`{cancelled:true}`.

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
