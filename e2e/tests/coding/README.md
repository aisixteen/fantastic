# `coding/` — the coder organs, LLM-DRIVEN, end to end

A coding harness is a set of verbs plus a rights table (see `THESIS.md`). Each
test here hands a MODEL one organ and one task, and scores the RESULT — the
file, the store, the repo, the suite — never prose. Each also requires, from
the transcript, that the model used THAT organ (a bypass fails the check).

An organ that IS a process needs ZERO kernel code: an `exec_bridge` leg with an
operator allowlist, driven by `start`/`status` ("a capability is a command").
The byte surface is the `file_bridge` streams; the EDIT organ is the `text_file` agent's `patch` (gated sends, no direct disk).

| test | organ | the model must |
|---|---|---|
| `file_edit` | `text_file.patch` (C2) | change ONE value in a config with a SEARCH/REPLACE patch — the rest of the file byte-identical |
| `search` | `exec_bridge` + `grep` (C3) | find a token hidden in a tree it was never shown, store the VALUE in memory |
| `git_ops` | `exec_bridge` + `git` (C16) | commit the working-tree change; log carries its message, the tree ends clean |
| `program_tests` | `exec_bridge` + `pytest`, composed with the edit organ (C20+C2) | run the failing suite, fix the bug in the PROGRAM, rerun to green |
| `undo` | `text_file` checkpoints (C12) | patch WITH `checkpoint:true`, then restore the pre-image by its `ckpt_id` — file ends byte-identical |
| `multiline_edit` | `text_file.patch` (C2) | ONE patch, two hunks — one replacing a 3-line block — everything else byte-identical, no whole-file rewrite |
| `cancel` | kernel `ps`+`cancel` over an `exec_bridge` job (C1) | find the in-flight job in the task table, kill it — the OS process must actually die |
| `grant` | the two-sided call gate + the ask protocol (C5) | denied on ingress, ask the human, operator opens the edge, retry to done |
| `multipatch` | the `patch` TRANSACTION (C2) | ONE call, `edits` across two files — coordinated rename, atomic, no half-applied state |
