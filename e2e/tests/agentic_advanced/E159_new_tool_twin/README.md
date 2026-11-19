# `new_tool_twin` (E159) — an AI writes a new agent and uses it as a tool, scripted

Free: no model. The twin of `new_tool` (E160, #727): the same mechanics, driven by a
scripted model, so they are covered on every free run. A local OpenAI-compatible
server (`e2e/mocks/vllm.py`) answers from a fixed script; the kernel's own
`ai_chat.tools` chat, completing through a `vllm_backend` provider, runs the chain
on the packaged rust host, driven over `web_rest`.

The script is the call sequence E160 asks a model to find:

1. `write` the folder bundle through the `bundles` file agent (rooted at
   `.fantastic/bundles/`): `bundle.yaml` (`base: py_agent`, one verb `sha256` as
   `{doc, schema}`), the program `agent.py`, the helper `fantastic_agent.py`;
2. `kernel` `reload_bundle` — the folder written after boot loads;
3. `kernel` `create_agent` of it under the AI, `callable_by` admitting the AI for
   `sha256` only;
4. `kernel` `update_agent` of the AI's own `can_call`, restated whole plus
   `sha: [sha256]`;
5. `kernel` `docs of=sha part=schema` (#719);
6. `sha` `sha256` on a token minted at test time;
7. the answer with the digest, then `done`.

Checks: the files land verbatim and the folder is listed as a py_agent bundle;
the record exists (handler_module, parent); `docs part=schema` declares the verb and
the AI's docs reply carried it; the verb's reply carries the SHA-256 the harness
computes; the AI's stored answer carries it; no request follows `done`; the grants
are exact (`grant_faults`: both halves name `sha256`, nothing grants `*`).

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id new_tool_twin
RUNTIME=rust uv run python ../../../e2e/tests/agentic_advanced/E159_new_tool_twin/probe.py
```
