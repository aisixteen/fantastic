> Durable notes are FILES on a gated surface; every MESSAGE lives in a MEMORY STORE agent.

# Memory — durable state as files, everywhere

Memory here is not a special store: it is documents you write. The substrate
imposes NO format — keep your memory as markdown, JSON, plain notes, source
code, whatever fits the way you think. What makes it memory is only that you
wrote it down and can find it again.

Two agents carry it:

- A `file_bridge` agent is the SURFACE — a directory served as an agent, gated
  and clamped. Raw bytes ride its `read_stream`/`write_stream`.
- A `text_file` agent is the EDITOR — line-numbered `read`, atomic `write`,
  surgical `patch` (SEARCH/REPLACE blocks), `append`, and ON DEMAND a
  `version` hash with an `if_match` guard, plus `checkpoint`/`restore` when a
  change is worth a pre-image. Point it at any surface with `on:` (or its
  `target_id` default).

The practice: keep one file per concern, named for what it holds
(`notes.md`, `decisions.md`, `state.json`). `append` for logs, `patch` for
precise updates, `read` to recall. A fact worth keeping is a line you write;
pruning is a patch that removes it. Structure is YOURS — a key-value shape, a
diary, a table — the medium does not care.

Wiring is the operator's, both ends as always: your `can_call` names the
text_file agent and the surface; their `callable_by` accepts you. An unwired
editor fails loud naming `target_id` — nothing persists silently.

## The memory store — where every message lives

The kernel keeps NO conversation. Every message — an AI agent's turns, folds and
views, and the kernel's own execution rows (`call`, `settle`, `ack`, `flush`) — is
a ROW in a MEMORY STORE agent (`ram_store` ships with every kernel: in RAM, gone
with the process; a durable store answers the same verbs). A store is
multi-tenant: every row sits under its owner's key `<kernel guid>|<agent id>`;
a caller reads and writes only its own key, plus what a grant covers.

- The ROOT names the kernel's store in `memory_store_id` — `kernel_start` refuses
  without it ("create a ram_store and name it in memory_store_id"). The kernel
  flushes its execution rows there; `ledger`, dead letters and named-call dedup
  read them back.
- Each AI agent names its own `memory_store_id` and needs `can_call` on the store
  plus the store's `callable_by` for it — the creator writes both; no defaults.
  Reply BODIES are never stored: a reply keeps a one-line `origin` saying how to
  get it again.
- Verbs: `append`, `read`, `search`, `prune` (your own rows); `share` /
  `unshare` / `grants` (a grant covers `head_until`, `range`, `fold` or `row` of
  YOUR rows only, `frozen` or `live`); `inject` / `dump` (the whole tables — for
  the store's `admins`).
- Context moves between agents only by grant: the owner's model `share`s, the
  reader starts from it with `fork_from {owner, grant, message}` on its AI agent.
  A third agent's row inside a shared scope shows as a stub; ask its owner with
  `request_share`.

Files and the store are different tools: files are what you choose to write
down; the store is the record of what was said and done.
