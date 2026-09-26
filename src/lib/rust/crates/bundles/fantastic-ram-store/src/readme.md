# ram_store — memory in RAM

A MEMORY STORE agent: conversation rows for many owners, folds, grants, search.
Everything it holds is lost when the process exits.

**Owner = caller.** Every row is keyed by who wrote it — `<kernel guid>|<agent id>`,
the qualified address of a caller on another kernel, or `kernel` for the operator. A
caller writes and reads its OWN rows; there is no `owner` field to forge.

| verb | what |
|---|---|
| `append {rows}` | add rows under your key, all or none; ids are the kernel's |
| `read {grant?, after?, limit?, kinds?, match?, from_last_kind?, order?}` | your rows, or a granted scope |
| `search {query, ids?, grant?}` | the LINES that mention something |
| `share {with, scope, mode?, why?}` | let `with` read a scope of YOUR rows |
| `unshare {grant}` · `grants {side}` | revoke · list given/received |
| `prune {before_id \| ids, keep?}` | delete your rows; folds refs + grants on them go too |
| `inject {tables}` · `dump` | admins: replace / capture the whole store |

A **scope** is a descriptor, never a list of messages: `{head_until: <row>}`,
`{range: {from, to}}`, `{fold: <id>}` (the fold and its members), `{row: <id>}`.
`frozen` (default) stops at the last row when shared; `live` follows your head.

**Access, nothing implicit:** `operator: true` serves the operator (as `kernel`);
`admins: [ids]` (`kernel` = the operator) may name another `owner` and may
`inject`/`dump`. Everyone else: own rows + grants. The kernel's call gate still
decides who may reach this agent at all.

**Same tables as sqlite:** rows `(owner, id)`, fold_refs, grants (FK cascade on
their anchor row), a text index. A durable store answers the same verbs.
