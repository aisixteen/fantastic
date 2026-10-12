# ram_store — memory in RAM

A MEMORY STORE agent: conversation rows for many owners, folds, search.
Everything it holds is lost when the process exits.

**Owner = caller.** Every row is keyed by who wrote it — `<kernel guid>|<agent id>`,
`<kernel guid>|kernel` for the operator and the kernel's own bookkeeping. A caller
writes and reads its OWN rows; there is no `owner` field to forge.

| verb | what |
|---|---|
| `append {rows}` | add rows under your key, all or none; ids are the kernel's |
| `read {after?, limit?, kinds?, match?, from_last_kind?, order?}` | your rows |
| `search {query, ids?}` | the LINES that mention something |
| `prune {before_id \| ids, keep?}` | delete your rows; fold refs on them go too |
| `inject {tables}` · `dump` | replace / capture the whole store |

**Access:** a caller reads and writes its own rows. Who may call any verb —
`inject`/`dump` included — is the kernel's call gate alone.

**Same tables as sqlite:** rows `(owner, id)`, fold_refs (FK cascade on their
rows), a text index. A durable store answers the same verbs.
