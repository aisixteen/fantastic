# text_file — text documents, any format, on any file surface

A stateless TEXT-DOCUMENT agent. It owns no disk and no format: every byte
rides a `send` to a `file_bridge` agent's stream verbs, so the running-dir
clamp and the ingress gate apply unchanged, and what a document *contains* is
entirely the caller's business — notes, markdown, JSON, source code. Memory
here is whatever you write.

Name the surface per call with `on: <file_bridge id>`, or set a default
`target_id` on this agent's record. The operator wires BOTH ends: this agent's
`can_call` names the leg, the leg's `callable_by` accepts this agent.

Verbs: `read` (line-numbered, paged, rejects binary) · `write` (atomic:
tmp + rename on the target) · `patch` (SEARCH/REPLACE blocks, all-or-nothing,
fails loud on `no_match`/`ambiguous`) · `append` · and ON DEMAND:
`checkpoint` / `checkpoints` / `restore` / `drop` (pre-images as plain files
under `.ckpt/` on the same surface — visible, caller-retained) plus a
`version` content-hash on every reply and an `if_match` guard on every mutate.

Atomicity, honestly: this agent owns the TRANSACTION protocol (tmp→rename
commit, checkpoint-before-mutate, verify-all-before-any-hunk); the file leg
owns per-op atomicity (one rename). A crash mid-sequence leaves visible
tmp/checkpoint debris — never lost data, never a half-written target.
