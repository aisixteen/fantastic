> Setting up an AI agent: the backend's record schema, keys in the credential store, templates — the human's choices.

# Setting up an AI agent

Nothing is configured by default and nothing is guessed: the backend, model,
endpoint, host and port are the HUMAN's choice. An agent asked to set one up
ASKS which they want.

## Where it is done

- The terminal UI: the tree's last row, `+ add an ai agent`.
- The canvas: ⚙ AI set-up (or the action popup's `configure AI…`) — the same
  page as a cell, through the host's `setup` agent.

Both draw one page from the backend's RECORD SCHEMA: `id`, `handler` (every `*_backend.tools` this build links),
the backend's required fields, `may call`, then every optional field at the
backend's own default. Only a field changed from its default is written.

## The record

A write admits the new agent to the memory store `memory`, creates it under
`user` (`memory_store_id: memory`, `callable_by {user}`, `can_call`
its targets and `memory`), grants both sides, boots and flushes it.

## Keys

- Kept in the kernel's credential store, the `credentials` agent
  (`reflect credentials`). Where it keeps them is the human's choice, with no
  default: **plain** — one 0600 file, `<home>/agents/ai_agent/keys/credentials.yaml` (not secured);
  **secure** — the keychain. `<home>` is `FANTASTIC_HOME`, else
  `~/.config/fantastic`. Until a choice is made there is no `credentials`
  agent.
- The home, laid out:

  ```
  <home>/
  ├── agents/ai_agent/templates/   saved set-ups (+ .preferred, the default)
  ├── agents/ai_agent/keys/        mode, refs.json (refs, no values), credentials.yaml (plain mode)
  └── fantastics/                  stored fantastic locations (the user store)
  ```
- A key is a REF `<provider>/<host>` (`responses/api.openai.com`) bound to its
  endpoint's host; the store refuses to hand it to any other host.
- An AI agent names its ref on its record — `credentials: {store:
  credentials, ref}` — and its backend reads it before every request: a stored
  or rotated key is live on the next call, with no restart.
- Ways in: typed in a set-up surface (`k` in the terminal UI; the key field in
  the canvas cell), or from a shell with `fantastic send` and `$env:NAME`
  (`fantastic readme mcp`). `set` only stores a value for a DECLARED ref: a
  new ref is declared first with `update_agent` on `credentials`, its `refs`
  map (`{<ref>: {host, readers, writers}}`, written whole), naming the caller
  in its `writers`.
- The store refuses an unmarked `set` / `get`; every value travels with a
  `secret` mark, so no row, ledger, reflect or stream holds it. An AI model
  never calls the store (`holds_secrets: true`).

## Templates

A record can be saved as a named template (no id, key or grants in it) in
`<home>`, loaded back into the page, and one marked the default — the canvas's
action popup spawns AI cells from them.
