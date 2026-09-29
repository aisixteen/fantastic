# llamacpp_backend — local LLM via `llama-server`, a context-managed completion agent

Talks to a local `llama-server` (llama.cpp) over its OpenAI-compatible surface.
Per-client chat threads, FIFO lock, and automatic context management (compaction /
recall / context_status). Conversation in its memory store (`memory_store_id`). **No agentic loop** —
one provider pass per `send`.

## Implementation
A thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt
assembly, the ONE-PASS `_run`, and all the context machinery live in
`ai_core.core`; this module supplies the `LlamaCppProvider` builder (endpoint /
model / sampling knobs from the agent record) and `ai_core.build()` wires the rest
into `(VERBS, handler)`.

Separate from `nvidia_nim_backend` even though both speak the OpenAI wire: a
`reflect` describing a local model as an NVIDIA NIM agent would put a lie in the
tree, and this system's whole premise is that the tree describes itself truthfully.

## Why not the ollama backend
`llama-server` mounts the OpenAI surface and nothing else — `/api/chat`,
`/api/tags`, `/api/generate` and `/api/show` all return 404, and the ollama
backend's provider builds on the `ollama` SDK, which posts to `/api/chat`. Pointing
its `endpoint` at a llama-server just posts into a 404.

## The thinking channel — the reason this bundle exists
A reasoning model splits its output. Started with `--reasoning-format deepseek`,
`llama-server` streams the thought channel as `reasoning_content` and only the
conclusion as `content`. Reading `content` alone silently drops whole turns: the
model spends a pass reasoning, you observe an EMPTY completion, and the chain dies
with no error anywhere. This provider reads BOTH and wraps the thought channel once
in `<think>…</think>` — parsed for tool calls, stripped before the turn is
persisted (`ai_core.core.without_thinking`).

`gemma-4` additionally GATES that channel behind a template kwarg
`enable_thinking`, **default false**, and emits `<|channel>thought … <channel|>`
rather than `<think>` tags. Set `think` on the record and this backend sends
`chat_template_kwargs: {"enable_thinking": …}` per request — so toggling thinking
is a record edit, not a server restart.

## Server side — the operator's job, not this agent's
Nothing here starts or downloads anything. Run the server consciously:

```sh
LLAMA_CACHE="$HOME/Projects/models" llama-server \
  -hf <org>/<repo>:<quant> --host 127.0.0.1 --port 8080 \
  -c 16384 -ngl 99 --jinja \
  --chat-template-kwargs '{"enable_thinking":true}' \
  --reasoning-format deepseek
```

`--reasoning-format` decides where thoughts land: `deepseek` →
`message.reasoning_content` (what this provider reads); `deepseek-legacy` → both
inline and in the field; `none` → unparsed inline in `content`.

## Record fields
- `endpoint` — REQUIRED: the llama-server's base URL (its bind + `/v1`); nothing is guessed.
- `model` — usually omit it. `llama-server` serves exactly ONE model, whichever
  `-hf`/`-m` loaded, and ignores the request's `model` field; when unset the key is
  not sent at all. Set it only when a proxy in front of the server routes on it.
- `api_key` — llama-server is KEYLESS unless started with `--api-key`. Absent ⇒ no
  `Authorization` header is sent; none is invented.
- `temperature` — default 0.3 (an agentic backend must emit exact tool-call JSON,
  not improvise).
- `num_predict` — generation cap (`max_tokens`). Unset ⇒ the server's default.
- `think` — `enable_thinking` per request. Unset ⇒ the kwarg is not sent (a model
  without a thinking gate rejects an unknown kwarg).

There is **no `num_ctx`**: the context window is fixed at server start (`-c N`) and
has no per-request equivalent, so this agent does not pretend to own one.

## Context management — owned HERE
The conversation lives in this agent's MEMORY STORE — `memory_store_id` on the record
is REQUIRED (a `send` fails loud naming it): every turn, fold and view is a row under
this agent's key. Reply BODIES are never stored — a reply row keeps its one-line
`origin`, and after a restart the model re-reads from it. Compaction fires at 0.8 of
the budget (calibrated by the endpoint's reported prompt size when it reports one)
with the per-agent `context_strategy` (default `compact`; `fold`, `truncate`) and a
`[context-notice]`; `context_status`, `context_recall`, `history {fold?}`, `grep` and
`context_clear` (a boundary, nothing deleted) inspect and page it. Sharing is by
GRANT, by the owner's own choice: it `share`s a scope of its rows through the store;
another agent starts from it with `fork_from {owner, grant, message}` and asks for a
third agent's row with `request_share`. `watch {body?}` / `unwatch` subscribe to this
agent's row events (a digest; bodies only for a watcher the rows are shared with).

## Calling this agent
`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. A first-class peer of `exec_bridge`: a
scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
`system_prompt:str` REPLACES the default system block. There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text, or by grant (`share` → `fork_from`).

An in-flight `send` is a kernel TASK: kill it with the universal `ps` → `cancel`
system verbs. There is no `interrupt` verb.
