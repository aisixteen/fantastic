> Durable notes are FILES on a gated surface; every MESSAGE lives in a MEMORY STORE agent.

# Memory — durable state as files, everywhere

Memory here is not a special store: it is documents you write. The substrate
imposes NO format — keep your memory as markdown, JSON, plain notes, source
code, whatever fits the way you think. What makes it memory is only that you
wrote it down and can find it again.

One agent carries it — a `file_bridge`: a directory served as an agent, gated,
clamped, and deny-all (its `allow` list names what it exposes). Its text verbs are
the editor: line-numbered `read` with a `version` hash, atomic `write`,
SEARCH/REPLACE `edit` (a transaction over one or more files), `append`, an
`if_match` guard on any write, and `undo` (one slot per path, in RAM). Raw bytes
ride its `read_stream`/`write_stream`.

The practice: keep one file per concern, named for what it holds
(`notes.md`, `decisions.md`, `state.json`). `append` for logs, `edit` for
precise updates, `read` to recall. A fact worth keeping is a line you write;
pruning is an edit that removes it. Structure is YOURS — a key-value shape, a
diary, a table — the medium does not care.

Keep notes in a DATA store, not the records store (`.fantastic` holds every
agent's record and grants):

    fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools \
        id=notes root=.fantastic/data allow='["."]' ingress_rule=allow_all_ingress

Wiring is the operator's, both ends as always: your `can_call` names the bridge;
its `callable_by` accepts you. Nothing persists silently — an unwired or unexposed
path is refused.

## The memory store — where every message lives

The kernel keeps NO conversation. Every message — an AI agent's turns, folds and
views, and the kernel's own execution rows (`call`, `settle`, `ack`, `flush`) — is
a ROW in a MEMORY STORE agent (`ram_store` ships with every kernel: in RAM, gone
with the process; `sqlite_store` answers the same verbs from one SQLite file and
survives a restart). A store is
multi-tenant: every row sits under its owner's key `<kernel guid>|<agent id>`;
a caller reads and writes only its own key.

- The ROOT names the kernel's store in `memory_store_id` — `kernel_start` refuses
  without it ("create a ram_store and name it in memory_store_id"). The kernel
  flushes its execution rows there; `ledger`, dead letters and named-call dedup
  read them back.
- Each AI agent names its own `memory_store_id` and needs `can_call` on the store
  plus the store's `callable_by` for it — the creator writes both; no defaults.
  Reply BODIES are never stored: a reply keeps a one-line `origin` saying how to
  get it again.
- Verbs: `append`, `read`, `search`, `prune` (your own rows); `inject` / `dump`
  (the whole tables). Who may call any of them is the call gate alone.
- Context moves between agents in the text of a call.

Files and the store are different tools: files are what you choose to write
down; the store is the record of what was said and done.
