# user

The one human, as an agent. It exports a subtree of the running tree as a
blueprint folder, and imports a blueprint folder as a live subtree.

## Verbs

- `reflect` - this agent: id, sentence, handler_module, verbs.
- `export_subtree {agent_id, path}` - writes the agent and everything under it as
  a blueprint folder at `path` (relative to the running dir; no `..`), which must
  be new or an empty folder. Reply `{exported, path, ports, files}`: how many
  agents were written, the path as given, the port names (sorted) and the files
  (`blueprint.yaml`, then each owned file).
- `import_subtree {path, parent_id, bind?, allow_code?, approve?}` - the other
  half: the blueprint folder at `path` (relative to the running dir; no `..`)
  becomes agents under `parent_id`, every id fresh (one `_<k>` suffix for all, the
  smallest `k` that is free), every port bound (`bind: {"@port": "<agent id>"}`, to
  an agent of the port's kind) on both ends. Without `approve: true` the reply is
  `{approval_required, agents, new_ids, reaches, ports, code, hint}` and nothing is
  created. With it: `{imported, ids, parent_id, ports}`. Only the literal `true`
  counts for `approve` and `allow_code`.

## Refusals

`export_subtree`, checked in this order:

- `bad_arg` - `agent_id` or `path` missing or not a non-empty string; a `..` in
  `path`; the root, an unknown id or a parentless record as `agent_id`.
- `exists` - `path` exists and is not an empty folder.
- `not_found` - the folder that holds `path` does not exist.
- `failed` - the folder could not be written or moved into place.
- `secret_found` - the scan of the written folder found a key, a row or an
  absolute path. Nothing is written; the error names only the file and the kind,
  never a value.

## What it writes

The folder holds `blueprint.yaml` (nesting under `agents:` is the parent, no
`parent_id`; grants are written by id on both ends; `@root` stands for the root)
and the owned files. The folder is made 0700 and each file 0600, in a temp
folder beside the target and renamed into place after the scan.

- every record minus runtime state (`parent_id`, `guid`, `pid`, `version`, `rows`,
  `history`, `cache`, `usage`, `created_at`, `updated_at`, `ts`, `last_error`,
  `error`, `agents`, and any `last_` prefix or `_error`, `_at`, `_ts` suffix) and
  minus any key value (`api_key`, `token`, `secret`, `password`, at any depth);
  `credentials.ref` stays;
- no rows: only records are read, so conversation history never leaves;
- no absolute paths: one under the subtree's own folder becomes relative to it,
  any other becomes a port `@path:<field>`;
- a reference that points OUTSIDE the subtree becomes a PORT: `@memory`,
  `@credentials`, `@exec`, `@files`, `@model` or `@connector:<id>`, listed under
  `ports:` with its kind, why and `used_by`. Two outside agents of one kind are
  numbered `_2`, `_3` and so on, in sorted outside-id order; the root is `@root`;
- an agent's `system_prompt_file` (relative, readable) goes beside the blueprint
  as `<agent id>/directive.md`, and the record points there. One that cannot be
  read is kept as it is and noted in the blueprint's header.
- an html view's body (`view.bytes`) goes beside it as `<agent id>/view.html`
  (the record keeps `view.bytes_file`), and the plain top-level files of a folder
  bundle an agent runs on go once per bundle to `bundles/<handler_module>/<name>`.
  Both are CODE, listed in the header as `# code: <path>`; a symlink, a hidden
  file, a non-UTF-8 file or one over 1 MiB is left out and noted.
- the kernel root answers the same verb (`kernel_state.export_subtree`).

## Import

`import_subtree` checks, in this order:

- `bad_arg` - `path` or `parent_id` missing or not a non-empty string; `bind` not
  a map; a `..` in `path`; a folder with a symlink, a file over 1 MiB or more than
  512 files; a blueprint that is not yaml, has not exactly one top entry under
  `agents:`, has an entry without an id or a handler_module, with a `parent_id`,
  an unsafe or repeated id, an undeclared `@port`, or a name that is neither in
  the blueprint nor a port.
- `failed` - the kernel has no root.
- `not_found` - `parent_id` is not an agent, or `path` holds no `blueprint.yaml`.
- `secret_found` - a key value (`api_key`, `token`, `secret`, `password`, at any
  depth), row text (`rows`, `history`), an absolute path, or an `sk-` token in any
  file. The error names the agent and the kind, never the value.
- `unbound_ports` - a declared port missing from `bind` (the error lists every
  one with its kind), or a `@path:` port (not bindable yet).
- `bad_binding` - a `bind` key that is not a port, a value that is not an agent id,
  an agent that does not exist, or one of the wrong kind (a `connector` port takes
  any agent).
- `code_not_allowed` - a handler that is not a registered bundle, or a file other
  than `blueprint.yaml` and `<agent id>/directive.md`, without `allow_code: true`.
  The error names each one.
- `failed` - an agent could not be created, or a grant on a bound agent was
  refused. The new top agent is deleted with everything under it and the grants
  already written are taken back: nothing is kept.

## What it creates

Agents are created depth first, a parent before its children, through the root's
`create_agent`. Inside ids, `@root` and bound ports are rewritten in `can_call`,
`callable_by`, `memory_store_id`, `file_bridge_id`, `backend_id` and
`credentials.store`; `*`, `$...` and page grants stay. The bound agent (or the
root) grants back: a `can_call` key gives a `callable_by` on the other end and the
reverse, with the same grant.
