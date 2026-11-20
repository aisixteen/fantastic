# `shared_provider` (E163) — two chats on one provider

Free: no model. Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) answer for ONE
`vllm_backend` provider; two `ai_chat.tools` chats name it (#632). Rust lane only
(`runtimes: [rust]`), driven over `web_rest` on the packaged host.

1. **admitted** — the creator (the test's root) names the provider for each chat and admits it
   on the provider's `callable_by` with `update_agent`: both chats are there.
2. **own knobs** — each chat's request carries its own `temperature` (0.3 and 0.9), `max_tokens`
   (8192 and 4096) and, for one, `top_k`; the provider's record holds none of them.
3. **KV accounting** — the provider's `reflect` shows both conversations live on its endpoint.
4. **lifecycle** — the provider's `endpoint` changed to a second server: the next request of a
   chat on it goes there; both chats deleted, the provider stays.

```sh
RUNTIME=rust uv run python e2e/tests/kernel/E163_shared_provider/probe.py
```
