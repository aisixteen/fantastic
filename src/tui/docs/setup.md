> Setting up an AI agent: the backend's record schema, keys in the credential store, saved set-ups — the human's choices.

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
  default: **plain** — one 0600 file, `<home>/user/keys/credentials.yaml` (not secured);
  **secure** — the keychain. `<home>` is `FANTASTIC_HOME`, else
  `~/.fantastic`. Until a choice is made there is no `credentials`
  agent.
- The home, laid out:

  ```
  <home>/
  ├── user/setups/ai_agent/        saved set-ups (+ .preferred, the default)
  ├── user/keys/                   mode, refs.json (refs, no values), credentials.yaml (plain mode)
  ├── user/credentials/            a credential_store's default file, <store id>.yaml
  └── fantastics/                  stored fantastic locations (the user store)
  ```

  A home from an older layout (`~/.config/fantastic`, `~/.fantastic/credentials`)
  is moved here by hand: `fantastic migrate-home` (`--dry-run` prints the moves).
  A fantastic never runs with the home as its `.fantastic`: `fantastic` in `~` is
  refused.

  ```
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
  map (`{<ref>: {host}}`, written whole). There are no per-ref access lists:
  any caller the kernel's call gate admits may `set`, `get` and `forget` a
  declared ref.
- The store's record holds only metadata (the host and the field names). The
  value travels in the call: a `set` payload and a `get` reply are recorded
  like any call's (the ledger, the memory store, the state stream), in clear.

## Saved set-ups

The AI agent's own data in its user folder: a record saved under a name
(`<home>/user/setups/ai_agent/<name>.yaml` — an AI backend's `handler_module`
and its config, no id, key value or grants), loaded back into a set-up page, and
one marked the default (`.preferred`). The double-click's `AI agent` makes a new
AI agent from the default; another one is the AI cell's ⚙ settings → `‹ other
set-ups`. A saved record on a handler that is not an AI is refused. Only the human
saves them (a set-up page, or a file there): the home store's grants admit `user` alone.
