# E125 `export_import` — export a subtree through a real kernel, import it back (#268)

A real host (`fantastic canvas`), a seeded three-level team of key-free agents
(`lead` > `mid` > `leaf`, exec bridges) that holds a key value, runtime fields and
row text, and that reaches the project's `files` agent from outside the subtree.

a) `user export_subtree {agent_id: lead, path: out/team}` writes a blueprint folder:
   three agents nested (no `parent_id`), the inside grant by id, the outside grant a
   declared port `@files`, and no key, runtime field or row text in any file;
   the reply says `exported: 3` and lists the port; exporting into the same
   (now non-empty) folder again is refused (`exists`);
b) `import_subtree` under `newparent` without `approve` asks for approval and creates
   nothing; with `approve` and the port bound to `files` it makes three agents
   under `newparent` with fresh ids, records equal to the originals (what a
   blueprint carries), the inside grant on the new ids and the port grant on `files`.

Every call is `host_call`, the way a canvas tab reaches the host.

Run: `cd src/lib/python && uv run python ../../../e2e/run.py --id export_import` (scripted, free).
