# user_home_store — the user's saved set-ups

Named agent-record BLUEPRINTS kept in the user's home config dir
(`FANTASTIC_HOME`, else `~/.config/fantastic`), one plain-text file per name
under `agents/ai_agent/saved/`. The CLI owns it: the binary fixes its directory and the
only callers it serves when it registers it — a record can do neither.

A saved set-up is `handler_module` plus that bundle's config (endpoint, model,
sampling, instructions, …). It never carries identity (`id`, `parent_id`,
`guid`), per-project wiring (`memory_store_id`), or authority (`can_call`,
`callable_by`, `ingress_rule`, `egress_rule`, `auth`): the creator adds those
when it makes an agent from it. Keys are never here — the keychain holds them.

Verbs:
- `persist_record {name, record, if_match?}` — save (the target bundle's own
  `validate_meta` must pass); `if_match` = the version you read, refused when
  the file changed since.
- `forget_record {name, if_match?}` — delete.
- `load_tree` — every saved set-up: `{saved set-ups: [{name, version, record}],
  preferred, dir}`.
- `prefer {name}` / `prefer {name: null}` — the owner's explicit default for
  new AI agents (none unless set).

Files are `0600`, directories `0700`, created with that mode; a saved set-up or
directory readable by others is refused, naming the `chmod`. Writes are atomic
(temp file + rename).
