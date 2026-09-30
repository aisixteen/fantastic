# sqlite_store — durable memory

A MEMORY STORE agent: conversation rows for many owners, folds, grants, search —
kept in ONE SQLite file (`path` on the record, relative to the project dir,
required). The tables survive the process: a restarted kernel reads its ledger,
dead letters, named-call dedup and every AI agent's memory straight back.

The SAME verbs, arguments, replies and refusals as `ram_store` — a caller cannot
tell the two apart except that this one is `durable: true`.

**Owner = caller.** Every row is keyed by who wrote it — `<kernel guid>|<agent id>`,
the qualified address of a caller on another kernel, or `kernel` for the operator. A
caller writes and reads its OWN rows; there is no `owner` field to forge.

| verb | what |
|---|---|
| `append {rows}` | add rows under your key, all or none; ids are the kernel's |
| `read {grant?, after?, limit?, kinds?, match?, from_last_kind?, order?}` | your rows, or a granted scope |
| `search {query, ids?, grant?}` | the LINES that mention something (FTS5 trigram index, substring semantics) |
| `share {with, scope, mode?, why?}` | let `with` read a scope of YOUR rows |
| `unshare {grant}` · `grants {side}` | revoke · list given/received |
| `prune {before_id \| ids, keep?}` | delete your rows; folds refs + grants on them go too |
| `inject {tables}` · `dump` | admins: replace / capture the whole store (a `ram_store` dump injects here and back) |

A **scope** is a descriptor, never a list of messages: `{head_until: <row>}`,
`{range: {from, to}}`, `{fold: <id>}` (the fold and its members), `{row: <id>}`.
`frozen` (default) stops at the last row when shared; `live` follows your head.

**Access, nothing implicit:** `operator: true` serves the operator (as `kernel`);
`admins: [ids]` (`kernel` = the operator) may name another `owner` and may
`inject`/`dump`. Everyone else: own rows + grants. The kernel's call gate still
decides who may reach this agent at all.

**The tables are real:** rows `(owner, id)`, ref_lists + fold_refs, grants — every
foreign key `ON DELETE CASCADE` with `PRAGMA foreign_keys=ON` — and an FTS5 text
index. `delete_agent` deletes the file (as ram_store's tables die with the agent); a kernel stop keeps it.
