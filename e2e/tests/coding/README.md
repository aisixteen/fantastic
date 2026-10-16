# `coding/` — the coder organs, LLM-DRIVEN, end to end

A coding harness is a set of verbs plus a rights table (see `THESIS.md`). Each
test here hands a MODEL one organ and one task, and scores the RESULT — the
file, the store, the repo, the suite — never prose. Each also requires, from
the transcript, that the model used THAT organ (a bypass fails the check).

An organ that IS a process needs ZERO kernel code: an `exec_bridge` leg with an
operator allowlist, driven by `start`/`status` ("a capability is a command").
The byte surface is the `file_bridge` streams; the EDIT organ is the same bridge's `edit` (SEARCH/REPLACE, a transaction over files; gated, no direct disk).

| test | organ | the model must |
|---|---|---|
| `file_edit` | `file_bridge.edit` (C2) | change ONE value in a config with a SEARCH/REPLACE edit — the rest of the file byte-identical |
| `search` | `exec_bridge` + `grep` (C3) | find a token hidden in a tree it was never shown, store the VALUE in memory |
| `git_ops` | `exec_bridge` + `git` (C16) | commit the working-tree change; log carries its message, the tree ends clean |
| `program_tests` | `exec_bridge` + `pytest`, composed with the edit organ (C20+C2) | run the failing suite, fix the bug in the PROGRAM, rerun to green |
| `undo` | `file_bridge.undo` (C12) | edit, then `undo` (one RAM slot per path) — file ends byte-identical |
| `multiline_edit` | `file_bridge.edit` (C2) | ONE edit, two hunks — one replacing a 3-line block — everything else byte-identical, no whole-file rewrite |
| `cancel` | kernel `ps`+`cancel` over an `exec_bridge` job (C1) | find the in-flight job in the task table, kill it — the OS process must actually die |
| `multipatch` | the `edit` TRANSACTION (C2) | ONE call, `edits` across two files — coordinated rename, atomic, no half-applied state |
| `grep_glob` | `exec_bridge` + `grep`/`find` (C3) | the marker hides among cross-type decoys — only a glob-filtered, line-numbered hit scores |
| `repo_map` | organs only, NO map organ (C14) | map a REAL package (aider, local clone) file-by-file — a minted module proves it was built, not recited |
| `compaction` | Context Protocol: truncate + `recall` (codex compact/truncation) | tiny window drops an early fact; the model recalls it from the whole durable transcript |
| `memory_distill` | memory as files on a `file_bridge` + the persist-on-notice howto | a fact distilled to `memory.md` is recovered by a COLD, history-less session |
| `fanout` | batch send + `N of M` reply tags (codex tool_parallelism) | five independent reads in ONE turn's batch, not five turns |
| `peer_review` | two AI agents + exec_bridge + editor (codex auto_review) | a reviewer agent reads the coder's `git diff` and REJECTS a planted flaw |
| `gated_write` | two-sided gate + delegation (codex guardian) | denied on ingress, the writer's work is unblocked by a PEER agent that opens the gate |
| `job_mail` | exec_bridge MAIL over the control fd (codex unified_exec) | the model steers a RUNNING job by mail mid-run; the job writes the mailed token |
| `finding_survival` (E58) | context protocol (folds) | notice a finding, checkpoint it unprompted, and report it after its read is gone |
| `implicit_constraint` (E59) | context protocol | honour constraints given in prose — one positive, one negative with the forbidden string planted as bait |
| `spread_rules` (E60) | context protocol (folds) | keep rules arriving in three files across a 12-filler run — checkpointing continues after cuts start |
| `grep_finds` (E61) | exec_bridge + `grep` | find the one of eight identical fillers that carries a marker — grep, then pull only the hit |
| `two_roots_live` (E63) | two root tasks | handle a second task the human sends mid-run — two chains interleave, two reports, no cross-talk |
| `fold_long` (E64) | folds | keep 8 rules over 4 files binding while folds fire in batches over 24 fillers |
| `fold_descent` (E65) | folds + descent | get an answer that lives inside a folded chain by descending into it |
| `facts_x_chains` (E66) | memory store | answer a question about an early root of four from the store, with four clean reports |
| `fold_paging` (E67) | folds | among many folds from many small tasks, open only the one that matters |
| `superlong_chain` (E68) | folds | run one chain of 40 reads at a 12k window — its past folds in spans, the view holds only folds + tail |
| `flat_roots` (E69) | folds of folds | answer from 24 tiny root tasks once folds of folds appear |
| `recall_early` (E70) | recall + grep | reach the very first instruction and a stored body after a long run |
| `nested_folds` (E71) | folds of folds | summarize every level; grep finds a grandchild |
| `user_fold` (E72) | user fold | keep the rules after the human folds the agent's finished work mid-run through the verb |
| `misleading_fold` (E73) | folds + descent | verify a planted fold whose summary contradicts its members by descending |
| `synthetic_recall` (E74) | recall | boot on a store planted from above, recall its own view, work from it |
| `reboot_continuity` (E75) | memory store + reboot | continue the work after a mid-run reboot — same store, fresh process state |
| `recall_after_fold` (E76) | recall + grep | recall after every fold batch; the marker under a fold comes back |
| `kernel_restart` (E78) | memory store + restart | after work, folds and shutdown, two AI agents in a NEW kernel on the same dir each continue from their own hydrated rows |
| `context_clear` (E80) | `context_clear` | treat a clear as a boundary, not a deletion — chat/clear/chat, chat/fold/clear/chat, and a clear that survives a kernel restart |
| `monitor` (E100) | exec_bridge monitors | run four workers under four parallel monitors, show each FATAL line to the user, stop those workers early, and after about a minute cancel the monitors left and stop everything |
