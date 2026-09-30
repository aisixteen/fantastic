# E20 · the set-up page with the OpenAI Responses backend

**Costs:** free. No model runs: the agent's endpoint is a scripted Responses
stub (`e2e/runners/_responses_stub.py`) on 127.0.0.1.

E17's twin on a build with `--features responses`: the set-up page lists
`responses_backend.tools` (the only linked backend), the human writes an agent
with the stub's full `…/v1/responses` URL as its endpoint, and it answers.

| run | proves |
|---|---|
| `init` | a fresh directory inits |
| `write` | the page picks `responses_backend.tools`, the record is written with its endpoint and model |
| `answer` | the next start rehydrates the agent; `hello` gets the stub's scripted reply |
| `recall` | a restart brings the thread back from the memory store, no new model turn |

The stub records every request: each carried `OPENAI_API_KEY` as `Bearer` (the
key is read from the environment at call time, never stored on the record).

    cd src/lib/python && uv run python -u ../../../e2e/runners/tui.py setup_responses
