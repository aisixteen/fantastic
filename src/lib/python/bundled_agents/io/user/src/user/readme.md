# user

The one human, as an agent. It exports a subtree of the running tree as a
blueprint folder.

## Verbs

- `reflect` - this agent: id, sentence, handler_module, verbs.
- `export_subtree {agent_id, path}` - writes the agent and everything under it as
  a blueprint folder at `path` (relative to the running dir; no `..`), which must
  be new or an empty folder. Reply `{exported, path, ports, files}`: how many
  agents were written, the path as given, the port names (sorted) and the files
  (`blueprint.yaml`, then each owned file).

## Refusals

Checked in this order:

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
