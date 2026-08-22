# exec_bridge — the process edge of the io family

Sealed by default (`ingress_rule=allow_all_ingress` opens it), interpreter **ALLOWLIST**
(`interpreters=[…]` on the record — no default, no PATH fallback, an unlisted
`interpreter` refuses naming the legal set).

`start {code?, args?, interpreter?, shim?}` spawns in the BACKGROUND → `{job_id,
status:'running'}` at once, streaming `progress`/`job_done`. The record's persisted
`code` is a durable tool — a SCRIPT run top to bottom, revived by `boot` (whose
error PROPAGATES). Any verb this bundle does not own is MAIL for the running
program (`{queued:true}`); jobs are kernel TASKS (`ps`/`cancel` reap the real OS
process).

The bridge publishes a WIRE: the inherited `FANTASTIC_CTRL_FD`, framed
`[4-byte BE length | JSON]`, env `FANTASTIC_SELF_ID` + `FANTASTIC_ARGS`.
`shim:'python'` prepends the wire client (a `kernel` global:
send/emit/reflect/watch/on_message + self_id + args); name no shim and the
program speaks the wire itself. `reflect` advertises the allowlist and shims.

NOT a sandbox: an allowlisted interpreter is a NAMED interpreter with all of that
interpreter's reach. The allowlist buys one property — the OPERATOR chose which
runtime and environment run here, and a program cannot choose a different one.
