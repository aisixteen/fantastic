> Durable memory is FILES — any format you choose, on a gated file surface.

# Memory — durable state as files, everywhere

Memory here is not a special store: it is documents you write. The substrate
imposes NO format — keep your memory as markdown, JSON, plain notes, source
code, whatever fits the way you think. What makes it memory is only that you
wrote it down and can find it again.

Two agents carry it:

- A `file_bridge` agent is the SURFACE — a directory served as an agent, gated
  and clamped. Raw bytes ride its `read_stream`/`write_stream`.
- A `text_file` agent is the EDITOR — line-numbered `read`, atomic `write`,
  surgical `patch` (SEARCH/REPLACE blocks), `append`, and ON DEMAND a
  `version` hash with an `if_match` guard, plus `checkpoint`/`restore` when a
  change is worth a pre-image. Point it at any surface with `on:` (or its
  `target_id` default).

The practice: keep one file per concern, named for what it holds
(`notes.md`, `decisions.md`, `state.json`). `append` for logs, `patch` for
precise updates, `read` to recall. A fact worth keeping is a line you write;
pruning is a patch that removes it. Structure is YOURS — a key-value shape, a
diary, a table — the medium does not care.

Wiring is the operator's, both ends as always: your `can_call` names the
text_file agent and the surface; their `callable_by` accepts you. An unwired
editor fails loud naming `target_id` — nothing persists silently.
