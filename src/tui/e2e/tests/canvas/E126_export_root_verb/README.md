# E126 `export_root_verb` — the kernel ROOT answers `export_subtree` too (#268)

The same seeded team as E125 (plus an absolute path on `lead`), a real host.

a) `export_subtree` called on the root (`kernel`, through `forward`) is no error,
   its reply says `exported: 3`, and the folder passes the same checks as the
   `user` one: nested, no `parent_id`, inside grant by id, outside grant a port,
   no key, runtime field, row text or absolute path;
b) the same subtree exported through `user` gives a `blueprint.yaml` byte for byte
   equal (one engine, two doors), and the reply is the same;
c) the root lists the verb (`docs of kernel_state`) and says it is the root's
   (`kernel_state.export_subtree`) when it refuses: a `..` path and an unknown
   agent id are refused on both doors, the unknown one writes no folder;
d) a non-root agent does not answer it as the root (the refusal names the root).

Every call is `host_call`, the way a canvas tab reaches the host.

Run: `cd src/lib/python && uv run python ../../../e2e/run.py --id export_root_verb` (scripted, free).
