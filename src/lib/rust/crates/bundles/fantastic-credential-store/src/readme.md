# credential_store — secrets an agent uses but never keeps

A credential is a REF (`openai/main`) holding named fields (`api_key`) and bound to
ONE HOST. A consumer names it on its own record — `credentials: {store, ref}` —
reads it at USE time and sends it only to that host. Rotating a key is one `set`;
the next read picks it up, with no restart.

| verb | what |
|---|---|
| `set {ref, host, fields}` | store or rotate; call with `secret: ['payload.fields']` |
| `get {ref, host?}` | `{ref, host, fields}`; call with `secret: ['result']`; `host` refuses a ref bound elsewhere |
| `list` | the refs YOU may read or write: metadata, never a value |
| `forget {ref}` | drop the value and its grants (writers only) |

**Backends** (`backend` on the record): `file` (default) — one YAML file,
`~/.fantastic/credentials/<id>.yaml` unless `file` names another path, written
0600 and read only if it is a regular file owned by this user with no group/other
bits; `keychain` — the OS keychain, one item per ref (service `keychain_service`,
default `fantastic`; account `<id>/<ref>`); `env` — read-only, each ref maps fields
to variables: `refs: {<ref>: {host, env: {api_key: VAR}}}`.

**Grants, per ref**, on the store's record (the operator edits them with
`update_agent`): `refs: {<ref>: {readers: [...], writers: [...], host, fields}}`. An
entry is an agent id, `$subtree:<id>`, or a `<guid>|<agent>` caller on another
kernel. An agent in `creators` may make a NEW ref, which it then reads and writes. A
caller with no grant is answered "no such ref". The operator may do everything.

**No value is ever recorded.** An unmarked `set`/`get` is refused: the value would
land in the ledger, the memory store and the state stream. The record holds the
host and the field NAMES only. The kernel's call gate still decides who may reach
this agent at all. Never give a credential to an AI agent's prompt — give it to a
bundle that sends it (an AI backend reads its own key this way).

**A secret holder, never a model's tool.** The store stamps `holds_secrets: true` on
its own record at boot (shown on its reflect) and refuses any other value at the
write ("holds_secrets must be true — a credential_store always holds secrets"). An AI
agent's backend reads its key here as the agent; its MODEL cannot — ai_core refuses
every model-emitted call that names an agent so marked. An operator may set
`holds_secrets: true` on any other agent to protect it the same way.

The kernel judges the field before the store does: on every agent only `true` or
`null` may be written (anything else is `invalid_meta`, "holds_secrets must be
exactly true (this agent holds secrets) or null (the operator clears the mark) —
got <type>"), so the store's own text answers `null` — the operator's clear, which
a store refuses on itself. And only the operator may lower a holder's mark: an
agent's `update_agent` writing anything but `true` there is refused
(`{reason:"unauthorized", side:"secret-holder"}`) — a model granted `update_agent`
cannot unmark this store and then call it.

**Not over a relay, yet.** A `relay_connector` leg refuses every call carrying
secret marks (the relay records the frames it routes unmasked), so a remote
`set`/`get` goes over a direct `ws_bridge` leg.
