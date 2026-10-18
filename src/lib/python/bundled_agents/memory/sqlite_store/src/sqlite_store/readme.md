# sqlite_store — durable memory

A MEMORY STORE agent: conversation rows for many owners, folds, search — kept in
ONE SQLite file (`path` on the record, relative to the project dir, required).
The tables survive the process: a restarted kernel reads its ledger, dead
letters, named-call dedup and every AI agent's memory straight back.

The SAME verbs, arguments, replies and refusals as `ram_store` — a caller cannot
tell the two apart except that this one is `durable: true`.

**Owner = caller.** Every row is keyed by who wrote it — `<kernel guid>|<agent id>`,
`<kernel guid>|kernel` for the operator and the kernel's own bookkeeping. A caller
writes and reads its OWN rows; there is no `owner` field to forge.

| verb | what |
|---|---|
| `append {rows}` | add rows under your key, all or none; ids are the kernel's |
| `read {after?, limit?, kinds?, match?, from_last_kind?, order?}` | your rows |
| `search {query, ids?}` | the LINES that mention something (FTS5 trigram index, substring semantics) |
| `prune {before_id \| ids, keep?}` | delete your rows; fold refs on them go too |
| `inject {tables}` · `dump` | replace / capture the whole store (a `ram_store` dump injects here and back) |

**Access:** a caller reads and writes its own rows. Who may call any verb —
`inject`/`dump` included — is the kernel's call gate alone.

**The tables are real:** rows `(owner, id)`, ref_lists + fold_refs — every
foreign key `ON DELETE CASCADE` with `PRAGMA foreign_keys=ON` — and an FTS5 text
index. The file runs in WAL mode with `synchronous=NORMAL` and a busy timeout.
`delete_agent` deletes the file with its `-wal` and `-shm` (as ram_store's tables
die with the agent); a kernel stop keeps it, checkpointing the WAL on close.

**Use it as the kernel's store:** create it under the root with a `path`, then name
it in the root's `memory_store_id`:

```bash
fantastic kernel create_agent parent_id=kernel_state handler_module=sqlite_store.tools id=memory path=.fantastic/memory.db
fantastic kernel update_agent id=kernel_state memory_store_id=memory
```
