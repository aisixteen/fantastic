# `subai_key_twin` (E167) — the key line a builder hands its new chat, scripted

Free: no model. The twin of the key wiring in `subai_graph` (E31). Two local
OpenAI-compatible servers (`e2e/mocks/vllm.py`) answer from a fixed script: `B` plays
the builder `ai`, `H` plays the new chat `hour_ai` and refuses a request without the
key. The packaged rust host runs the chain, driven over `web_rest`.

The builder holds E31's lines: `creds: get` (the operator's, beside its `create`
line) and E30's `create` line on itself, which forces the provider's `can` to
`creds: get`. Every ancestor of `hour_ai_prov` whose tree does not hold the key store
caps its `creds` call, so `hour_ai` needs the line too. The operator writes no line on
a model-made agent: a `can` the builder writes replaces the whole field.

1. the builder creates `hour_ai` with `can` `{memory, creds: get}`, then
   `hour_ai_prov` under it (keyed), then `done`; a tick to `hour_ai` completes on `H`
   with the key, no request refused;
2. the builder rewrites `hour_ai`'s `can` without the key line; a tick fails
   `no agent 'creds'` and never reaches `H`;
3. the builder rewrites it with the line kept; a tick completes on `H` with the key.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id subai_key_twin
RUNTIME=rust uv run python ../../../e2e/tests/agentic_advanced/E167_subai_key_twin/probe.py
```
