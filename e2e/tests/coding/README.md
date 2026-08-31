# `coding/` — the coder organs, end to end

A coding harness is a set of verbs plus a rights table (see `THESIS.md`). This
group proves those verbs at the product surface — over the real kernel, in every
runtime — without a model in the loop, so each is a free, deterministic
capability test. The model *using* the organs to do real work is `agentic/`.

| test | organ | proves |
|---|---|---|
| `file_edit` | `file_bridge.patch_text_file` (C2) | a SEARCH/REPLACE patch applies over the stream, and a non-matching hunk fails loud leaving the file untouched (all-or-nothing) |
