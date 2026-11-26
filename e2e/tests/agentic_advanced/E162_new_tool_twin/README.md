# `new_tool_twin` (E162) — an AI writes a new agent and uses it as a tool, scripted

Free: no model. The twin of `new_tool` (E160, #727): the same mechanics, driven by a
scripted model, so they are covered on every free run. A local OpenAI-compatible
server (`e2e/mocks/vllm.py`) answers from a fixed script; the kernel's own
`ai_chat.tools` chat, completing through a `vllm_backend` provider, runs the chain
on the packaged rust host, driven over `web_rest`.

The script is the call sequence E160 asks a model to find:

1. `write` the folder bundle through the `bundles` file agent (rooted at
   `.fantastic/bundles/`): `bundle.yaml` (`base: py_agent`, one verb `sha256` as
   `{doc, schema}`), the program `agent.py`, the helper `fantastic_agent.py`;
2. `kernel` `reload_bundle` — the folder written after boot loads (the AI's line on
   the root names the loader's verb);
3. `kernel` `create_agent` of it under the AI, with no line field: the AI's `create`
   line on itself names the bundle, and the new agent is in its tree (position);
4. `kernel` `update_agent` of the AI's own `can`, widened with a line on `sha`:
   refused, `not_allowed` naming `can` (an agent never writes its own `can`);
5. `sha` `reflect part=schema` (#719);
6. `sha` `sha256` on a token minted at test time;
7. the answer with the digest, then `done`.

Checks: the files land verbatim and the folder is listed (`reflect rest bundles=all`)
as a py_agent bundle; the record exists (handler_module, parent); the self-widening is
`not_allowed` naming `can` and the AI's `can` is unchanged; `reflect part=schema` declares the
verb and the AI's reflect reply carried it; the verb's reply carries the SHA-256 the
harness computes; the AI's stored answer carries it; no request follows `done`; the
lines are exact (E160's `grant_faults`: the AI's `can` is unchanged, the new agent's
`can` holds no `*`, it is under the AI), and the same scorer flags a `*` planted on
`sha` and a line the AI widened its own `can` with; E160's own `score` passes this
run, so the model test's scoring is proven passable by a perfect actor.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id new_tool_twin
RUNTIME=rust uv run python ../../../e2e/tests/agentic_advanced/E162_new_tool_twin/probe.py
```
