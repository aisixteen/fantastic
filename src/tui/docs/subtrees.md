> Export and import a subtree: `export_subtree` writes an agent and everything under it as a blueprint folder (ports for what points outside, owned files beside it, no secrets), and `import_subtree` makes it again under a parent, with fresh ids, bound ports and an approval step.

# Export and import a subtree

## Export a subtree

The verb `export_subtree {agent_id, path}` (on `user` and on the kernel's
root, the same engine and the same reply) writes an agent and every agent
under it as a blueprint folder at `path` (relative to the running dir; a new
or empty folder; no `..`). In the TUI, `/export <path>` exports the selected
agent; on the canvas, a cell's right-click menu has "Export…":

- `blueprint.yaml` in the shape of `src/tui/blueprint.yaml`: nesting under
  `agents:` is the parent (no `parent_id`), children in creation order, grants
  by id on both ends, `@root` for the root. Ids stay the readable names; the
  importer makes fresh ones.
- the owned files beside it: an agent's `system_prompt_file` (relative and
  readable) as `<agent id>/directive.md`, the record pointing there; an html
  view's body (`view.bytes`) as `<agent id>/view.html`, the record keeping
  `view.bytes_file` in its place (`kind` and `calls` stay); and, for an agent
  that runs on a FOLDER BUNDLE, the plain top-level files of
  `.fantastic/bundles/<handler_module>/` (`bundle.yaml`, `agent.py`,
  `panel.html`…) as `bundles/<handler_module>/<name>`, once per bundle.
  `<agent id>/view.html` and `bundles/…` are CODE: the blueprint's header
  lists each as `# code: <path>`, and the importer takes them only with
  `allow_code`. A symlink, a hidden file, a file that is not UTF-8 text or is
  over 1 MiB is left out and noted in the header.

The reply is `{exported, path, ports, files}`. Files are written `0600`,
folders `0700`, into a temp folder that is renamed into place.

A reference that points OUTSIDE the subtree (a grant key in `can_call` /
`callable_by`, `memory_store_id`, `file_bridge_id`, `backend_id`,
`credentials.store`) becomes a PORT, listed under `ports:` with its `kind`,
`why` and `used_by`:

| port | kind | for |
|---|---|---|
| `@memory` | memory | a memory store (`ram_store.tools`, `sqlite_store.tools`, `user_home_store.tools`, `user_store.tools`) |
| `@credentials` | credentials | the key store (`credential_store.tools`) |
| `@exec` | exec | an exec agent (`exec_bridge.tools`) |
| `@files` | files | a file bridge (`file_bridge.tools`) |
| `@model` | model | an AI backend (`anthropic_backend.tools`, `ollama_backend.tools`, `responses_backend.tools`, `vllm_backend.tools`, `foundation_models_backend.tools`) |
| `@connector:<id>` | connector | anything else |
| `@path:<field>` | path | an absolute path outside the subtree's folder |

The kind is the target's handler name, nothing else: a target that is
`ai_chat.tools` is a `@connector`, and a target not in the tree is too.
Two outside agents of one kind are `@memory`, `@memory_2`…, numbered by
outside id. `$self`, `$parent`, `$subtree`, `*` and page grants
(`<guid>|<id>`) are written as they are.

Stripped: runtime state (`guid`, `pid`, `version`, `rows`, `history`, `cache`,
`usage`, `ts`, `created_at`, `updated_at`, `error`, `last_error`, any `last_*`,
`*_error`, `*_at`, `*_ts`), every `api_key`, `token`, `secret` and `password`
(at any depth; `credentials.ref` stays), and absolute paths (relative to the
subtree's folder when under it, else a port). Only records are read, never
rows. A secrets scan runs over the written folder; a hit deletes it and the
export is refused (`secret_found`).

Not yet: a frontend agent's top-level `html` field stays inline in the
blueprint. The import is below.

## Import a subtree

`user`'s verb `import_subtree {path, parent_id, bind?, allow_code?, approve?}`
reads the blueprint folder at `path` (relative to the running dir; no `..`;
plain files only, no symlinks) and makes its agents under `parent_id`.
Everything is checked before anything is created:

- the shape: one top entry under `agents:`, nesting is the parent, a
  `parent_id` in an entry is refused (`bad_arg`);
- NO SECRETS (`secret_found`): an `api_key`, `token`, `secret` or `password`
  key at any depth, `rows` or `history`, an `sk-` token in any file, or an
  absolute path. The message names the agent and the key, never the value;
  `credentials.ref` is a reference and passes;
- PORTS: every `@…` name used must be declared under `ports:`, and every
  declared port bound with `bind: {"@memory": "<agent id>", …}` to an
  existing agent of the port's kind (a `@connector:<id>` takes any agent).
  Every unbound port is named at once (`unbound_ports`); a bind of the wrong
  kind, of no agent, or of a name that is no port is `bad_binding`.
  `@path:*` ports are not bindable yet;
- CODE (`code_not_allowed`): a handler that is not a registered bundle (a
  starter, `py_<hex>`…) or a file other than `blueprint.yaml` and
  `<agent id>/directive.md`. `allow_code: true` lets it in.

FRESH IDS: every id gets the same suffix `_<k>`, the smallest `k` for which
none of the new ids is taken, so a second import under the same parent gives
`a_2`, `b_2`… beside `a_1`, `b_1`…. Grants inside the blueprint are re-wired
to the new ids, `@root` becomes the root's id, a port becomes the bound
agent; `$self`, `$parent`, `$subtree`, `*` and page grants stay. A name that
is neither in the blueprint nor a port is refused.

APPROVAL: without `approve: true` (only the literal counts, as for
`allow_code`) nothing is created and the reply is the preview:
`{approval_required, agents, new_ids, reaches: [{id, from, can_call,
callable_by, ports}], ports: {port: agent id}, code}`. With it the agents are
created depth first and the other end of every bound edge is written on the
bound agent (its `callable_by` for a `can_call` to the port, its `can_call`
for a `callable_by`), so the copy reaches exactly what it was granted and what
its ports were bound to. A failure deletes what was made (`failed`). The
reply is `{imported, ids, parent_id, ports}`.

From a shell: `fantastic import <path> --parent <agent id> [--bind
@port=agent id]… [--allow-code] [--approve]` calls the verb in the running
kernel (attach-only, like `send`). It prints the preview (new ids, what each
reaches, the ports, the code) and creates nothing; on a terminal it then asks
`approve? [y/N]`, and `--approve` sends the approved call at once. Exit 0 for a
preview or an import, 1 for a refusal (`refused (<reason>): <message>`), 2 for
a bad command line. On the canvas, dropping a folder that holds
`blueprint.yaml` onto a cell (the parent) or the canvas (the canvas's host
node) shows the same preview with Approve/Cancel (code needs its own "allow
code" tick) instead of opening the folder.

Not yet: the verb on the kernel's root, `@path:*` ports, and owned files
beyond the directive (a directive keeps its relative `system_prompt_file`; the
file is not written into the new agent's folder).
