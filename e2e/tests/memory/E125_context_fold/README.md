# E125 context_fold

NO MODEL, Python runtime. A live `ollama_backend` agent on a stub provider takes six
turns; the operator then folds two rows in the middle with `context_fold` and a given
summary. Scored on the rows before the range (stored and in the next prompt) staying
byte for byte what they were, the fold row replacing exactly its members, a refused
range naming its arg, and a kernel restart not rewriting the prefix.

```sh
cd src/lib/python && uv run python ../../../e2e/tests/memory/E125_context_fold/probe.py
```
