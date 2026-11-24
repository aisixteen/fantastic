# `folder_bundles` (E117) — a folder under `.fantastic/bundles/` as an agent kind

Free: no model. Folder bundles (`bundle.yaml` with `base: py_agent` + a stdlib
program using `fantastic_agent.py`) on the packaged rust host, every call over
`web_rest`; `rest` holds `reload_bundle` on the root (the root loader's verb). It proves:

- **cold load**: folders written before boot are read by the root loader (through
  the fs clamp on rust); a seeded instance record of one resolves and its
  program answers right after boot;
- **catalog**: `reflect rest bundles=all` (the leg's own tree: the whole catalog)
  lists the folder (`base`, `sentence`, `handler_module`, `folder`; its one row
  with `bundle_names`); `reflect <instance> part=docs` reads `bundle.yaml`
  (oneliner signature, `tier: detailed` doc); `reflect of=…` is refused `bad_arg`;
  an agent holding only `call: [reflect]` on the root gets no `bundles` key;
- **create**: `create_agent handler_module=<folder>` works, the instance reflects
  `base: py_agent` and a running program, its verbs answer; `delete_agent` kills
  the program; `create_agent handler_module=py_agent.tools` is refused (a base);
- **hot load**: a folder added after boot loads on the root's `reload_bundle`;
  editing its program and reloading restarts the live instance (new pid, new
  answer); a reload of a now-malformed `bundle.yaml` is refused and the instance
  keeps running the previous load; `reload_bundle` of no folder is `not_found`;
- **not fatal**: an unparsable `bundle.yaml` and one with no `sentence` are listed
  with their `error`; an instance of a refused folder answers `unhandled`;
- **reserved verbs**: a bundle declaring `reflect` is refused at load; a bundle
  declaring the kernel verb `ps` loads, but `ps` on its instance is the kernel's
  `not_here`, never the program's answer;
- every program is gone once the kernel stops.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id folder_bundles --lanes 3
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E117_folder_bundles/probe.py
```
