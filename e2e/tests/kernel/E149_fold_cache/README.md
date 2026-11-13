# `fold_cache` (E149) — the fold cycle under `caching: provider_cache`: anchors, recall, grep, cache marks (#556)

Free: no model. A scripted server plays the model for two AI agents on the PACKAGED host
of `RUNTIME`, next to a `file_bridge` agent (`files`, an 80 kB file whose line 50 holds a
word found nowhere else) and a test-only folder bundle `bigdoc` (written by the probe at
runtime, as E117 does) whose verb doc is about 70,000 chars, over AUTOFOLD_AT (16000
tokens = 64,000 chars). `context_window` is 262144, so both bodies are delivered inline.

Plugs: the cycle (`caching: provider_cache`) is rust: `ai_chat` on a scripted vllm plug; python: an
`anthropic_backend` agent on the Anthropic mock (`e2e/mocks/anthropic.py`), because
Python's `vllm_backend` refuses `provider_cache`. The wire readers differ per plug; the
checks do not.

- B1a: a `read` of the big file is whole in its turn (and stored whole).
- B1b: the next request holds `[folded · read reply from files · ~N tokens · head: "…" ·
  get: B]`, not the body; a `docs` of `bigdoc` (detailed) is whole in its turn.
- B2: the next request holds `[folded · docs reply from kernel · … · get: D]`; `recall`
  by row id D gives the body back whole.
- B3: the recall reply is `[folded · recall reply from ai_b · id D · ~N tokens · get: …]`
  after the answer; the store row D is still whole; `grep` for the word found only inside
  the folded read body returns the hit (`L050 …`), and that body is still folded in view.
- B4: every anchor is byte-identical in every later request (a folded row is never
  rewritten); every request carries the cache mark (rust vllm wire: `"cache_mark": true`
  on a message; python anthropic wire: `cache_control` ephemeral on the last block of a
  row, and on the system block); where a body that will fold arrives whole the mark sits on
  the message before it, and where nothing will fold the last message is marked.

Shape sources. Rust: `fantastic-ai-core/src/history_view.rs` (`docs_dropped` :126-133,
`drop_read_docs` :202-245, autofold :522-603), `caching.rs` (`mark` :182-219),
`retrieval.rs:83-238` (grep), `verbs.rs:60,741`; vllm plug `fantastic-vllm-backend/src/
lib.rs:309-326`. Python: `ai_core/core.py` (`_DOCS_DROPPED` :1502-1505, `drop_read_docs`
:1556, `_grep` :4784, recall :4651), `autofold.py:146-192`, `caching.py:117-149`;
`anthropic_backend/provider.py:386-428,532-548`.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id fold_cache --lanes 2
```
