# credential_store — secrets an agent uses but never keeps

A credential is a REF (`openai/main`) holding named fields (`api_key`) and bound to
ONE HOST. A consumer names it on its own record — `credentials: {store, ref}` —
reads it at USE time and sends it only to that host. Rotating a key is one `set`;
the next read picks it up, with no restart.

| verb | what |
|---|---|
| `set {ref, fields}` | store or rotate the value of a DECLARED ref; takes no `host` — the declaration's is the binding |
| `get {ref, host?}` | `{ref, host, fields}`; `host` refuses a ref bound elsewhere |
| `list` | the declared refs: metadata, never a value |
| `forget {ref}` | drop the VALUE; the declaration stays |

**Backends** (`backend` on the record): `file` (default) — one YAML file at an
ABSOLUTE path, `<home>/user/credentials/<id>.yaml` (`<home>` is `FANTASTIC_HOME`,
else `~/.fantastic`) unless `file` names another
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
carrying no host (`… not None — …`) — `set` the value again. A store elsewhere declaring the same id and ref with
another host reads nothing. `env` keeps no value at rest.

**A ref is declared ONLY by `update_agent`** on the store's own record (the
operator, or the store's parent):
`refs: {<ref>: {host, fields?/env?}}` (it replaces
the whole `refs` map — write every ref you keep; `host` is required: "refs['<ref>'].host
is required — the one host this ref's value may be sent to"). `set` never creates a ref (an undeclared one answers "no such ref",
the operator included) and takes no host ("credential_store set: takes no host — a
ref's host is its declaration, written by update_agent on this store's refs");
`forget` drops the value and keeps the declaration — only
`update_agent` undeclares. On the `env` backend `forget` is refused like `set`
("credential_store forget: the env backend is read-only — unset the environment
variable its ref names"). A ref's `fields` are its stored value's names: `set`
writes them, `forget` drops them.
**There are no per-ref access lists**: any caller the kernel's call gate admits may
`set`/`get`/`forget` a declared ref.

**The record holds the host and the field NAMES only.** The value rides the call:
a `set` payload and a `get` reply are recorded like any call's — the ledger, the
memory store, the state stream — as `[secret]`; the caller gets the value whole. Never give a credential to an AI agent's
prompt — give it to a bundle that sends it (an AI backend reads its own key this
way, as itself).

**Connected kernels (issue #285):** a `set` forwarded through a `ws_bridge` or
`relay_connector` is recorded by the CALLER with its `fields` marked (the bridge's
`payload_secret` hook), and the far store's `get` reply marks ride the reply frame
(`secret`) back, so the calling kernel records the reply as `[secret]` too. The
caller still gets the values whole. The page-peer route (`<guid>|agent` over a
web_ws hello) is not covered; it only reaches browser kernels, which hold no store.
