# llamacpp_backend — local LLM via `llama-server`, a context-managed completion agent

Talks to a local `llama-server` (llama.cpp) over its OpenAI-compatible surface.
Per-client chat threads, FIFO lock, and automatic context management (compaction /
recall / context_status). Persistence via `file_bridge_id`. **No agentic loop** —
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
- `endpoint` — default `http://127.0.0.1:8080/v1` (llama-server's own bind + path).
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

## Calling this agent
`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. A first-class peer of `exec_bridge`: a
scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
`system_prompt:str` REPLACES the default system block. Optional `messages:list`
REPLACES persisted history (fully stateless — no `file_bridge_id` needed, and no
compaction — the caller owns the context in that mode).

An in-flight `send` is a kernel TASK: kill it with the universal `ps` → `cancel`
system verbs. There is no `interrupt` verb.
