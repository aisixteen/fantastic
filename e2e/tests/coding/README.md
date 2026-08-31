# `coding/` — the coder organs, LLM-DRIVEN, end to end

A coding harness is a set of verbs plus a rights table (see `THESIS.md`). Each
test here hands a MODEL one organ and one task, and scores the RESULT — the
file, the store, the repo, the suite — never prose. Each also requires, from
the transcript, that the model used THAT organ (a bypass fails the check).

An organ that IS a process needs ZERO kernel code: an `exec_bridge` leg with an
operator allowlist, driven by `start`/`status` ("a capability is a command").
Only the byte surface (edit) is a `file_bridge` verb.

| test | organ | the model must |
|---|---|---|
| `file_edit` | `file_bridge.patch_text_file` (C2) | change ONE value in a config with a SEARCH/REPLACE patch — the rest of the file byte-identical |
| `search` | `exec_bridge` + `grep` (C3) | find a token hidden in a tree it was never shown, store the VALUE in memory |
| `git_ops` | `exec_bridge` + `git` (C16) | commit the working-tree change; log carries its message, the tree ends clean |
| `program_tests` | `exec_bridge` + `pytest`, composed with the edit organ (C20+C2) | run the failing suite, fix the bug in the PROGRAM, rerun to green |
