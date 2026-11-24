# `artifact_smoke` (E121) — the packaged kernel boots and describes itself

Free: no model. The PACKAGED rust kernel (staged by `artifacts.stage`): a one-shot
`reflect` in an empty directory, then a booted kernel driven over `web_rest` —
`rest`'s root reflect carries no operator frame (`runtime`, `env`, `version`,
`peers`, `guid`), the operator's one-shots after the stop carry the documented
root keys and the twelve kernel verbs (`kernel reflect part=docs`), `rest` sees no
kernel verb at its `call: [reflect]` level, and `reflect rest bundles: "all"`
with one instance per listed kind under `rest` whose `reflect part=docs` gives a
oneliner for every verb (kinds that cannot be instantiated: `NO_INSTANCE`).
On rust a packaged python kernel is booted beside it as the reference:
the same bundle kinds (kinds documented as runtime-specific excepted:
`foundation_models_backend` on rust), and per shared
kind the same `docs` verbs and `schema` field names. The checks are listed in
`probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id artifact_smoke --lanes 3
```
