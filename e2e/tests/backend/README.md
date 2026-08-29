# `backend/` — plug round-trips

Checks that measure a **connector**, not the kernel. They run only when their
plug is the selected one (`only_backend:` in `test.yaml`).

| test | what it proves |
|---|---|
| `llamacpp` | llama-server round trip — streaming, thinking markers, tool envelope |

This group exists so the top level stays organised by *kind*. Not every test type
is an LLM test, and a future non-LLM group should not have to pretend to be one.
