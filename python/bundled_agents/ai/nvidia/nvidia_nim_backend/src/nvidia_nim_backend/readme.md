# nvidia_nim_backend — NVIDIA NIM LLM agent
OpenAI-compatible LLM backend. api_key stored out-of-band via `file_agent_id` sidecar; rate-limit retry. Same surface as ollama_backend.

## Implementation
This bundle is a thin binding over the shared `ai_core` lib. The queue/FIFO lock/menu cache, prompt assembly, the agentic `_run` loop, and all verb bodies live in `ai_core.core`; this module supplies the `NvidiaNimProvider` builder and the NIM-specific extras below, and `ai_core.build()` wires the rest into `(VERBS, handler)`.

## NIM-specific verbs
- `set_api_key {api_key}` — persists the key to `.fantastic/agents/<id>/api_key` via `file_agent_id` (never in agent.json). Drops the cached provider so the next `send` reads the key fresh. Failfast if `file_agent_id` is unset or key is empty.
- `clear_api_key {}` — deletes the api_key sidecar via `file_agent_id` and drops the cached provider. Returns `{ok:true, deleted:bool}`.

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

## Wiring your completion — routing is YOUR decision, not plumbed
You are a workflow unit: you receive a call, think (calling other agents as tools
via `send`), and stream `token`/`status`/`done` on your OWN id. HOW your result
reaches its addressee is your decision:
- the per-call PROMPT names WHO listens for your completion — the addressee id(s),
  and there may be MANY;
- your SYSTEM PROMPT always carries the `send` call() signature.
Given those, route your own output: `send()` to a named listener, or simply answer
— any caller that `watch`es your id consumes the same stream. There is no
`reply_to`; a capable model routes itself, to one addressee or several.

## Recursion guard
Tool-calls propagate a reserved `_call_stack`. A call that re-enters an agent
already in the chain (a cycle) or exceeds depth 8 is refused BEFORE the FIFO lock
— so AI→AI chains can neither deadlock nor run away. Don't set `_call_stack`
yourself.

## Meta-possibility — any routine orchestrates the whole substrate
Every routine (this AI, a host job, a peer kernel, or any out-of-process caller)
reaches every agent by id. From any of them you can read memory anywhere (`send(<state>,
{read})`), run an inference turn (`send(<ai>, {send, ...})`), and/or spawn compute
(`send(<py>, {start, code})`) — regardless of which kernel owns the target. Code
steps and LLM-call steps are interchangeable, both directions.
