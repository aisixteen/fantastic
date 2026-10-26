# E127 fork marks

NO MODEL, Python runtime. A live `ollama_backend` agent on a stub provider takes six
turns; it then marks its history (`mark`), takes one more turn, lists its marks
(`fork_points`) and forks twice at the mark (`fork {at}`). Scored on both forks holding
exactly the parent's rows up to the mark (ids and content), the two forks being the same
as each other, the later turn not reaching either, the mark listing its `rows_after` and
model, and an `at` inside a turn being refused as `cuts_turn` with no agent created.

```sh
cd src/lib/python && uv run python ../../../e2e/tests/memory/E127_fork_marks/probe.py
```
