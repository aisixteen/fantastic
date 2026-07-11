# nvidia_nim_backend — NVIDIA NIM, a context-managed completion agent
OpenAI-compatible LLM backend. api_key stored out-of-band via `file_bridge_id` sidecar; rate-limit retry; automatic context management. **No agentic loop** — one provider pass per `send`. Same surface as ollama_backend.

## Implementation
This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `NvidiaNimProvider` builder and the NIM-specific extras below, and `ai_core.build()` wires the rest into `(VERBS, handler)`.

## Bare completion — the tool loop is the CALLER's
This backend does NOT parse tool calls and does NOT loop. `send` performs exactly ONE
provider pass. It NEVER sends the OpenAI `tools`/`tool_choice` fields nor reads
`delta.tool_calls`; the `NvidiaNimProvider` streams plain `content` text, returned
verbatim. Any tool convention is the caller's — an authored agent teaches it in the
prompt (via `system_prompt`), reads the completion, dispatches, and feeds the result
back as the next `{send, text}` turn.

## Context management — owned HERE
Like ollama_backend: per-client `chat.json` history, auto-compaction on overflow
(`context_strategy`, default `compact`, + `[context-notice]`), and the `recall` /
`context_status` verbs. The conversation's owner owns its context.

## NIM-specific verbs
- `set_api_key {api_key}` — persists the key to the store-relative `agents/<id>/api_key` via `file_bridge_id` (wire it to the `.fantastic` store; never in agent.json; no `.fantastic/.fantastic/…` double-nest). Drops the cached provider so the next `send` reads the key fresh. Failfast if `file_bridge_id` is unset or key is empty.
- `clear_api_key {}` — deletes the api_key sidecar via `file_bridge_id` and drops the cached provider. Returns `{ok:true, deleted:bool}`.

`reflect` reports `has_api_key:bool` (never the key value itself).

## Rate-limit and failfast behavior
- **No key → failfast**: if no api_key is set, `send` returns an error immediately (`"api_key not set; call set_api_key first"`). The provider builder returns `None` when no key is present, and `ai_core` with `require_provider=True` surfaces this as a clean caller-facing error.
- **HTTP 429 → retry once**: on a rate-limit response received before any chunk is yielded, the bundle waits `Retry-After` seconds (clamped to [1, 60]; default 5s if the header is absent) and retries the request once. Mid-stream 429 or any non-429 HTTP error propagates unchanged.

## Calling this agent as a workflow unit
`send {type:'send', text, client_id?}` runs ONE inference turn (per-backend FIFO
lock) and returns `{response, final}`. It is a first-class peer of
`python_runtime`: a scheduler, a host job, a peer kernel, or another AI can
call it by id. Optional `system_prompt:str` REPLACES the auto-built prompt
(caller-supplied role/context — read it from a state agent yourself; the AI has NO
yaml coupling). Optional `messages:list` REPLACES persisted history (fully
stateless — though `api_key` is still required to reach the provider).

## Streaming
You stream `token`/`status`/`done` on your OWN id. Any caller that `watch`es your id
consumes the same stream (the driving caller, a browser tab filtering by `client_id`, a
peer). You do not route your own output — you complete; the caller reads it and decides
what to do next.

## Recursion guard
A `send` carrying a reserved `_call_stack` that already contains this agent (a cycle)
or exceeds depth 8 is refused BEFORE the FIFO lock — so an authored agent's AI→AI chain can
neither deadlock nor run away. Don't set `_call_stack` yourself.

## Meta-possibility — any routine orchestrates the whole substrate
Every routine (this AI, a host job, a peer kernel, or any out-of-process caller)
reaches every agent by id. From any of them you can read memory anywhere (`send(<state>,
{read})`), run an inference turn (`send(<ai>, {send, ...})`), and/or spawn compute
(`send(<py>, {start, code})`) — regardless of which kernel owns the target. Code
steps and LLM-call steps are interchangeable, both directions.
