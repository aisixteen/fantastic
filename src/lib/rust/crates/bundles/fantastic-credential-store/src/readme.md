# credential_store — secrets an agent uses but never keeps

A credential is a REF (`openai/main`) holding named fields (`api_key`) and bound to
ONE HOST. A consumer names it on its own record — `credentials: {store, ref}` —
reads it at USE time and sends it only to that host. Rotating a key is one `set`;
the next read picks it up, with no restart.

| verb | what |
|---|---|
| `set {ref, fields}` | store or rotate the value of a DECLARED ref (writers); call with `secret: ['payload.fields']`; takes no `host` — the declaration's is the binding |
| `get {ref, host?}` | `{ref, host, fields}` (readers); call with `secret: ['result']`; `host` refuses a ref bound elsewhere |
| `list` | the refs YOU may read or write: metadata, never a value |
| `forget {ref}` | drop the VALUE (writers only); the declaration stays |

**Backends** (`backend` on the record): `file` (default) — one YAML file at an
ABSOLUTE path, `~/.fantastic/credentials/<id>.yaml` unless `file` names another
(`/…` or `~…`; a relative path is refused at the write: "file='creds.yaml' must be
an absolute path (starting with / or ~) — a relative path is refused"), written
0600 and read only if it is a regular file owned by this user with no group/other
bits — judged on the one open it is read from (`O_NOFOLLOW`, then `fstat` of that
descriptor), so nothing can swap it between the check and the read; `keychain` — the OS keychain, one item per ref under the namespace
`keychain_namespace` (default `fantastic` — macOS calls it the item's "service";
account `<id>/<ref>`); `env` — read-only, each ref maps fields to variables:
`refs: {<ref>: {host, env: {api_key: VAR}}}`.

**The host is stored with the value**: the file entry and the keychain item are
`{host, fields}`, the host written by `set` from the ref's declaration. A `get`
whose declaration names another host than the stored one is refused with the
same answer as a `get {ref, host}` naming another host
("credential_store: ref '<ref>' is bound to host '<declared>', not '<stored>' —
refused, nothing read", `reason: bad_arg`, `arg: host`), and so is a stored value
carrying no host (`… not None — …`) — a writer sets the value again. A store elsewhere declaring the same id and ref with
another host reads nothing. `env` keeps no value at rest.

**A ref is declared ONLY by `update_agent`** on the store's own record — the kernel's
privileged verb, granted explicitly, is the one path:
`refs: {<ref>: {host, readers: [...], writers: [...], fields?/env?}}` (it replaces
the whole `refs` map — write every ref you keep; `host` is required: "refs['<ref>'].host
is required — the one host this ref's value may be sent to"). The declaration is the
operator's alone: `set` never creates a ref (an undeclared one answers "no such ref",
the operator included) and takes no host ("credential_store set: takes no host — a
ref's host is its declaration, written by update_agent on this store's refs");
`forget` drops the value and keeps the declaration (host, readers, writers) — only
`update_agent` undeclares. On the `env` backend `forget` is refused like `set`
("credential_store forget: the env backend is read-only — unset the environment
variable its ref names"). A ref's `fields` are its stored value's names: `set`
writes them, `forget` drops them.
`set`/`forget` need the caller in `writers`, `get` in `readers`; an entry is an agent id,
`$subtree:<id>`, or a `<guid>|<agent>` caller on another kernel. A caller with no
grant is answered "no such ref". The operator passes every reader and writer check.

**No value is ever recorded.** An unmarked `set`/`get` is refused: the value would
land in the ledger, the memory store and the state stream. Either spelling of a mark
counts (`['payload', 'fields']` is `payload.fields`), as does marking every field;
a field name holding a dot is marked only as an array (`['result', 'fields', 'a.b']`
— `result.fields.a.b` is `a` → `b`). The record holds the
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
`set`/`get` goes over a direct `ws_bridge` leg to a loopback host (an ssh tunnel's
local end) — a `ws://` leg to any other host refuses secret marks too.
