# `chat_provider_split` (E150) — a chat and its provider apart

Free: no model. Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) answer for
`vllm_backend` PROVIDERS; the chat is `ai_chat.tools`, its `provider_id` naming one. Rust lane
only (`runtimes: [rust]`), driven over `web_rest` on the packaged host.

1. **local split** — the chat answers `send`; its record names the provider and holds no
   `endpoint`; its `history` holds the transcript; the provider's holds none.
2. **swap** — `update_agent` the chat's `provider_id` to a second provider (another server): the
   next turn reaches the second server and its request carries the earlier rows.
3. **grants** — a chat with no `complete` / `watch` on its provider, and a provider that does not
   admit the chat, are each refused by the provider's name (`cannot watch provider …`).
4. **across the bridge** — the provider lives on a second kernel that dials the first through a
   `ws_bridge` (as `E118_kernel_bridges`); the chat names it `<guid>|<id>` and completes there.

```sh
RUNTIME=rust uv run python e2e/tests/kernel/E150_chat_provider_split/probe.py
```
