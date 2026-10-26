# E129 `export_owned_files` — a subtree with owned files, exported and imported (#268)

The E125 team, with files an agent owns: `mid` has a `system_prompt_file`
(`prompts/mid.md`) and an html view (`view.bytes`); `leaf` runs on a FOLDER BUNDLE
(`.fantastic/bundles/e129_card/` with `bundle.yaml`, `agent.py`, `panel.html`).

a) `user export_subtree` writes, beside `blueprint.yaml`, `mid/directive.md`,
   `mid/view.html` and `bundles/e129_card/{bundle.yaml,agent.py,panel.html}`, each
   byte for byte its source; the blueprint's header lists every code file as
   `# code: <path>` (the view and the bundle files, not the directive); the
   record holds `view.bytes_file` and no `bytes`, and `system_prompt_file` points
   at the copy; the folder still passes the secrets / runtime / row checks;
b) the kernel root's export gives the same files and blueprint;
c) importing without `allow_code` is refused (`code_not_allowed`, the code named)
   and creates nothing; with `allow_code` and `approve` the team is made under
   `newparent`, the export folder's files are untouched, and the new records keep
   the pointers (`view.bytes_file`, `system_prompt_file`).

(The importer does not write the owned files into the new agents' folders: that
is documented as "not yet" in docs/system.md, so the files compared are the
export folder's.)

Run: `cd src/lib/python && uv run python ../../../e2e/run.py --id export_owned_files` (scripted, free).
