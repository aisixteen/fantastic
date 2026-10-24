# `artifact_smoke` (E121) — the packaged kernel boots and describes itself

Free: no model. The PACKAGED kernel of `RUNTIME` (staged by `artifacts.stage`):
a one-shot `reflect` in an empty directory, then a booted kernel driven over
`web_rest` — the root reflect's documented keys, the `detail: "full"` doc frame,
the keyed doc tree (`readme: index` / `<key>` / `true` / unknown), `docs of=kernel`,
and `reflect bundles: "all"` with `docs of=<handler_module>` for every listed kind.
On rust a packaged python kernel is booted beside it as the reference:
the same bundle kinds (kinds documented as runtime-specific excepted:
`foundation_models_backend` on rust), and per shared
kind the same `docs` verbs and `schema` field names. The checks are listed in
`probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id artifact_smoke --lanes 3   # all three
```
